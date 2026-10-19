//! AMD's encoder driven directly through the driver's AMF runtime on our D3D11 device.
//!
//! Media Foundation's wrapper around the same encoder costs about twice the CPU (measured 22.7%
//! of one core against 10.4% for direct NVENC while recording a game at 1080p60): every frame
//! crosses its topology, a format converter and several work queues. AMF takes the BGRA
//! texture, converts it on the GPU, and hands back Annex B H.264 (SPS/PPS repeated on every
//! IDR) that goes to the MP4 writer unchanged, as with NVENC.
//!
//! The interfaces are C vtables from the AMF SDK 1.5.3 headers (amf/public/include); only the
//! entries Clipper calls are named, the rest are padding, and every index below matches the
//! header's declaration order.

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{Interface, s, w};

use crate::capture::{Gpu, bgra_texture, copy_picture};
use crate::nvenc::Packet;

type Result<T> = std::result::Result<T, String>;

// AMF_RESULT values (core/Result.h).
const AMF_OK: i32 = 0;
const AMF_EOF: i32 = 23;
const AMF_REPEAT: i32 = 24;
const AMF_INPUT_FULL: i32 = 25;
const AMF_NEED_MORE_INPUT: i32 = 44;

/// The SDK version these bindings were written and checked against (core/Version.h).
const SDK_VERSION: u64 = 1 << 48 | 5 << 32 | 3 << 16;
/// Input textures in rotation, so the encoder can work on a few frames while capture goes on.
/// A texture is reused only once the encoder has let go of its surface.
const INPUTS: usize = 4;
/// How long the encoder may go without producing a packet before it counts as stuck (a frame
/// takes a few ms).
const STUCK: Duration = Duration::from_secs(1);

const AMF_DX11_0: i32 = 110;
const AMF_SURFACE_BGRA: i32 = 3;

const VARIANT_BOOL: i32 = 1;
const VARIANT_INT64: i32 = 2;
const VARIANT_SIZE: i32 = 5;
const VARIANT_RATE: i32 = 7;

// Encoder enums (components/VideoEncoderVCE.h).
const USAGE_LOW_LATENCY: i64 = 2;
const PROFILE_HIGH: i64 = 100;
const RATE_CONTROL_PEAK_CONSTRAINED_VBR: i64 = 2;
const QUALITY_PRESET_SPEED: i64 = 1;
const OUTPUT_DATA_TYPE_IDR: i64 = 0;
const COLOR_PROFILE_709: i64 = 1;
/// AMF_COLOR_PRIMARIES_BT709 and AMF_COLOR_TRANSFER_CHARACTERISTIC_BT709 (ColorSpace.h).
const BT709: i64 = 1;

/// AMFGuid: the same layout as a Windows GUID.
#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

const IID_AMF_BUFFER: Guid = Guid(
    0xb04b7248,
    0xb6f0,
    0x4321,
    [0xb6, 0x91, 0xba, 0xa4, 0x74, 0x0f, 0x9f, 0xcb],
);

/// AMFVariantStruct: a type tag and a 16-byte union, passed to SetProperty by value.
#[repr(C)]
#[derive(Clone, Copy)]
struct Variant {
    kind: i32,
    value: Value,
}

#[repr(C)]
#[derive(Clone, Copy)]
union Value {
    boolean: bool,
    int: i64,
    pair: [u32; 2],
    interface: *mut Object,
    raw: [u64; 2],
}

impl Variant {
    fn int(v: i64) -> Self {
        Self {
            kind: VARIANT_INT64,
            value: Value { int: v },
        }
    }
    fn boolean(v: bool) -> Self {
        let mut value = Value { raw: [0; 2] };
        value.boolean = v;
        Self {
            kind: VARIANT_BOOL,
            value,
        }
    }
    /// AMFSize and AMFRate are both two 32-bit fields.
    fn pair(kind: i32, a: u32, b: u32) -> Self {
        let mut value = Value { raw: [0; 2] };
        value.pair = [a, b];
        Self { kind, value }
    }
}

/// Every AMF interface starts with a vtable pointer.
#[repr(C)]
struct Object {
    vtbl: *const c_void,
}

type Fn0 = usize; // an entry Clipper never calls

/// AMFFactoryVtbl (core/Factory.h). The factory is not reference counted.
#[repr(C)]
struct FactoryVtbl {
    create_context: unsafe extern "system" fn(*mut Object, *mut *mut Object) -> i32, // 0
    create_component:
        unsafe extern "system" fn(*mut Object, *mut Object, *const u16, *mut *mut Object) -> i32, // 1
}

/// AMFContextVtbl (core/Context.h).
#[repr(C)]
struct ContextVtbl {
    _acquire: Fn0,                                                              // 0
    release: unsafe extern "system" fn(*mut Object) -> i32,                     // 1
    _property_storage: [Fn0; 11],                                               // 2..=12
    terminate: unsafe extern "system" fn(*mut Object) -> i32,                   // 13
    _dx9: [Fn0; 4],                                                             // 14..=17
    init_dx11: unsafe extern "system" fn(*mut Object, *mut c_void, i32) -> i32, // 18
    _rest: [Fn0; 30],                                                           // 19..=48
    create_surface_from_dx11_native:
        unsafe extern "system" fn(*mut Object, *mut c_void, *mut *mut Object, *mut c_void) -> i32, // 49
}

/// AMFComponentVtbl (components/Component.h), Windows order of the property-info entries.
#[repr(C)]
struct ComponentVtbl {
    _acquire: Fn0,                                                                    // 0
    release: unsafe extern "system" fn(*mut Object) -> i32,                           // 1
    _query_interface: Fn0,                                                            // 2
    set_property: unsafe extern "system" fn(*mut Object, *const u16, Variant) -> i32, // 3
    get_property: unsafe extern "system" fn(*mut Object, *const u16, *mut Variant) -> i32, // 4
    _more_storage: [Fn0; 12],                                                         // 5..=16
    init: unsafe extern "system" fn(*mut Object, i32, i32, i32) -> i32,               // 17
    _reinit: Fn0,                                                                     // 18
    terminate: unsafe extern "system" fn(*mut Object) -> i32,                         // 19
    drain: unsafe extern "system" fn(*mut Object) -> i32,                             // 20
    _flush: Fn0,                                                                      // 21
    submit_input: unsafe extern "system" fn(*mut Object, *mut Object) -> i32,         // 22
    query_output: unsafe extern "system" fn(*mut Object, *mut *mut Object) -> i32,    // 23
}

/// AMFDataVtbl / AMFBufferVtbl / AMFSurfaceVtbl prefix (core/Data.h, core/Buffer.h).
#[repr(C)]
struct DataVtbl {
    acquire: unsafe extern "system" fn(*mut Object) -> i32, // 0
    release: unsafe extern "system" fn(*mut Object) -> i32, // 1
    query_interface: unsafe extern "system" fn(*mut Object, *const Guid, *mut *mut Object) -> i32, // 2
    _set_property: Fn0, // 3
    get_property: unsafe extern "system" fn(*mut Object, *const u16, *mut Variant) -> i32, // 4
    _storage: [Fn0; 14], // 5..=18
    set_pts: unsafe extern "system" fn(*mut Object, i64), // 19
    get_pts: unsafe extern "system" fn(*mut Object) -> i64, // 20
    _set_duration: Fn0, // 21
    _get_duration: Fn0, // 22
    // AMFBuffer only from here on.
    _set_size: Fn0,                                                    // 23
    get_size: unsafe extern "system" fn(*mut Object) -> usize,         // 24
    get_native: unsafe extern "system" fn(*mut Object) -> *mut c_void, // 25
}

unsafe fn vt<T>(o: *mut Object) -> &'static T {
    unsafe { &*((*o).vtbl as *const T) }
}

fn check(what: &str, result: i32) -> Result<()> {
    if result == AMF_OK {
        Ok(())
    } else {
        Err(format!("AMF {what} failed: {result}"))
    }
}

/// One lock for every call into the AMF runtime. The capture thread submits frames while
/// another thread finishes the previous segment's encoder and a third builds the next one, all
/// on the same D3D11 device; the driver is not safe for that (0.2.3 crashed in SubmitInput with
/// a null pointer inside amfrtdrv64.dll). The lock is reentrant per thread, so an encoder that
/// fails halfway through `new` can be dropped while its builder still holds it.
static AMF: Mutex<()> = Mutex::new(());
thread_local!(static HOLDING: Cell<bool> = const { Cell::new(false) });

struct AmfGuard(Option<MutexGuard<'static, ()>>);

fn amf_lock() -> AmfGuard {
    if HOLDING.get() {
        return AmfGuard(None);
    }
    let guard = AMF.lock().unwrap_or_else(PoisonError::into_inner);
    HOLDING.set(true);
    AmfGuard(Some(guard))
}

impl Drop for AmfGuard {
    fn drop(&mut self) {
        if self.0.is_some() {
            HOLDING.set(false);
        }
    }
}

struct Factory(*mut Object);
// The factory is a process-wide singleton the runtime keeps alive; it is safe to share.
unsafe impl Send for Factory {}
unsafe impl Sync for Factory {}

/// The runtime's factory, loaded once. `None` when no AMD driver with AMF is present.
fn factory() -> Option<*mut Object> {
    static FACTORY: OnceLock<Option<Factory>> = OnceLock::new();
    FACTORY
        .get_or_init(|| unsafe {
            let lib = LoadLibraryW(w!("amfrt64.dll")).ok()?;
            let query: unsafe extern "C" fn(*mut u64) -> i32 =
                std::mem::transmute(GetProcAddress(lib, s!("AMFQueryVersion"))?);
            let init: unsafe extern "C" fn(u64, *mut *mut Object) -> i32 =
                std::mem::transmute(GetProcAddress(lib, s!("AMFInit"))?);
            let mut runtime = 0u64;
            // 1.4 introduced every entry used here. Ask for the SDK version the bindings match,
            // or the runtime's own when it is older.
            if query(&mut runtime) != AMF_OK || runtime < (1 << 48 | 4 << 32) {
                return None;
            }
            let mut f = null_mut();
            (init(runtime.min(SDK_VERSION), &mut f) == AMF_OK && !f.is_null()).then_some(Factory(f))
        })
        .as_ref()
        .map(|f| f.0)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// An input texture and the AMF surface that wraps it, both made once per encoder.
struct Input {
    texture: ID3D11Texture2D,
    surface: *mut Object,
}

pub struct AmfEnc {
    context: *mut Object,
    encoder: *mut Object,
    inputs: Vec<Input>,
    next: usize,
    /// Frames submitted whose packet has not come out yet.
    in_flight: usize,
    /// When the encoder last made progress: a packet came out, or it got work while idle.
    progress: Instant,
    /// The encoder said it has nothing more after a drain.
    eof: bool,
    width: u32,
    height: u32,
}

// AMF objects may be used from any thread; every call goes through `amf_lock`.
unsafe impl Send for AmfEnc {}

impl AmfEnc {
    pub fn new(gpu: &Gpu, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let _amf = amf_lock();
        let factory = factory().ok_or("no AMD encoder runtime")?;
        unsafe {
            let f = vt::<FactoryVtbl>(factory);
            let mut context = null_mut();
            check("context", (f.create_context)(factory, &mut context))?;
            // From here on, Drop releases whatever was created.
            let mut enc = AmfEnc {
                context,
                encoder: null_mut(),
                inputs: Vec::new(),
                next: 0,
                in_flight: 0,
                progress: Instant::now(),
                eof: false,
                width,
                height,
            };
            let c = vt::<ContextVtbl>(context);
            check(
                "DirectX 11 setup",
                (c.init_dx11)(context, gpu.device.as_raw(), AMF_DX11_0),
            )?;
            for _ in 0..INPUTS {
                let texture = bgra_texture(gpu, width, height)?;
                let mut surface = null_mut();
                check(
                    "wrap texture",
                    (c.create_surface_from_dx11_native)(
                        context,
                        texture.as_raw(),
                        &mut surface,
                        null_mut(),
                    ),
                )?;
                enc.inputs.push(Input { texture, surface });
            }
            let id = wide("AMFVideoEncoderVCE_AVC");
            check(
                "H.264 encoder",
                (f.create_component)(factory, context, id.as_ptr(), &mut enc.encoder),
            )?;
            enc.configure(fps, bitrate)?;
            let e = vt::<ComponentVtbl>(enc.encoder);
            check(
                "encoder start",
                (e.init)(enc.encoder, AMF_SURFACE_BGRA, width as i32, height as i32),
            )?;
            Ok(enc)
        }
    }

    fn set(&self, name: &str, value: Variant) -> Result<()> {
        let name16 = wide(name);
        let e = unsafe { vt::<ComponentVtbl>(self.encoder) };
        check(name, unsafe {
            (e.set_property)(self.encoder, name16.as_ptr(), value)
        })
    }

    fn configure(&self, fps: u32, bitrate: u32) -> Result<()> {
        // Usage first: it resets every other property to that usage's defaults.
        self.set("Usage", Variant::int(USAGE_LOW_LATENCY))?;
        self.set("Profile", Variant::int(PROFILE_HIGH))?;
        self.set(
            "FrameSize",
            Variant::pair(VARIANT_SIZE, self.width, self.height),
        )?;
        self.set("FrameRate", Variant::pair(VARIANT_RATE, fps, 1))?;
        self.set(
            "RateControlMethod",
            Variant::int(RATE_CONTROL_PEAK_CONSTRAINED_VBR),
        )?;
        self.set("TargetBitrate", Variant::int(bitrate as i64))?;
        self.set("PeakBitrate", Variant::int(bitrate as i64 / 3 * 4))?;
        // A keyframe every second, no B-frames, and parameter sets on every keyframe: precise
        // clip cuts, no reordering delay, and every segment decodes on its own.
        self.set("IDRPeriod", Variant::int(fps as i64))?;
        self.set("BPicturesPattern", Variant::int(0))?;
        self.set("HeaderInsertionSpacing", Variant::int(fps as i64))?;
        // Tuning that older drivers may not know; the defaults are fine there.
        let _ = self.set("VBVBufferSize", Variant::int(bitrate as i64));
        let _ = self.set("QualityPreset", Variant::int(QUALITY_PRESET_SPEED));
        // The desktop is full-range RGB; the video is HD, so BT.709 limited range, written into
        // the stream so every player shows the same colours.
        let _ = self.set("InputFullRangeColor", Variant::boolean(true));
        let _ = self.set("OutColorProfile", Variant::int(COLOR_PROFILE_709));
        let _ = self.set("FullRangeColor", Variant::boolean(false));
        let _ = self.set("OutColorPrimaries", Variant::int(BT709));
        let _ = self.set("OutColorTransferChar", Variant::int(BT709));
        // QueryOutput answers at once (QueryTimeout 0, the default): letting the driver wait
        // instead held the capture thread past the next frame and lost up to a third of them
        // (measured 41-60 fps against 56-60 with encode()'s 1 ms sleeps), for the same CPU.
        let _ = self.set("QueryTimeout", Variant::int(0));
        Ok(())
    }

    /// SPS and PPS with start codes, for the MP4 sample description.
    pub fn sequence_header(&self) -> Result<Vec<u8>> {
        let _amf = amf_lock();
        unsafe {
            let e = vt::<ComponentVtbl>(self.encoder);
            let mut v = Variant::int(0);
            let name = wide("ExtraData");
            check(
                "sequence header",
                (e.get_property)(self.encoder, name.as_ptr(), &mut v),
            )?;
            let object = v.value.interface;
            if object.is_null() {
                return Err("AMF sequence header is empty".into());
            }
            // The variant holds a reference to the interface; read it, then let it go.
            let bytes = buffer_bytes(object);
            (vt::<DataVtbl>(object).release)(object);
            bytes
        }
    }

    /// Starts encoding `src` and returns the packets of earlier frames that are ready, without
    /// waiting for the GPU: the capture thread never blocks on it. When the encoder still holds
    /// every input texture, the frame is dropped, so a GPU busy with the game records fewer
    /// frames instead of stalling. An encoder that makes no progress for a second is broken:
    /// the error makes the recorder stop using it.
    pub fn encode(&mut self, gpu: &Gpu, src: &ID3D11Texture2D, time: i64) -> Result<Vec<Packet>> {
        let _amf = amf_lock();
        let mut packets = self.collect()?;
        if self.in_flight > 0 && self.progress.elapsed() >= STUCK {
            return Err("AMF encoder produced no frame within 1 s".into());
        }
        // The next input whose surface the encoder no longer holds. A packet coming out does not
        // mean the encoder released the surface; submitting one it still holds breaks the
        // driver. With none free, this frame is dropped.
        let Some(slot) = (0..INPUTS)
            .map(|i| (self.next + i) % INPUTS)
            .find(|&i| self.in_flight < INPUTS && only_ours(self.inputs[i].surface))
        else {
            return Ok(packets);
        };
        let input = &self.inputs[slot];
        copy_picture(gpu, &input.texture, 0, src, self.width, self.height);
        unsafe {
            // Submit the copy now, so the encoder does not wait for the queue to flush.
            gpu.ctx.Flush();
            (vt::<DataVtbl>(input.surface).set_pts)(input.surface, time);
            let e = vt::<ComponentVtbl>(self.encoder);
            match (e.submit_input)(self.encoder, input.surface) {
                AMF_INPUT_FULL => return Ok(packets),
                r => check("submit", r)?,
            }
        }
        if self.in_flight == 0 {
            self.progress = Instant::now();
        }
        self.in_flight += 1;
        self.next = (slot + 1) % INPUTS;
        packets.extend(self.collect()?);
        Ok(packets)
    }

    /// The packets that are ready now.
    pub fn collect(&mut self) -> Result<Vec<Packet>> {
        let _amf = amf_lock();
        let mut packets = Vec::new();
        while let Some(packet) = self.poll()? {
            self.in_flight = self.in_flight.saturating_sub(1);
            self.progress = Instant::now();
            packets.push(packet);
        }
        Ok(packets)
    }

    /// Waits for the packets of every frame still in the encoder, at the end of a segment:
    /// Drain, then outputs until the encoder reports the end. The lock is not held while
    /// waiting, so capture into the next segment's encoder goes on.
    pub fn drain(&mut self) -> Result<Vec<Packet>> {
        let mut packets = Vec::new();
        if self.in_flight == 0 {
            return Ok(packets);
        }
        {
            let _amf = amf_lock();
            let e = unsafe { vt::<ComponentVtbl>(self.encoder) };
            check("drain", unsafe { (e.drain)(self.encoder) })?;
            self.progress = Instant::now();
        }
        loop {
            packets.extend(self.collect()?);
            if self.in_flight == 0 || self.eof {
                return Ok(packets);
            }
            if self.progress.elapsed() >= STUCK {
                return Err("AMF encoder produced no frame within 1 s".into());
            }
            // A frame takes a few milliseconds; Rust's sleep uses a high-resolution timer.
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// One output if the encoder has one ready. A returned object is always used and released,
    /// whatever status came with it.
    fn poll(&mut self) -> Result<Option<Packet>> {
        unsafe {
            let e = vt::<ComponentVtbl>(self.encoder);
            let mut data = null_mut();
            let result = (e.query_output)(self.encoder, &mut data);
            self.eof |= result == AMF_EOF;
            if data.is_null() {
                return match result {
                    AMF_OK | AMF_REPEAT | AMF_NEED_MORE_INPUT | AMF_EOF => Ok(None),
                    r => check("output", r).map(|()| None),
                };
            }
            let d = vt::<DataVtbl>(data);
            let mut kind = Variant::int(-1);
            let keyframe = (d.get_property)(data, w!("OutputDataType").as_ptr(), &mut kind)
                == AMF_OK
                && kind.value.int == OUTPUT_DATA_TYPE_IDR;
            let time = (d.get_pts)(data);
            let bytes = buffer_bytes(data);
            (d.release)(data);
            bytes.map(|data| {
                Some(Packet {
                    data,
                    keyframe,
                    time,
                })
            })
        }
    }
}

/// True when Clipper holds the only reference to `surface`: the encoder is done with it.
fn only_ours(surface: *mut Object) -> bool {
    unsafe {
        let d = vt::<DataVtbl>(surface);
        // Acquire returns the new count: ours plus this temporary one.
        let count = (d.acquire)(surface);
        (d.release)(surface);
        count == 2
    }
}

/// Copies the bytes of an AMFData that is an AMFBuffer.
unsafe fn buffer_bytes(data: *mut Object) -> Result<Vec<u8>> {
    unsafe {
        let mut buffer = null_mut();
        check(
            "buffer",
            (vt::<DataVtbl>(data).query_interface)(data, &IID_AMF_BUFFER, &mut buffer),
        )?;
        let b = vt::<DataVtbl>(buffer);
        let (ptr, size) = ((b.get_native)(buffer), (b.get_size)(buffer));
        let bytes = if ptr.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(ptr as *const u8, size).to_vec()
        };
        (b.release)(buffer);
        Ok(bytes)
    }
}

impl Drop for AmfEnc {
    fn drop(&mut self) {
        let _amf = amf_lock();
        unsafe {
            for input in &self.inputs {
                (vt::<DataVtbl>(input.surface).release)(input.surface);
            }
            if !self.encoder.is_null() {
                let e = vt::<ComponentVtbl>(self.encoder);
                (e.terminate)(self.encoder);
                (e.release)(self.encoder);
            }
            if !self.context.is_null() {
                let c = vt::<ContextVtbl>(self.context);
                (c.terminate)(self.context);
                (c.release)(self.context);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// The vtable entries sit at the indices the AMF 1.5.3 headers declare.
    #[test]
    fn vtable_layouts_match_the_headers() {
        let at = |offset: usize| offset / size_of::<usize>();
        assert_eq!(at(offset_of!(ContextVtbl, terminate)), 13);
        assert_eq!(at(offset_of!(ContextVtbl, init_dx11)), 18);
        assert_eq!(
            at(offset_of!(ContextVtbl, create_surface_from_dx11_native)),
            49
        );
        assert_eq!(at(offset_of!(ComponentVtbl, set_property)), 3);
        assert_eq!(at(offset_of!(ComponentVtbl, init)), 17);
        assert_eq!(at(offset_of!(ComponentVtbl, terminate)), 19);
        assert_eq!(at(offset_of!(ComponentVtbl, drain)), 20);
        assert_eq!(at(offset_of!(ComponentVtbl, submit_input)), 22);
        assert_eq!(at(offset_of!(ComponentVtbl, query_output)), 23);
        assert_eq!(at(offset_of!(DataVtbl, acquire)), 0);
        assert_eq!(at(offset_of!(DataVtbl, release)), 1);
        assert_eq!(at(offset_of!(DataVtbl, set_pts)), 19);
        assert_eq!(at(offset_of!(DataVtbl, get_size)), 24);
        assert_eq!(at(offset_of!(DataVtbl, get_native)), 25);
        // AMFVariantStruct: a 4-byte type tag, then a 16-byte union aligned to 8.
        assert_eq!(size_of::<Variant>(), 24);
        assert_eq!(offset_of!(Variant, value), 8);
        assert_eq!(size_of::<Guid>(), 16);
    }

    /// A thread that holds the AMF lock can take it again (an encoder dropped inside `new`),
    /// and releasing the outer guard frees it for other threads.
    #[test]
    fn amf_lock_is_reentrant_per_thread() {
        let outer = amf_lock();
        let inner = amf_lock();
        drop(inner);
        assert!(
            HOLDING.get(),
            "the inner guard must not release the outer one"
        );
        drop(outer);
        assert!(!HOLDING.get());
        let other = std::thread::spawn(|| drop(amf_lock()));
        other.join().unwrap();
    }

    /// The recorder makes a new encoder for every 10 s segment; 60 of them (10 minutes of
    /// recording) must keep every frame and not leave memory behind. Needs an AMD GPU:
    /// `cargo test --release churn -- --ignored --nocapture`, watching the process's memory.
    #[test]
    #[ignore]
    fn churn() {
        let gpu = Gpu::new("AMD Radeon(TM) Graphics").unwrap();
        let src = bgra_texture(&gpu, 1920, 1080).unwrap();
        for i in 0..60 {
            let mut enc = AmfEnc::new(&gpu, 1920, 1080, 60, 16_000_000).unwrap();
            enc.sequence_header().unwrap();
            let mut packets = Vec::new();
            for f in 0..60 {
                packets.extend(enc.encode(&gpu, &src, f * 166_666).unwrap());
                // Frames arrive at 60 fps, as from capture.
                std::thread::sleep(Duration::from_micros(16_666));
            }
            packets.extend(enc.drain().unwrap());
            // At 60 fps no frame is dropped; each comes out once, in order, a keyframe first.
            assert_eq!(packets.len(), 60);
            assert!(packets[0].keyframe);
            assert!(packets.windows(2).all(|p| p[0].time < p[1].time));
            drop(enc);
            eprintln!("encoder {i}: {} packets", packets.len());
        }
    }

    /// The recorder's threads at once, sped up: capture encodes at 60 fps while another thread
    /// builds the next encoder and a third drains and drops the previous one, every 0.2 s.
    /// 0.2.3 crashed inside the driver after this overlap. Needs an AMD GPU:
    /// `cargo test --release overlap -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn overlapping_encoders_do_not_crash() {
        use std::sync::{Arc, mpsc};
        let gpu = Arc::new(Gpu::new("AMD Radeon(TM) Graphics").unwrap());
        let (spare_tx, spare_rx) = mpsc::sync_channel::<AmfEnc>(1);
        let (done_tx, done_rx) = mpsc::channel::<AmfEnc>();
        let builder = {
            let gpu = gpu.clone();
            std::thread::spawn(move || {
                for _ in 0..300 {
                    let enc = AmfEnc::new(&gpu, 1920, 1080, 60, 16_000_000).unwrap();
                    enc.sequence_header().unwrap();
                    if spare_tx.send(enc).is_err() {
                        return;
                    }
                }
            })
        };
        let finisher = std::thread::spawn(move || {
            let mut n = 0;
            for mut enc in done_rx {
                enc.drain().unwrap();
                n += 1;
            }
            n
        });
        let src = bgra_texture(&gpu, 1920, 1080).unwrap();
        let mut enc = spare_rx.recv().unwrap();
        let mut frame = 0i64;
        for next in spare_rx.iter() {
            for _ in 0..12 {
                enc.encode(&gpu, &src, frame * 166_666).unwrap();
                frame += 1;
                std::thread::sleep(Duration::from_micros(16_666));
            }
            done_tx.send(std::mem::replace(&mut enc, next)).unwrap();
        }
        drop(done_tx);
        builder.join().unwrap();
        eprintln!(
            "{} encoders finished, {frame} frames",
            finisher.join().unwrap()
        );
    }
}
