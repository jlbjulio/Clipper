use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_GPU_PREFERENCE_MINIMUM_POWER,
    IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIFactory6,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA,
    IWICImagingFactory, WICBitmapEncoderNoCache,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIDeviceManager, MFCreateDXGIDeviceManager, MFMediaType_Video,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::BOOL;
use windows::core::{HSTRING, Interface, Result};

/// One D3D11 device shared by capture and encoding, so frames never leave the GPU.
pub struct Gpu {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    pub manager: IMFDXGIDeviceManager,
    winrt: IDirect3DDevice,
    pub name: String,
    /// PCI vendor id: 0x10DE NVIDIA, 0x1002 AMD, 0x8086 Intel.
    pub vendor: u32,
    /// Windows' software rasterizer (no usable GPU): frames have to be encoded on the CPU.
    pub software: bool,
}

// The device is multithread-protected and Media Foundation objects are free-threaded.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

/// Hardware GPUs in Windows' order; the first one drives the main display.
fn hardware_adapters() -> Vec<IDXGIAdapter1> {
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return Vec::new();
    };
    (0..)
        .map_while(|i| unsafe { factory.EnumAdapters1(i) }.ok())
        .filter(|a| {
            unsafe { a.GetDesc1() }
                .is_ok_and(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0)
        })
        .collect()
}

fn adapter_name(a: &IDXGIAdapter1) -> String {
    let desc = unsafe { a.GetDesc1() }.unwrap_or_default();
    let len = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..len])
}

/// `CLIPPER_SOFTWARE=1` forces software capture and encoding: a workaround for a broken
/// graphics driver, and how the no-GPU path is tested on machines that have one.
pub fn software_only() -> bool {
    std::env::var_os("CLIPPER_SOFTWARE").is_some_and(|v| v == "1")
}

/// Whether any hardware H.264 encoder is installed. Without one, Media Foundation falls back to
/// Windows' software encoder, and recording is scaled down to keep the CPU free.
pub fn hardware_encoder() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| {
        if software_only() {
            return false;
        }
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        unsafe {
            let found = MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                None,
                Some(&output),
                &mut list,
                &mut count,
            )
            .is_ok();
            for i in 0..count as usize {
                drop(std::ptr::read(list.add(i)));
            }
            if !list.is_null() {
                CoTaskMemFree(Some(list as _));
            }
            found && count > 0
        }
    })
}

/// Names of the GPUs that can capture and encode, for the settings window.
pub fn adapters() -> Vec<String> {
    hardware_adapters().iter().map(adapter_name).collect()
}

/// The most power-efficient GPU: the integrated one on a PC with two. Recording with it lets a
/// dedicated GPU sleep: an encode session holds a laptop's NVIDIA card at full clocks even on an
/// idle desktop (4 W to 14 W measured on an RTX 3050), while a game running there keeps it awake
/// anyway. Windows copies the other card's monitors over at the full frame rate.
pub fn power_saving_adapter() -> Option<String> {
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory6>() }.ok()?;
    let adapter: IDXGIAdapter1 =
        unsafe { factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_MINIMUM_POWER) }.ok()?;
    let desc = unsafe { adapter.GetDesc1() }.ok()?;
    (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0).then(|| adapter_name(&adapter))
}

impl Gpu {
    /// Uses the GPU named `preferred`, or Windows' default when it is empty or gone.
    pub fn new(preferred: &str) -> Result<Self> {
        let adapter = hardware_adapters()
            .into_iter()
            .find(|a| !preferred.is_empty() && adapter_name(a) == preferred)
            .and_then(|a| a.cast::<IDXGIAdapter>().ok());
        let create = |adapter: Option<&IDXGIAdapter>,
                      kind,
                      flags|
         -> Result<(ID3D11Device, ID3D11DeviceContext)> {
            let (mut device, mut ctx) = (None, None);
            unsafe {
                D3D11CreateDevice(
                    adapter,
                    kind,
                    Default::default(),
                    flags,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut ctx),
                )?;
            }
            Ok((device.unwrap(), ctx.unwrap()))
        };
        let kind = if adapter.is_some() {
            D3D_DRIVER_TYPE_UNKNOWN
        } else {
            D3D_DRIVER_TYPE_HARDWARE
        };
        // Video support lets Media Foundation convert and encode straight from GPU textures.
        let hardware = if software_only() {
            Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                0x887A0004_u32 as i32,
            )))
        } else {
            create(
                adapter.as_ref(),
                kind,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            )
        };
        let software = hardware.is_err();
        let (device, ctx) = match hardware {
            Ok(pair) => pair,
            Err(e) => {
                // Virtual machines, remote sessions and broken drivers: Windows' software
                // rasterizer still captures the screen, just with more CPU.
                crate::log(&format!(
                    "no hardware GPU ({e}); capturing with the software renderer"
                ));
                create(None, D3D_DRIVER_TYPE_WARP, D3D11_CREATE_DEVICE_BGRA_SUPPORT)?
            }
        };
        unsafe {
            let _ = device
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true);
            let (mut token, mut manager) = (0, None);
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(&device, token)?;
            let dxgi = device.cast::<IDXGIDevice>()?;
            let adapter = dxgi.GetAdapter()?.cast::<IDXGIAdapter1>()?;
            let name = adapter_name(&adapter);
            let vendor = adapter.GetDesc1().map(|d| d.VendorId).unwrap_or_default();
            let winrt = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
            Ok(Self {
                device,
                ctx,
                manager,
                winrt,
                name,
                vendor,
                software,
            })
        }
    }
}

/// A live capture of one monitor. Dropping it stops the capture.
pub struct Session {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    stale: Arc<AtomicBool>,
}

impl Session {
    /// True once this session no longer matches the monitor: Windows ended it (unplugged,
    /// sleep) or the resolution changed, as when a game switches the display mode.
    pub fn is_stale(&self) -> bool {
        self.stale.load(Ordering::Acquire)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn item(monitor: HMONITOR) -> Result<GraphicsCaptureItem> {
    unsafe {
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?
            .CreateForMonitor(monitor)
    }
}

pub struct MonitorInfo {
    /// HMONITOR as an address, so it can cross threads.
    pub handle: usize,
    /// Stable name such as \\.\DISPLAY1, used in the config file.
    pub device: String,
    pub primary: bool,
    pub size: (u32, u32),
}

/// Every connected monitor, primary first.
pub fn monitors() -> Vec<MonitorInfo> {
    unsafe extern "system" fn collect(m: HMONITOR, _: HDC, _: *mut RECT, list: LPARAM) -> BOOL {
        unsafe { (*(list.0 as *mut Vec<HMONITOR>)).push(m) };
        true.into()
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut handles as *mut _ as isize),
        );
    }
    let mut list: Vec<MonitorInfo> = handles
        .into_iter()
        .filter_map(|m| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            unsafe { GetMonitorInfoW(m, &mut info.monitorInfo as *mut MONITORINFO) }
                .ok()
                .ok()?;
            let len = info
                .szDevice
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(info.szDevice.len());
            Some(MonitorInfo {
                handle: m.0 as usize,
                device: String::from_utf16_lossy(&info.szDevice[..len]),
                primary: info.monitorInfo.dwFlags & 1 != 0,
                size: monitor_size(m).unwrap_or_default(),
            })
        })
        .collect();
    list.sort_by_key(|m| (!m.primary, m.device.clone()));
    list
}

/// The monitor's current resolution in physical pixels, from the window manager. Cheap enough
/// to poll; a game in exclusive full screen that switches the display mode changes it without
/// telling a running capture session, which then only sees a stale, dark desktop.
pub fn monitor_pixels(monitor: HMONITOR) -> Option<(u32, u32)> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(monitor, &mut info) }.ok().ok()?;
    let r = info.rcMonitor;
    Some(((r.right - r.left) as u32, (r.bottom - r.top) as u32))
}

/// The monitor's resolution in physical pixels, as capture will deliver it.
pub fn monitor_size(monitor: HMONITOR) -> Result<(u32, u32)> {
    let size = item(monitor)?.Size()?;
    Ok((size.Width as u32, size.Height as u32))
}

/// Starts capturing `monitor`; `on_frame` gets each new frame and its QPC timestamp (100 ns units).
/// The current image arrives right away; after that, frames only come when the screen changes.
pub fn start(
    gpu: &Gpu,
    monitor: HMONITOR,
    cursor: bool,
    fps: Option<u32>,
    on_frame: impl Fn(&ID3D11Texture2D, i64) + Send + 'static,
) -> Result<Session> {
    let item = item(monitor)?;
    let size = item.Size()?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &gpu.winrt,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;
    let session = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsBorderRequired(false);
    let _ = session.SetIsCursorCaptureEnabled(cursor);
    if let Some(fps) = fps {
        // A 144 Hz screen would otherwise wake us 144 times a second for a 60 fps recording.
        // Three quarters of a frame keeps 60 fps content that lands on uneven refresh ticks.
        // Windows 11 24H2+; older versions ignore it.
        let interval = Duration::from_micros(750_000 / fps as u64);
        let _ = session.SetMinUpdateInterval(interval.into());
    }
    let stale = Arc::new(AtomicBool::new(false));
    let flag = stale.clone();
    item.Closed(&TypedEventHandler::new(move |_, _| {
        flag.store(true, Ordering::Release);
        Ok(())
    }))?;
    let flag = stale.clone();
    pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, _>::new(
        move |pool, _| {
            let frame = pool.ok()?.TryGetNextFrame()?;
            if frame.ContentSize()? != size {
                // The display mode changed: these frames no longer fit the pool or the
                // encoder. Drop them until the session is rebuilt at the new size.
                flag.store(true, Ordering::Release);
                return Ok(());
            }
            let ts = frame.SystemRelativeTime()?.Duration;
            let tex: ID3D11Texture2D = unsafe {
                frame
                    .Surface()?
                    .cast::<IDirect3DDxgiInterfaceAccess>()?
                    .GetInterface()?
            };
            on_frame(&tex, ts);
            Ok(())
        },
    ))?;
    session.StartCapture()?;
    Ok(Session {
        pool,
        session,
        stale,
    })
}

/// Grabs the current image of `monitor` and writes it as PNG. WGC delivers the first frame
/// immediately, even when nothing on screen is changing.
pub fn screenshot(
    gpu: &Gpu,
    monitor: HMONITOR,
    path: &Path,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::sync_channel::<ID3D11Texture2D>(1);
    let tx = std::sync::Mutex::new(Some(tx));
    let _session = start(gpu, monitor, false, None, move |tex, _| {
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(tex.clone());
        }
    })?;
    let tex = rx.recv_timeout(Duration::from_secs(2))?;
    let (pixels, w, h) = read_back(gpu, &tex)?;
    write_png(path, &pixels, w, h)?;
    Ok(())
}

fn read_back(gpu: &Gpu, tex: &ID3D11Texture2D) -> Result<(Vec<u8>, u32, u32)> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;
    let mut staging = None;
    unsafe {
        gpu.device
            .CreateTexture2D(&desc, None, Some(&mut staging))?
    };
    let staging = staging.unwrap();
    let (w, h) = (desc.Width, desc.Height);
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    unsafe {
        gpu.ctx.CopyResource(&staging, tex);
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.ctx
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
        for y in 0..h as usize {
            let src = std::slice::from_raw_parts(
                (map.pData as *const u8).add(y * map.RowPitch as usize),
                w as usize * 4,
            );
            pixels[y * w as usize * 4..][..w as usize * 4].copy_from_slice(src);
        }
        gpu.ctx.Unmap(&staging, 0);
    }
    // The desktop's alpha channel is undefined; make every pixel opaque.
    pixels
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .for_each(|p| p[3] = 255);
    Ok((pixels, w, h))
}

fn write_png(path: &Path, pixels: &[u8], w: u32, h: u32) -> Result<()> {
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = wic.CreateStream()?;
        stream.InitializeFromFilename(&HSTRING::from(path.as_os_str()), GENERIC_WRITE.0)?;
        let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.unwrap();
        frame.Initialize(None)?;
        frame.SetSize(w, h)?;
        let mut format = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut format)?;
        frame.WritePixels(h, w * 4, pixels)?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}
