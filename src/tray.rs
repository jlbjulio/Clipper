//! The notification-area icon, its menu, and the "saved" notifications. A hidden window owns
//! the icon and the global shortcuts; the settings window finds it by class name.

use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, GetCursorPos,
    GetSystemMetrics, HICON, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR, LoadIconW, LoadImageW,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, PostMessageW, RegisterClassW, RegisterWindowMessageW,
    SM_CXSMICON, SetForegroundWindow, SetWindowTextW, TPM_BOTTOMALIGN, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, WM_APP, WNDCLASSW,
};

pub const CLASS: &str = "ClipperTray";
/// Icon callbacks, re-posted from the window procedure so the main loop sees them.
pub const WM_TRAY: u32 = WM_APP + 1;
/// Sent by the settings window after it saved the config file.
pub const WM_RELOAD: u32 = WM_APP + 2;
/// Sent by the settings window while it listens for a new shortcut, so ours don't fire.
pub const WM_KEYS_OFF: u32 = WM_APP + 3;
pub const WM_KEYS_ON: u32 = WM_APP + 4;
/// A clip or screenshot finished; `lParam` owns a boxed `Saved`.
pub const WM_SAVED: u32 = WM_APP + 5;
/// Explorer restarted and the icon must be added again.
pub const WM_RECREATE: u32 = WM_APP + 6;

const CALLBACK: u32 = WM_APP + 100;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn copy(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

static mut TASKBAR_CREATED: u32 = 0;

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if msg == CALLBACK {
            // NOTIFYICON_VERSION_4: the low word of lParam is the event, wParam the anchor point.
            PostMessageW(hwnd, WM_TRAY, (l as u32 & 0xFFFF) as usize, 0);
            return 0;
        }
        if msg == TASKBAR_CREATED && msg != 0 {
            PostMessageW(hwnd, WM_RECREATE, 0, 0);
            return 0;
        }
        DefWindowProcW(hwnd, msg, w, l)
    }
}

pub struct Tray {
    pub hwnd: HWND,
    data: NOTIFYICONDATAW,
    icons: [HICON; 2],
}

fn icon(id: u16, size_metric: i32) -> HICON {
    unsafe {
        let size = GetSystemMetrics(size_metric);
        let h = LoadImageW(
            GetModuleHandleW(null_mut()),
            std::ptr::without_provenance::<u16>(id as usize),
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR,
        );
        if h.is_null() {
            LoadIconW(null_mut(), IDI_APPLICATION)
        } else {
            h
        }
    }
}

/// Lets the tray menu follow Windows' dark mode. uxtheme exposes this only by ordinal (135 =
/// SetPreferredAppMode, 136 = FlushMenuThemes, Windows 10 1903+); when missing, menus stay light.
fn allow_dark_menus() {
    unsafe {
        let ux = LoadLibraryW(wide("uxtheme.dll").as_ptr());
        if ux.is_null() {
            return;
        }
        if let Some(f) = GetProcAddress(ux, 135usize as *const u8) {
            let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 =
                std::mem::transmute(f);
            set_preferred_app_mode(1); // AllowDark: follow the system setting
        }
        if let Some(f) = GetProcAddress(ux, 136usize as *const u8) {
            let flush_menu_themes: unsafe extern "system" fn() = std::mem::transmute(f);
            flush_menu_themes();
        }
    }
}

impl Tray {
    pub fn new() -> Tray {
        allow_dark_menus();
        unsafe {
            let class = wide(CLASS);
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: GetModuleHandleW(null_mut()),
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            TASKBAR_CREATED = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
            // A real (never shown) top-level window: message-only windows miss the TaskbarCreated
            // broadcast and cannot be found by the settings window.
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("").as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                wc.hInstance,
                null_mut(),
            );
            if hwnd.is_null() {
                crate::log(&format!(
                    "tray window failed: error {}",
                    windows_sys::Win32::Foundation::GetLastError()
                ));
            }
            let mut data: NOTIFYICONDATAW = std::mem::zeroed();
            data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            data.hWnd = hwnd;
            data.uID = 1;
            data.uCallbackMessage = CALLBACK;
            let mut tray = Tray {
                hwnd,
                data,
                icons: [icon(1, SM_CXSMICON), icon(2, SM_CXSMICON)],
            };
            tray.add();
            tray
        }
    }

    /// Adds the icon (again, after an Explorer restart).
    pub fn add(&mut self) {
        unsafe {
            self.data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
            if self.data.hIcon.is_null() {
                self.data.hIcon = self.icons[0];
            }
            Shell_NotifyIconW(NIM_ADD, &self.data);
            self.data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &self.data);
        }
    }

    /// Shows the recording state in the icon and its tooltip.
    pub fn set_state(&mut self, recording: bool, tip: &str) {
        self.data.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        self.data.hIcon = self.icons[if recording { 0 } else { 1 }];
        copy(&mut self.data.szTip, tip);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &self.data) };
    }

    /// Publishes a status line for the settings window, which reads it as the window title.
    pub fn set_status(&self, status: &str) {
        unsafe { SetWindowTextW(self.hwnd, wide(status).as_ptr()) };
    }

    /// Shows the menu at the cursor and returns the chosen item id (0 when dismissed).
    /// Items are (id, label, enabled); id 0 draws a separator.
    pub fn menu(&self, items: &[(u32, String, bool)]) -> u32 {
        unsafe {
            let menu = CreatePopupMenu();
            for (id, label, enabled) in items {
                if *id == 0 {
                    AppendMenuW(menu, MF_SEPARATOR, 0, null_mut());
                } else {
                    let flags = MF_STRING | if *enabled { 0 } else { MF_GRAYED };
                    AppendMenuW(menu, flags, *id as usize, wide(label).as_ptr());
                }
            }
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            // Required so the menu closes when the user clicks elsewhere.
            SetForegroundWindow(self.hwnd);
            let chosen = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                0,
                self.hwnd,
                null_mut(),
            );
            DestroyMenu(menu);
            chosen as u32
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe { Shell_NotifyIconW(NIM_DELETE, &self.data) };
    }
}
