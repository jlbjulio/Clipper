//! The fallback for shortcuts another app already reserved.
//!
//! Clipper registers its shortcuts with `RegisterHotKey` first: that works everywhere,
//! including while a game running as administrator has focus. A combination another app
//! registered first (Alt+F1 is a common one) is handed to this low-level keyboard hook instead,
//! which sees keys before any app does. The hook runs on its own thread so a busy main thread
//! never delays typing, and reads its bindings from atomics so it never waits on a lock.
//! Windows does not run a normal app's hook for keys sent to an elevated window, so a reserved
//! shortcut cannot work there; the settings window lets the user pick a free one.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, PeekMessageW,
    PostMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WM_APP, WM_HOTKEY, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::config::{Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};

/// Each binding packed as `mods << 32 | vk`; 0 means unassigned.
static BINDINGS: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static ENABLED: AtomicBool = AtomicBool::new(true);
static TARGET: AtomicUsize = AtomicUsize::new(0);
/// The hook thread's id, for telling it to install or remove the hook.
static THREAD: AtomicUsize = AtomicUsize::new(0);
/// The shortcut being held (packed like a binding), so its auto-repeat and release are
/// swallowed too.
static HELD: AtomicU64 = AtomicU64::new(0);

/// An unassigned virtual key. Sent while Alt or Win is down so that releasing the modifier does
/// not open the foreground app's menu bar (or the Start menu) after we swallowed the real key.
const MASK_KEY: u16 = 0xE8;

fn pressed(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

fn modifiers() -> u32 {
    let mut m = 0;
    if pressed(VK_MENU) {
        m |= MOD_ALT;
    }
    if pressed(VK_CONTROL) {
        m |= MOD_CONTROL;
    }
    if pressed(VK_SHIFT) {
        m |= MOD_SHIFT;
    }
    if pressed(VK_LWIN) || pressed(VK_RWIN) {
        m |= MOD_WIN;
    }
    m
}

fn mask_modifier_release() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: MASK_KEY,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [key(0), key(KEYEVENTF_KEYUP)];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };
}

unsafe extern "system" fn hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let vk = (*(l as *const KBDLLHOOKSTRUCT)).vkCode;
            let down = matches!(w as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let up = matches!(w as u32, WM_KEYUP | WM_SYSKEYUP);
            let held = HELD.load(Ordering::Relaxed);
            if held != 0 && held as u32 == vk {
                // Repeats and the release of a shortcut we acted on stay with us. A press
                // without the shortcut's modifiers is a new keystroke: the release we were
                // waiting for went elsewhere (a UAC prompt, the secure desktop).
                if up {
                    HELD.store(0, Ordering::Relaxed);
                    return 1;
                }
                if (held >> 32) as u32 == modifiers() {
                    return 1;
                }
                HELD.store(0, Ordering::Relaxed);
            }
            if down && vk != MASK_KEY as u32 && ENABLED.load(Ordering::Relaxed) {
                let mods = modifiers();
                let combo = (mods as u64) << 32 | vk as u64;
                if let Some(id) = BINDINGS
                    .iter()
                    .position(|b| b.load(Ordering::Relaxed) == combo)
                {
                    HELD.store(combo, Ordering::Relaxed);
                    if mods & (MOD_ALT | MOD_WIN) != 0 {
                        mask_modifier_release();
                    }
                    PostMessageW(TARGET.load(Ordering::Relaxed) as _, WM_HOTKEY, id + 1, 0);
                    return 1;
                }
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, w, l)
    }
}

/// True when some shortcut is handed to the hook and the hook is not paused.
fn needed() -> bool {
    ENABLED.load(Ordering::Relaxed) && BINDINGS.iter().any(|b| b.load(Ordering::Relaxed) != 0)
}

/// Starts the hook thread. Matching shortcuts arrive at `target` as `WM_HOTKEY` with
/// `wParam` = 1, 2 or 3 (the position in `set`'s list, plus one). The hook is installed only
/// while a shortcut needs it: every key pressed anywhere, in a game too, goes through an
/// installed low-level hook first.
pub fn start(target: usize) {
    TARGET.store(target, Ordering::Relaxed);
    thread::spawn(|| unsafe {
        let mut msg: MSG = std::mem::zeroed();
        // Create the message queue before publishing the id, so no request is lost.
        PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
        THREAD.store(GetCurrentThreadId() as usize, Ordering::Release);
        let mut installed: HHOOK = std::ptr::null_mut();
        loop {
            match (needed(), installed.is_null()) {
                (true, true) => {
                    installed =
                        SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), std::ptr::null_mut(), 0);
                    if installed.is_null() {
                        crate::log("keyboard hook unavailable; reserved shortcuts will not work");
                    }
                }
                (false, false) => {
                    UnhookWindowsHookEx(installed);
                    installed = std::ptr::null_mut();
                }
                _ => {}
            }
            // A low-level hook only runs while its thread pumps messages; WM_APP asks for a
            // fresh look at `needed()`.
            if GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) <= 0 {
                return;
            }
        }
    });
}

/// Sets the shortcuts the hook handles (`Hotkey::NONE` for the ones `RegisterHotKey` took);
/// `enabled = false` lets every key through while the settings window records a shortcut.
pub fn set(keys: [Hotkey; 3], enabled: bool) {
    for (slot, key) in BINDINGS.iter().zip(keys) {
        let packed = if key.is_set() {
            (key.mods as u64) << 32 | key.vk as u64
        } else {
            0
        };
        slot.store(packed, Ordering::Relaxed);
    }
    ENABLED.store(enabled, Ordering::Relaxed);
    // Before the thread has started, it reads the bindings itself.
    let thread = THREAD.load(Ordering::Acquire);
    if thread != 0 {
        unsafe { PostThreadMessageW(thread as u32, WM_APP, 0, 0) };
    }
}
