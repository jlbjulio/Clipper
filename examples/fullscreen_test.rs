//! A stand-in for a game in exclusive full screen, to test capture of that case.
//!
//! It takes the primary monitor in DXGI exclusive mode with the legacy blit swap chain (no
//! desktop composition) and switches the display to the given resolution, as older games do.
//! The picture cycles colours and flashes white for 0.1 s every second.
//!
//!     cargo run --release --example fullscreen_test -- [seconds] [width] [height]

use std::ptr::null_mut;
use std::time::Instant;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11CreateDeviceAndSwapChain, ID3D11Device,
    ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_MODE_DESC, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH, DXGI_SWAP_EFFECT_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGISwapChain,
};
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
    RegisterClassW, SW_SHOW, ShowWindow, TranslateMessage, WNDCLASSW, WS_POPUP,
};

unsafe extern "system" fn wndproc(
    h: windows_sys::Win32::Foundation::HWND,
    m: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn main() -> windows::core::Result<()> {
    let args: Vec<u32> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let seconds = *args.first().unwrap_or(&20) as f32;
    let (width, height) = (*args.get(1).unwrap_or(&1280), *args.get(2).unwrap_or(&720));
    unsafe {
        let class = wide("ClipperFullscreenTest");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: GetModuleHandleW(null_mut()),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("Clipper full-screen test").as_ptr(),
            WS_POPUP,
            0,
            0,
            width as i32,
            height as i32,
            null_mut(),
            null_mut(),
            wc.hInstance,
            null_mut(),
        );
        ShowWindow(hwnd, SW_SHOW);

        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: width,
                Height: height,
                RefreshRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: HWND(hwnd),
            Windowed: false.into(),
            SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
            Flags: DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH.0 as u32,
        };
        let (mut swap, mut device, mut ctx): (
            Option<IDXGISwapChain>,
            Option<ID3D11Device>,
            Option<ID3D11DeviceContext>,
        ) = (None, None, None);
        D3D11CreateDeviceAndSwapChain(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&desc),
            Some(&mut swap),
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
        let (swap, device, ctx) = (swap.unwrap(), device.unwrap(), ctx.unwrap());
        let back: ID3D11Texture2D = swap.GetBuffer(0)?;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
        let rtv = rtv.unwrap();

        let start = Instant::now();
        let mut msg: MSG = std::mem::zeroed();
        while start.elapsed().as_secs_f32() < seconds {
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let t = start.elapsed().as_secs_f32();
            let color = if t.fract() < 0.1 {
                [1.0, 1.0, 1.0, 1.0]
            } else {
                let h = t * 0.5;
                [0.3 + 0.2 * h.sin(), 0.2 + 0.2 * (h * 1.3).cos(), 0.35, 1.0]
            };
            ctx.ClearRenderTargetView(&rtv, &color);
            swap.Present(1, Default::default()).ok()?;
        }
        // Leave exclusive mode before the swap chain goes away, as DXGI requires.
        swap.SetFullscreenState(false, None)?;
    }
    Ok(())
}
