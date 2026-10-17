use std::path::Path;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEXTURE2D_DESC,
    ID3D11Texture2D,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::core::{HSTRING, Interface, Result};

use crate::audio::RATE;
use crate::capture::Gpu;

const AUDIO_BYTES_PER_SEC: u32 = 20_000; // 160 kbps AAC

/// One MP4 segment written by the hardware H.264 encoder. Frames are copied GPU-to-GPU into
/// pooled textures; the colour conversion and encoding never touch system memory.
pub struct Encoder {
    writer: IMFSinkWriter,
    pool: IMFVideoSampleAllocatorEx,
    video: u32,
    audio: u32,
    width: u32,
    height: u32,
    frame: i64,
}

// Media Foundation sink writers are free-threaded; callers serialize access through a mutex.
unsafe impl Send for Encoder {}

fn attrs(n: u32) -> Result<IMFAttributes> {
    let mut a = None;
    unsafe { MFCreateAttributes(&mut a, n)? };
    Ok(a.unwrap())
}

fn video_type(
    subtype: &windows::core::GUID,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | height as u64)?;
        t.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1 << 32 | 1)?;
        Ok(t)
    }
}

fn audio_type(subtype: &windows::core::GUID, bytes_per_sec: u32) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_sec)?;
        Ok(t)
    }
}

/// Encoder settings handed over while the sink writer configures the encoder; vendor encoders
/// ignore rate control changed afterwards. Variable bitrate keeps static scenes tiny, and a
/// keyframe every second keeps clip cuts precise.
fn encoding_params(fps: u32, bitrate: u32) -> Result<IMFAttributes> {
    let a = attrs(4)?;
    unsafe {
        a.SetUINT32(
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32,
        )?;
        a.SetUINT32(&CODECAPI_AVEncCommonMeanBitRate, bitrate)?;
        a.SetUINT32(&CODECAPI_AVEncCommonMaxBitRate, bitrate / 3 * 4)?;
        a.SetUINT32(&CODECAPI_AVEncMPVGOPSize, fps)?;
    }
    Ok(a)
}

impl Encoder {
    pub fn new(
        gpu: &Gpu,
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self> {
        unsafe {
            let a = attrs(4)?;
            a.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &gpu.manager)?;
            a.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            // Audio keeps flowing while a static screen produces no frames; never block on it.
            a.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            a.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &a)?;

            let out = video_type(&MFVideoFormat_H264, width, height, fps)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            let video = writer.AddStream(&out)?;
            let input = video_type(&MFVideoFormat_ARGB32, width, height, fps)?;
            writer.SetInputMediaType(video, &input, &encoding_params(fps, bitrate)?)?;

            let audio = writer.AddStream(&audio_type(&MFAudioFormat_AAC, AUDIO_BYTES_PER_SEC)?)?;
            let pcm = audio_type(&MFAudioFormat_PCM, RATE * 4)?;
            pcm.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            writer.SetInputMediaType(audio, &pcm, None)?;
            writer.BeginWriting()?;

            let mut pool: Option<IMFVideoSampleAllocatorEx> = None;
            MFCreateVideoSampleAllocatorEx(
                &IMFVideoSampleAllocatorEx::IID,
                &mut pool as *mut _ as _,
            )?;
            let pool = pool.unwrap();
            pool.SetDirectXManager(&gpu.manager)?;
            let pa = attrs(1)?;
            pa.SetUINT32(
                &MF_SA_D3D11_BINDFLAGS,
                (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            )?;
            pool.InitializeSampleAllocatorEx(2, 6, &pa, &input)?;

            Ok(Self {
                writer,
                pool,
                video,
                audio,
                width,
                height,
                frame: 10_000_000 / fps as i64,
            })
        }
    }

    /// Queues one frame at `time` (100 ns units from the segment start). Drops the frame when
    /// the encoder still holds every pooled texture.
    pub fn video(&self, gpu: &Gpu, src: &ID3D11Texture2D, time: i64) -> Result<()> {
        unsafe {
            let sample = match self.pool.AllocateSample() {
                Ok(s) => s,
                Err(e) if e.code() == MF_E_SAMPLEALLOCATOR_EMPTY => return Ok(()),
                Err(e) => return Err(e),
            };
            let buffer = sample.GetBufferByIndex(0)?;
            let dxgi: IMFDXGIBuffer = buffer.cast()?;
            let mut dst: Option<ID3D11Texture2D> = None;
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut dst as *mut _ as _)?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut desc);
            let area = D3D11_BOX {
                right: desc.Width.min(self.width),
                bottom: desc.Height.min(self.height),
                back: 1,
                ..Default::default()
            };
            gpu.ctx.CopySubresourceRegion(
                &dst.unwrap(),
                dxgi.GetSubresourceIndex()?,
                0,
                0,
                0,
                src,
                0,
                Some(&area),
            );
            buffer.SetCurrentLength(buffer.GetMaxLength()?)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(self.frame)?;
            self.writer.WriteSample(self.video, &sample)
        }
    }

    /// Queues 16-bit stereo PCM starting at `time` (100 ns units from the segment start).
    pub fn audio(&self, pcm: &[u8], time: i64) -> Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(pcm.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), ptr, pcm.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(pcm.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(pcm.len() as i64 / 4 * 10_000_000 / RATE as i64)?;
            self.writer.WriteSample(self.audio, &sample)
        }
    }

    /// Tells the muxer no video exists up to `time`, so it stops holding audio back while the
    /// screen is static and produces no frames.
    pub fn video_gap(&self, time: i64) -> Result<()> {
        unsafe { self.writer.SendStreamTick(self.video, time) }
    }

    /// Flushes the encoder and writes the MP4 index.
    pub fn finish(self) -> Result<()> {
        unsafe { self.writer.Finalize() }
    }
}
