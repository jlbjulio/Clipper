//! NVIDIA's encoder driven directly through the driver's NVENC API on our D3D11 device.
//!
//! Media Foundation's NVIDIA encoder wrapper costs about 130 MB of system memory per session;
//! talking to NVENC directly keeps its buffers in video memory. Frames are copied GPU-to-GPU
//! into a registered texture and encoded synchronously; the H.264 output (Annex B, SPS/PPS
//! repeated on every IDR) goes to the MP4 writer unchanged.

use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ptr::null_mut;
use std::sync::OnceLock;

use moq_nvenc::sys::nvEncodeAPI::*;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{Interface, s, w};

use crate::capture::Gpu;

type Result<T> = std::result::Result<T, String>;

/// A zero-filled NVENC parameter struct. The API expects zeroes in every field it is not told
/// about, but some of these structs hold Rust enums with no zero value, so they must never
/// exist as a Rust value while zeroed: fields are written through the pointer, and only fields
/// the driver has filled in are read back.
struct Zeroed<T>(Box<MaybeUninit<T>>);

impl<T> Zeroed<T> {
    fn new() -> Self {
        Self(Box::new(MaybeUninit::zeroed()))
    }

    fn ptr(&mut self) -> *mut T {
        self.0.as_mut_ptr()
    }
}

struct Api(NV_ENCODE_API_FUNCTION_LIST);
// The function table is immutable after loading.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// The driver's function table, loaded once. `None` when no NVIDIA driver with NVENC is present.
fn api() -> Option<&'static NV_ENCODE_API_FUNCTION_LIST> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let lib = LoadLibraryW(w!("nvEncodeAPI64.dll")).ok()?;
        let max: unsafe extern "C" fn(*mut u32) -> NVENCSTATUS = std::mem::transmute(
            GetProcAddress(lib, s!("NvEncodeAPIGetMaxSupportedVersion"))?,
        );
        let create: unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS =
            std::mem::transmute(GetProcAddress(lib, s!("NvEncodeAPICreateInstance"))?);
        let mut version = 0;
        let wanted = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
        if max(&mut version) != NVENCSTATUS::NV_ENC_SUCCESS || version < wanted {
            return None;
        }
        // The function list holds only integers and optional function pointers: zero is valid.
        let mut list: NV_ENCODE_API_FUNCTION_LIST = std::mem::zeroed();
        list.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        (create(&mut list) == NVENCSTATUS::NV_ENC_SUCCESS).then_some(Api(list))
    })
    .as_ref()
    .map(|a| &a.0)
}

fn check(what: &str, status: NVENCSTATUS) -> Result<()> {
    if status == NVENCSTATUS::NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(format!("NVENC {what} failed: {status:?}"))
    }
}

macro_rules! call {
    ($api:expr, $f:ident) => {
        $api.$f
            .ok_or(concat!("NVENC driver lacks ", stringify!($f)))?
    };
}

pub struct NvEnc {
    api: &'static NV_ENCODE_API_FUNCTION_LIST,
    session: *mut c_void,
    input: ID3D11Texture2D,
    registered: NV_ENC_REGISTERED_PTR,
    bitstream: NV_ENC_OUTPUT_PTR,
    width: u32,
    height: u32,
    frame: u32,
}

// An NVENC session may be used from any thread as long as calls are not concurrent; the
// recorder serializes them behind its mutex.
unsafe impl Send for NvEnc {}

/// One encoded frame in Annex B form, with its time in 100 ns units from the segment start.
pub struct Packet {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub time: i64,
}

const TUNING: NV_ENC_TUNING_INFO = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_LOW_LATENCY;

impl NvEnc {
    pub fn new(gpu: &Gpu, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let api = api().ok_or("no NVIDIA encoder driver")?;
        let input = crate::capture::bgra_texture(gpu, width, height)?;
        let mut session = null_mut();
        unsafe {
            let mut open = Zeroed::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>::new();
            let p = open.ptr();
            (*p).version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
            (*p).deviceType = NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX;
            (*p).device = gpu.device.as_raw();
            (*p).apiVersion = NVENCAPI_VERSION;
            let opened = check(
                "open session",
                call!(api, nvEncOpenEncodeSessionEx)(p, &mut session),
            );
            if let Err(e) = opened {
                // The API requires destroying even a session that failed to open (for
                // example when another app holds all of GeForce's encoder sessions).
                if !session.is_null()
                    && let Some(destroy) = api.nvEncDestroyEncoder
                {
                    destroy(session);
                }
                return Err(e);
            }
        }
        // From here on, Drop closes the session whatever fails next.
        let mut enc = NvEnc {
            api,
            session,
            input,
            registered: null_mut(),
            bitstream: null_mut(),
            width,
            height,
            frame: 0,
        };
        enc.configure(fps, bitrate)?;
        enc.buffers()?;
        Ok(enc)
    }

    fn configure(&mut self, fps: u32, bitrate: u32) -> Result<()> {
        let api = self.api;
        unsafe {
            let mut preset = Zeroed::<NV_ENC_PRESET_CONFIG>::new();
            let p = preset.ptr();
            (*p).version = NV_ENC_PRESET_CONFIG_VER;
            (*p).presetCfg.version = NV_ENC_CONFIG_VER;
            let get = call!(api, nvEncGetEncodePresetConfigEx);
            check(
                "preset",
                get(
                    self.session,
                    NV_ENC_CODEC_H264_GUID,
                    NV_ENC_PRESET_P4_GUID,
                    TUNING,
                    p,
                ),
            )?;
            // The driver filled the whole config with valid values; it is safe to read now.
            let mut cfg: NV_ENC_CONFIG = std::ptr::read(&raw const (*p).presetCfg);
            cfg.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
            // A keyframe every second and no B-frames: precise clip cuts, no reordering delay.
            cfg.gopLength = fps;
            cfg.frameIntervalP = 1;
            cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR;
            cfg.rcParams.averageBitRate = bitrate;
            cfg.rcParams.maxBitRate = bitrate / 3 * 4;
            cfg.rcParams.vbvBufferSize = bitrate;
            let h264 = &mut cfg.encodeCodecConfig.h264Config;
            h264.idrPeriod = fps;
            // Every segment and every clip cut starts with the parameter sets it needs.
            h264.set_repeatSPSPPS(1);

            let mut init = Zeroed::<NV_ENC_INITIALIZE_PARAMS>::new();
            let i = init.ptr();
            (*i).version = NV_ENC_INITIALIZE_PARAMS_VER;
            (*i).encodeGUID = NV_ENC_CODEC_H264_GUID;
            (*i).presetGUID = NV_ENC_PRESET_P4_GUID;
            (*i).encodeWidth = self.width;
            (*i).encodeHeight = self.height;
            (*i).darWidth = self.width;
            (*i).darHeight = self.height;
            (*i).maxEncodeWidth = self.width;
            (*i).maxEncodeHeight = self.height;
            (*i).frameRateNum = fps;
            (*i).frameRateDen = 1;
            (*i).enablePTD = 1;
            (*i).tuningInfo = TUNING;
            (*i).encodeConfig = &mut cfg;
            check(
                "initialize",
                call!(api, nvEncInitializeEncoder)(self.session, i),
            )
        }
    }

    fn buffers(&mut self) -> Result<()> {
        let api = self.api;
        unsafe {
            let mut reg = Zeroed::<NV_ENC_REGISTER_RESOURCE>::new();
            let r = reg.ptr();
            (*r).version = NV_ENC_REGISTER_RESOURCE_VER;
            (*r).resourceType = NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
            (*r).width = self.width;
            (*r).height = self.height;
            (*r).resourceToRegister = self.input.as_raw();
            (*r).bufferFormat = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
            (*r).bufferUsage = NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;
            check(
                "register texture",
                call!(api, nvEncRegisterResource)(self.session, r),
            )?;
            self.registered = (*r).registeredResource;

            let mut out = Zeroed::<NV_ENC_CREATE_BITSTREAM_BUFFER>::new();
            let o = out.ptr();
            (*o).version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            check(
                "bitstream buffer",
                call!(api, nvEncCreateBitstreamBuffer)(self.session, o),
            )?;
            self.bitstream = (*o).bitstreamBuffer;
            Ok(())
        }
    }

    /// SPS and PPS with start codes, for the MP4 sample description.
    pub fn sequence_header(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 512];
        let mut size = 0u32;
        unsafe {
            let mut payload = Zeroed::<NV_ENC_SEQUENCE_PARAM_PAYLOAD>::new();
            let p = payload.ptr();
            (*p).version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
            (*p).inBufferSize = buf.len() as u32;
            (*p).spsppsBuffer = buf.as_mut_ptr().cast();
            (*p).outSPSPPSPayloadSize = &mut size;
            check(
                "sequence header",
                call!(self.api, nvEncGetSequenceParams)(self.session, p),
            )?;
        }
        buf.truncate(size as usize);
        Ok(buf)
    }

    /// Encodes `src` and returns the frame, waiting for the GPU (a few milliseconds at 1080p).
    pub fn encode(&mut self, gpu: &Gpu, src: &ID3D11Texture2D, time: i64) -> Result<Packet> {
        let api = self.api;
        crate::capture::copy_picture(gpu, &self.input, 0, src, self.width, self.height);
        unsafe {
            // Submit the copy now; otherwise NVENC waits for the queue to flush on its own.
            gpu.ctx.Flush();

            let mut map = Zeroed::<NV_ENC_MAP_INPUT_RESOURCE>::new();
            let m = map.ptr();
            (*m).version = NV_ENC_MAP_INPUT_RESOURCE_VER;
            (*m).registeredResource = self.registered;
            check(
                "map input",
                call!(api, nvEncMapInputResource)(self.session, m),
            )?;
            let mapped = (*m).mappedResource;

            let mut pic = Zeroed::<NV_ENC_PIC_PARAMS>::new();
            let p = pic.ptr();
            (*p).version = NV_ENC_PIC_PARAMS_VER;
            (*p).inputWidth = self.width;
            (*p).inputHeight = self.height;
            (*p).frameIdx = self.frame;
            (*p).inputTimeStamp = time as u64;
            (*p).inputBuffer = mapped;
            (*p).outputBitstream = self.bitstream;
            (*p).bufferFmt = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
            (*p).pictureStruct = NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
            self.frame = self.frame.wrapping_add(1);
            let result = check("encode", call!(api, nvEncEncodePicture)(self.session, p))
                .and_then(|()| self.read_packet(time));
            call!(api, nvEncUnmapInputResource)(self.session, mapped);
            result
        }
    }

    fn read_packet(&mut self, time: i64) -> Result<Packet> {
        let api = self.api;
        unsafe {
            let mut lock = Zeroed::<NV_ENC_LOCK_BITSTREAM>::new();
            let l = lock.ptr();
            (*l).version = NV_ENC_LOCK_BITSTREAM_VER;
            (*l).outputBitstream = self.bitstream;
            check(
                "lock bitstream",
                call!(api, nvEncLockBitstream)(self.session, l),
            )?;
            let data = std::slice::from_raw_parts(
                (*l).bitstreamBufferPtr as *const u8,
                (*l).bitstreamSizeInBytes as usize,
            )
            .to_vec();
            let keyframe = (*l).pictureType == NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
            call!(api, nvEncUnlockBitstream)(self.session, self.bitstream);
            Ok(Packet {
                data,
                keyframe,
                time,
            })
        }
    }
}

impl Drop for NvEnc {
    fn drop(&mut self) {
        let api = self.api;
        unsafe {
            if !self.bitstream.is_null()
                && let Some(f) = api.nvEncDestroyBitstreamBuffer
            {
                f(self.session, self.bitstream);
            }
            if !self.registered.is_null()
                && let Some(f) = api.nvEncUnregisterResource
            {
                f(self.session, self.registered);
            }
            if let Some(f) = api.nvEncDestroyEncoder {
                f(self.session);
            }
        }
    }
}
