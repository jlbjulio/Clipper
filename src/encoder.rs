use std::path::Path;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::core::{HSTRING, Interface, Result};

use crate::amf::AmfEnc;
use crate::audio::RATE;
use crate::capture::Gpu;
use crate::log;
use crate::nvenc::{NvEnc, Packet};

const AUDIO_BYTES_PER_SEC: u32 = 20_000; // 160 kbps AAC
const NVIDIA: u32 = 0x10DE;
const AMD: u32 = 0x1002;

/// A GPU vendor's encoder driven through its own driver API.
enum Direct {
    Nvenc(NvEnc),
    Amf(AmfEnc),
}

impl Direct {
    fn encode(
        &mut self,
        gpu: &Gpu,
        src: &ID3D11Texture2D,
        time: i64,
    ) -> std::result::Result<Packet, String> {
        match self {
            Direct::Nvenc(e) => e.encode(gpu, src, time),
            Direct::Amf(e) => e.encode(gpu, src, time),
        }
    }
}

/// When a direct encoder last failed mid-recording. For a minute after that, new segments use
/// the Media Foundation encoder; then the direct one gets another chance, since most failures
/// (a display mode change, a driver reset) pass.
static DIRECT_FAILED: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
const DIRECT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// Who turns frames into H.264.
enum Video {
    /// The Media Foundation encoder inside the sink writer, fed from a pool of GPU textures.
    /// Used for Intel, and for NVIDIA or AMD when their own API is unavailable.
    Writer(IMFVideoSampleAllocatorEx),
    /// NVENC or AMF driven directly; the sink writer only stores the packets they produce.
    /// About half the CPU of the Media Foundation path for the same encoder.
    Direct(Direct),
    /// A direct encoder that failed: the rest of this segment has no new video rather than
    /// stalling every frame (and the audio behind the same lock) on a stuck driver.
    Retired,
    /// No usable GPU: frames are read back to system memory for a CPU-side encoder.
    Cpu(ID3D11Texture2D),
}

/// One MP4 segment: hardware H.264 video plus AAC audio. Frames are copied GPU-to-GPU and the
/// colour conversion and encoding never touch system memory.
pub struct Encoder {
    writer: IMFSinkWriter,
    encoder: Video,
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
/// keyframe every second keeps clip cuts precise. The speed preset and low-latency mode keep
/// the GPU's video engine light while it records nonstop: about 15% instead of 40% at 1080p60
/// on an AMD Radeon 680M, mostly from low latency (that encoder already defaulted to speed).
fn encoding_params(fps: u32, bitrate: u32) -> Result<IMFAttributes> {
    let a = attrs(6)?;
    unsafe {
        a.SetUINT32(
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32,
        )?;
        a.SetUINT32(&CODECAPI_AVEncCommonMeanBitRate, bitrate)?;
        a.SetUINT32(&CODECAPI_AVEncCommonMaxBitRate, bitrate / 3 * 4)?;
        a.SetUINT32(&CODECAPI_AVEncMPVGOPSize, fps)?;
        a.SetUINT32(&CODECAPI_AVEncCommonQualityVsSpeed, 0)?;
        a.SetUINT32(&CODECAPI_AVLowLatencyMode, 1)?;
    }
    Ok(a)
}

/// A sample backed by a copy of `bytes` in system memory.
fn memory_sample(bytes: &[u8], time: i64, duration: i64) -> Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(duration)?;
        Ok(sample)
    }
}

/// NVENC for NVIDIA cards or AMF for AMD ones, with the SPS/PPS the MP4 header needs. `None`
/// means "use the Media Foundation encoder", which also covers any failure of either.
fn direct(gpu: &Gpu, width: u32, height: u32, fps: u32, bitrate: u32) -> Option<(Direct, Vec<u8>)> {
    if DIRECT_FAILED
        .lock()
        .unwrap()
        .is_some_and(|at| at.elapsed() < DIRECT_BACKOFF)
    {
        return None;
    }
    let started = match gpu.vendor {
        NVIDIA => NvEnc::new(gpu, width, height, fps, bitrate)
            .and_then(|enc| enc.sequence_header().map(|h| (Direct::Nvenc(enc), h))),
        AMD => AmfEnc::new(gpu, width, height, fps, bitrate)
            .and_then(|enc| enc.sequence_header().map(|h| (Direct::Amf(enc), h))),
        _ => return None,
    };
    match started {
        Ok(ready) => Some(ready),
        Err(e) => {
            // A new encoder starts every 10 s; one line per run is enough.
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| log(&format!("{e}; using the Media Foundation encoder")));
            None
        }
    }
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
            if !gpu.software {
                a.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &gpu.manager)?;
            }
            a.SetUINT32(
                &MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
                (!gpu.software && crate::capture::hardware_encoder()) as u32,
            )?;
            // Audio keeps flowing while a static screen produces no frames; never block on it.
            a.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            a.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &a)?;

            let out = video_type(&MFVideoFormat_H264, width, height, fps)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            let (video, encoder) = match direct(gpu, width, height, fps, bitrate) {
                Some((enc, header)) => {
                    // Same type in and out: the writer stores the packets as they are.
                    out.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &header)?;
                    let video = writer.AddStream(&out)?;
                    writer.SetInputMediaType(video, &out, None)?;
                    (video, Video::Direct(enc))
                }
                None if gpu.software => {
                    let video = writer.AddStream(&out)?;
                    let input = video_type(&MFVideoFormat_ARGB32, width, height, fps)?;
                    // Positive stride: rows top to bottom, as the readback produces them.
                    input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4)?;
                    writer.SetInputMediaType(video, &input, &encoding_params(fps, bitrate)?)?;
                    (video, Video::Cpu(staging(gpu, width, height)?))
                }
                None => {
                    let video = writer.AddStream(&out)?;
                    let input = video_type(&MFVideoFormat_ARGB32, width, height, fps)?;
                    writer.SetInputMediaType(video, &input, &encoding_params(fps, bitrate)?)?;
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
                    (video, Video::Writer(pool))
                }
            };

            let audio = writer.AddStream(&audio_type(&MFAudioFormat_AAC, AUDIO_BYTES_PER_SEC)?)?;
            let pcm = audio_type(&MFAudioFormat_PCM, RATE * 4)?;
            pcm.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            writer.SetInputMediaType(audio, &pcm, None)?;
            writer.BeginWriting()?;

            Ok(Self {
                writer,
                encoder,
                video,
                audio,
                width,
                height,
                frame: 10_000_000 / fps as i64,
            })
        }
    }

    /// Queues one frame at `time` (100 ns units from the segment start). With the Media
    /// Foundation encoder, drops the frame when it still holds every pooled texture.
    pub fn video(&mut self, gpu: &Gpu, src: &ID3D11Texture2D, time: i64) -> Result<()> {
        let pool = match &mut self.encoder {
            Video::Direct(enc) => {
                let packet = match enc.encode(gpu, src, time) {
                    Ok(p) => p,
                    Err(e) => {
                        log(&format!(
                            "{e}; using the Media Foundation encoder for the next minute"
                        ));
                        *DIRECT_FAILED.lock().unwrap() = Some(std::time::Instant::now());
                        self.encoder = Video::Retired;
                        return Err(windows::core::Error::new(E_FAIL, e));
                    }
                };
                let sample = memory_sample(&packet.data, packet.time, self.frame)?;
                unsafe {
                    sample.SetUINT32(&MFSampleExtension_CleanPoint, packet.keyframe as u32)?;
                    return self.writer.WriteSample(self.video, &sample);
                }
            }
            Video::Retired => return Ok(()),
            Video::Cpu(staging) => {
                let pixels = read_back(gpu, staging, src, self.width, self.height)?;
                let sample = memory_sample(&pixels, time, self.frame)?;
                return unsafe { self.writer.WriteSample(self.video, &sample) };
            }
            Video::Writer(pool) => pool,
        };
        unsafe {
            let sample = match pool.AllocateSample() {
                Ok(s) => s,
                Err(e) if e.code() == MF_E_SAMPLEALLOCATOR_EMPTY => return Ok(()),
                Err(e) => return Err(e),
            };
            let buffer = sample.GetBufferByIndex(0)?;
            let dxgi: IMFDXGIBuffer = buffer.cast()?;
            let mut dst: Option<ID3D11Texture2D> = None;
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut dst as *mut _ as _)?;
            crate::capture::copy_picture(
                gpu,
                &dst.unwrap(),
                dxgi.GetSubresourceIndex()?,
                src,
                self.width,
                self.height,
            );
            buffer.SetCurrentLength(buffer.GetMaxLength()?)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(self.frame)?;
            self.writer.WriteSample(self.video, &sample)
        }
    }

    /// Queues 16-bit stereo PCM starting at `time` (100 ns units from the segment start).
    pub fn audio(&self, pcm: &[u8], time: i64) -> Result<()> {
        let sample = memory_sample(pcm, time, pcm.len() as i64 / 4 * 10_000_000 / RATE as i64)?;
        unsafe { self.writer.WriteSample(self.audio, &sample) }
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

fn staging(gpu: &Gpu, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    Ok(tex.unwrap())
}

/// Copies src into a tightly packed, top-down BGRA buffer of the encoder's size.
fn read_back(
    gpu: &Gpu,
    staging: &ID3D11Texture2D,
    src: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        src.GetDesc(&mut desc);
        let (w, h) = (desc.Width.min(width), desc.Height.min(height));
        crate::capture::copy_picture(gpu, staging, 0, src, width, height);
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
        for y in 0..h as usize {
            let row = std::slice::from_raw_parts(
                (map.pData as *const u8).add(y * map.RowPitch as usize),
                w as usize * 4,
            );
            pixels[y * width as usize * 4..][..w as usize * 4].copy_from_slice(row);
        }
        gpu.ctx.Unmap(staging, 0);
    }
    Ok(pixels)
}

const E_FAIL: windows::core::HRESULT = windows::core::HRESULT(0x8000_4005_u32 as i32);
