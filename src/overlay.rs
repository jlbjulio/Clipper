//! The "saved" confirmation drawn on top of games.
//!
//! Windows holds back its own notifications while a full-screen game runs, so Clipper draws a
//! small card itself: topmost, click-through (the mouse and the game never notice it), never
//! activated (the game keeps focus), and gone after a couple of seconds. A game in true
//! exclusive full screen owns the display and no window can appear over it, this one included.

use std::ptr::null_mut;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CLEARTYPE_QUALITY, CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, FW_NORMAL, FW_SEMIBOLD, GetDC, GetMonitorInfoW,
    HBITMAP, HDC, HFONT, MONITORINFO, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT, TextOutW,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DI_NORMAL, DefWindowProcW, DestroyIcon, DrawIconEx, GetSystemMetrics,
    HWND_TOPMOST, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, RegisterClassW, SM_CXICON, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, ShowWindow, ULW_ALPHA,
    UpdateLayeredWindow, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::tray::wide;

const FADE_IN: Duration = Duration::from_millis(120);
const VISIBLE: Duration = Duration::from_millis(2400);
const FADE_OUT: Duration = Duration::from_millis(300);

/// Card colors: the app icon's charcoal, a one-step lighter edge, and text at full contrast.
const BACKGROUND: [u8; 3] = [30, 32, 36];
const EDGE: [u8; 3] = [62, 64, 70];
const TITLE: u32 = 0x00F3F2F1; // COLORREF is 0x00BBGGRR
const DETAIL: u32 = 0x00BDB8B3;

pub struct Overlay {
    hwnd: HWND,
    dc: HDC,
    bitmap: HBITMAP,
    pos: POINT,
    size: SIZE,
    shown: Option<Instant>,
    /// The opacity last drawn, so the fully visible stretch is not redrawn 30 times a second.
    alpha: u8,
}

impl Overlay {
    pub fn new() -> Overlay {
        unsafe {
            let class = wide("ClipperOverlay");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: GetModuleHandleW(null_mut()),
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                class.as_ptr(),
                wide("").as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                wc.hInstance,
                null_mut(),
            );
            Overlay {
                hwnd,
                dc: null_mut(),
                bitmap: null_mut(),
                pos: POINT { x: 0, y: 0 },
                size: SIZE { cx: 0, cy: 0 },
                shown: None,
                alpha: 0,
            }
        }
    }

    /// Shows the card in the top-right corner of `monitor`'s work area.
    pub fn show(&mut self, monitor: usize, title: &str, detail: &str) {
        unsafe {
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..std::mem::zeroed()
            };
            if GetMonitorInfoW(monitor as _, &mut info) == 0 {
                return;
            }
            let (mut dpi, mut unused) = (96u32, 96u32);
            GetDpiForMonitor(monitor as _, MDT_EFFECTIVE_DPI, &mut dpi, &mut unused);
            let s = |v: f32| (v * dpi as f32 / 96.0).round() as i32;
            self.release();
            self.size = SIZE {
                cx: s(340.0),
                cy: s(64.0),
            };
            let work: RECT = info.rcWork;
            self.pos = POINT {
                x: work.right - self.size.cx - s(24.0),
                y: work.top + s(24.0),
            };
            self.paint(s(10.0) as f32, &s, title, detail);
            self.shown = Some(Instant::now());
            ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            // Stay above windows that went topmost after us, without taking focus.
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            self.alpha = 0;
            self.present(0);
        }
    }

    /// Advances the fade. Returns false once the card is gone, so the caller can stop its timer.
    pub fn tick(&mut self) -> bool {
        let Some(shown) = self.shown else {
            return false;
        };
        let t = shown.elapsed();
        let alpha = if t < FADE_IN {
            t.as_secs_f32() / FADE_IN.as_secs_f32()
        } else if t < VISIBLE {
            1.0
        } else if t < VISIBLE + FADE_OUT {
            1.0 - (t - VISIBLE).as_secs_f32() / FADE_OUT.as_secs_f32()
        } else {
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            self.shown = None;
            self.release();
            return false;
        };
        let alpha = (alpha.clamp(0.0, 1.0) * 255.0) as u8;
        if alpha != self.alpha {
            self.alpha = alpha;
            self.present(alpha);
        }
        true
    }

    fn present(&self, alpha: u8) {
        unsafe {
            let screen = GetDC(null_mut());
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: alpha,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let origin = POINT { x: 0, y: 0 };
            UpdateLayeredWindow(
                self.hwnd, screen, &self.pos, &self.size, self.dc, &origin, 0, &blend, ULW_ALPHA,
            );
            ReleaseDC(null_mut(), screen);
        }
    }

    /// Draws the card into a premultiplied-alpha bitmap.
    fn paint(&mut self, radius: f32, s: &dyn Fn(f32) -> i32, title: &str, detail: &str) {
        let (w, h) = (self.size.cx, self.size.cy);
        unsafe {
            let screen = GetDC(null_mut());
            self.dc = CreateCompatibleDC(screen);
            ReleaseDC(null_mut(), screen);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let mut bits = null_mut();
            self.bitmap =
                CreateDIBSection(self.dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if self.bitmap.is_null() {
                return;
            }
            SelectObject(self.dc, self.bitmap);
            let pixels = std::slice::from_raw_parts_mut(bits as *mut [u8; 4], (w * h) as usize);

            // Opaque card with a one-pixel edge; the rounded corners are cut in the last pass.
            for y in 0..h {
                for x in 0..w {
                    let d =
                        edge_distance(x as f32 + 0.5, y as f32 + 0.5, w as f32, h as f32, radius);
                    let c = if d < 1.0 { EDGE } else { BACKGROUND };
                    pixels[(y * w + x) as usize] = [c[2], c[1], c[0], 255];
                }
            }

            let icon_size = s(32.0);
            let icon = LoadImageW(
                GetModuleHandleW(null_mut()),
                // MAKEINTRESOURCE(1): the app icon in the resource file.
                std::ptr::without_provenance::<u16>(1),
                IMAGE_ICON,
                GetSystemMetrics(SM_CXICON).max(icon_size),
                0,
                LR_DEFAULTCOLOR,
            );
            if !icon.is_null() {
                DrawIconEx(
                    self.dc,
                    s(16.0),
                    (h - icon_size) / 2,
                    icon as _,
                    icon_size,
                    icon_size,
                    0,
                    null_mut(),
                    DI_NORMAL,
                );
                DestroyIcon(icon as _);
            }

            SetBkMode(self.dc, TRANSPARENT as i32);
            let text_x = s(16.0) + icon_size + s(14.0);
            let title_font = font(s(15.0), FW_SEMIBOLD as i32);
            let detail_font = font(s(13.0), FW_NORMAL as i32);
            let old = SelectObject(self.dc, title_font);
            SetTextColor(self.dc, TITLE);
            let t = wide(title);
            TextOutW(self.dc, text_x, s(13.0), t.as_ptr(), t.len() as i32 - 1);
            SelectObject(self.dc, detail_font);
            SetTextColor(self.dc, DETAIL);
            let d = wide(detail);
            TextOutW(self.dc, text_x, s(35.0), d.as_ptr(), d.len() as i32 - 1);
            SelectObject(self.dc, old);
            DeleteObject(title_font);
            DeleteObject(detail_font);

            // GDI text and icons leave alpha undefined: set coverage from the rounded shape and
            // premultiply, as UpdateLayeredWindow expects.
            for y in 0..h {
                for x in 0..w {
                    let d =
                        edge_distance(x as f32 + 0.5, y as f32 + 0.5, w as f32, h as f32, radius);
                    let cover = d.clamp(0.0, 1.0);
                    let p = &mut pixels[(y * w + x) as usize];
                    for c in &mut p[..3] {
                        *c = (*c as f32 * cover) as u8;
                    }
                    p[3] = (cover * 255.0) as u8;
                }
            }
        }
    }

    fn release(&mut self) {
        unsafe {
            if !self.dc.is_null() {
                DeleteDC(self.dc);
                self.dc = null_mut();
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
                self.bitmap = null_mut();
            }
        }
    }
}

/// Distance inside a rounded rectangle's border (negative outside), for coverage and the edge.
fn edge_distance(x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let dx = (r - x).max(x - (w - r)).max(0.0);
    let dy = (r - y).max(y - (h - r)).max(0.0);
    if dx > 0.0 && dy > 0.0 {
        r - (dx * dx + dy * dy).sqrt()
    } else {
        x.min(y).min(w - x).min(h - y)
    }
}

fn font(height: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            wide("Segoe UI Variable Text").as_ptr(),
        )
    }
}
