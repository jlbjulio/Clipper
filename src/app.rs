//! The tray process: records in the background and reacts to shortcuts, the tray menu and the
//! settings window.

use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::Duration;
use std::{env, fs, thread};

use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, POINT};
use windows_sys::Win32::Graphics::Gdi::{
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint, MonitorFromWindow,
};
use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetClassNameW, GetCursorPos, GetForegroundWindow, GetMessageW, KillTimer,
    MB_ICONHAND, MB_OK, MSG, SW_SHOWNORMAL, SetTimer, TranslateMessage, WM_CONTEXTMENU, WM_HOTKEY,
    WM_TIMER, WM_USER,
};

use crate::config::{Config, Hotkey};
use crate::recorder::{Outcome, Params, Recorder};
use crate::text::t;
use crate::tray::{self, Tray, wide};
use crate::{audio, capture, log, now_ticks, stamp};

const HK_CLIP: i32 = 1;
const HK_SHOT: i32 = 2;
const HK_PAUSE: i32 = 3;

const NIN_SELECT: usize = WM_USER as usize;
const NIN_KEYSELECT: usize = WM_USER as usize + 1;
const NIN_BALLOONUSERCLICK: usize = WM_USER as usize + 5;

/// Timer id that re-enables shortcuts if the settings window disappears mid-capture.
const KEYS_TIMEOUT: usize = 2;

const MENU_CLIP: u32 = 1;
const MENU_SHOT: u32 = 2;
const MENU_PAUSE: u32 = 3;
const MENU_CLIPS: u32 = 4;
const MENU_SHOTS: u32 = 5;
const MENU_SETTINGS: u32 = 6;
const MENU_QUIT: u32 = 7;

enum Saved {
    Clip(Outcome),
    Shot(Result<PathBuf, String>),
}

fn post_saved(hwnd: usize, saved: Saved) {
    let ptr = Box::into_raw(Box::new(saved)) as isize;
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(hwnd as _, tray::WM_SAVED, 0, ptr)
    };
}

fn shell_open(file: &str, args: Option<&str>) {
    let args = args.map(wide);
    unsafe {
        ShellExecuteW(
            null_mut(),
            wide("open").as_ptr(),
            wide(file).as_ptr(),
            args.as_ref().map_or(null_mut(), |a| a.as_ptr()),
            null_mut(),
            SW_SHOWNORMAL,
        )
    };
}

fn open_folder(dir: &Path) {
    let _ = fs::create_dir_all(dir);
    shell_open(&dir.display().to_string(), None);
}

/// Opens Explorer with the file selected.
fn reveal(file: &Path) {
    shell_open(
        "explorer.exe",
        Some(&format!("/select,\"{}\"", file.display())),
    );
}

pub fn open_settings() {
    if let Ok(exe) = env::current_exe() {
        let _ = std::process::Command::new(exe).arg("--settings").spawn();
    }
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

fn set_autostart(on: bool) {
    unsafe {
        if on {
            let Ok(exe) = env::current_exe() else { return };
            let value = wide(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                wide(RUN_KEY).as_ptr(),
                wide("Clipper").as_ptr(),
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            );
        } else {
            RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                wide(RUN_KEY).as_ptr(),
                wide("Clipper").as_ptr(),
            );
        }
    }
}

fn beep(ok: bool) {
    unsafe { MessageBeep(if ok { MB_OK } else { MB_ICONHAND }) };
}

fn params(c: &Config) -> Params {
    Params {
        clip_secs: c.clip_secs,
        fps: c.fps,
        quality: c.quality,
    }
}

/// The monitor to record: the one holding the focused window, or the configured one.
fn target_monitor(cfg: &Config, fallback: usize) -> usize {
    if !cfg.follow_focus {
        return fallback;
    }
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_null() {
            return fallback;
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(fg, class.as_mut_ptr(), class.len() as i32) as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        // The desktop and taskbar span every monitor; the pointer tells which one is in use.
        if matches!(
            class.as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        ) {
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            return MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY) as usize;
        }
        MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST) as usize
    }
}

fn fallback_monitor(cfg: &Config) -> usize {
    let list = capture::monitors();
    list.iter()
        .find(|m| m.device == cfg.monitor)
        .or_else(|| list.iter().find(|m| m.primary))
        .map_or_else(
            || unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) as usize },
            |m| m.handle,
        )
}

struct App {
    cfg: Config,
    tray: Tray,
    gpu: Arc<capture::Gpu>,
    rec: Arc<Recorder>,
    mixer: audio::Mixer,
    session: Option<capture::Session>,
    monitor: usize,
    fallback: usize,
    candidate: (usize, u8),
    paused: bool,
    keys_on: bool,
    taken: Vec<&'static str>,
    last_saved: Option<PathBuf>,
}

impl App {
    /// Uses the GPU the monitor is plugged into (or the one chosen in settings), so frames never
    /// have to cross between cards.
    fn pick_gpu(&mut self, hmon: HMONITOR) {
        let wanted = if self.cfg.gpu.is_empty() {
            capture::adapter_of(hmon).unwrap_or_default()
        } else {
            self.cfg.gpu.clone()
        };
        if wanted.is_empty() || wanted == self.gpu.name {
            return;
        }
        match capture::Gpu::new(&wanted) {
            Ok(g) => {
                log(&format!("using {}", g.name));
                self.gpu = Arc::new(g);
                self.rec.set_gpu(self.gpu.clone());
            }
            Err(e) => log(&format!("cannot use {wanted}: {e}")),
        }
    }

    fn begin(&mut self) {
        drop(self.session.take());
        let hmon = HMONITOR(self.monitor as _);
        self.pick_gpu(hmon);
        // The encoder has to exist before the session starts: on a static screen the session's
        // first frame may be the only one for a long time.
        let started = capture::monitor_size(hmon).and_then(|size| {
            self.rec.set_size(size);
            let sink = self.rec.clone();
            capture::start(&self.gpu, hmon, self.cfg.cursor, move |tex, ts| {
                sink.on_frame(tex, ts)
            })
        });
        match started {
            Ok(s) => self.session = Some(s),
            Err(e) => log(&format!("capture failed to start: {e}")),
        }
        self.publish();
    }

    fn register_keys(&mut self) {
        let keys = [
            (HK_CLIP, self.cfg.clip_key, "clip"),
            (HK_SHOT, self.cfg.shot_key, "shot"),
            (HK_PAUSE, self.cfg.pause_key, "pause"),
        ];
        self.taken.clear();
        for (id, key, name) in keys {
            unsafe { UnregisterHotKey(self.tray.hwnd, id) };
            if self.keys_on
                && key.is_set()
                && unsafe { RegisterHotKey(self.tray.hwnd, id, key.mods | MOD_NOREPEAT, key.vk) }
                    == 0
            {
                self.taken.push(name);
            }
        }
    }

    fn warn_taken(&mut self) {
        let keys: Vec<String> = [
            (self.cfg.clip_key, "clip"),
            (self.cfg.shot_key, "shot"),
            (self.cfg.pause_key, "pause"),
        ]
        .iter()
        .filter(|(_, n)| self.taken.contains(n))
        .map(|(k, _)| k.label())
        .collect();
        if !keys.is_empty() {
            log(&format!(
                "shortcuts taken by another app: {}",
                keys.join(", ")
            ));
            self.tray.notify(
                "Clipper",
                &format!("{} {}", keys.join(", "), t().key_taken),
                true,
            );
        }
    }

    /// Updates the tray icon and the status line the settings window shows.
    fn publish(&mut self) {
        let recording = !self.paused && self.session.is_some();
        let device = capture::monitors()
            .into_iter()
            .find(|m| m.handle == self.monitor)
            .map(|m| m.device)
            .unwrap_or_default();
        let tip = if recording {
            format!("Clipper · {} · {} s", t().recording, self.cfg.clip_secs)
        } else {
            format!("Clipper · {}", t().paused)
        };
        self.tray.set_state(recording, &tip);
        self.tray.set_status(&format!(
            "state={};monitor={};gpu={};taken={}",
            if recording { "recording" } else { "paused" },
            device,
            self.gpu.name,
            self.taken.join(",")
        ));
    }

    fn apply(&mut self, new: Config) {
        let old = std::mem::replace(&mut self.cfg, new);

        set_autostart(self.cfg.autostart);
        self.mixer
            .system
            .store(self.cfg.system_audio, std::sync::atomic::Ordering::Release);
        self.mixer
            .mic
            .store(self.cfg.mic, std::sync::atomic::Ordering::Release);
        let params_changed = params(&old) != params(&self.cfg);
        self.rec.set_params(params(&self.cfg));
        let keys_changed = (old.clip_key, old.shot_key, old.pause_key)
            != (self.cfg.clip_key, self.cfg.shot_key, self.cfg.pause_key);
        if keys_changed {
            self.register_keys();
            self.warn_taken();
        }
        self.fallback = fallback_monitor(&self.cfg);
        let monitor = target_monitor(&self.cfg, self.fallback);
        // New parameters start a new segment, which needs a fresh session to get its first frame.
        if !self.paused
            && (params_changed
                || old.cursor != self.cfg.cursor
                || old.gpu != self.cfg.gpu
                || monitor != self.monitor
                || self.session.is_none())
        {
            self.monitor = monitor;
            self.begin();
        }
        self.publish();
    }

    fn save_clip(&mut self) {
        let name = format!("Clip {}.mp4", stamp());
        self.rec.save(now_ticks(), self.cfg.clips_dir.join(name));
        if !self.paused {
            // A fresh session hands the new segment the current image even if nothing moves.
            self.begin();
        }
    }

    fn screenshot(&self) {
        let (gpu, monitor, hwnd) = (
            self.gpu.clone(),
            target_monitor(&self.cfg, self.fallback),
            self.tray.hwnd as usize,
        );
        let path = self
            .cfg
            .shots_dir
            .join(format!("Screenshot {}.png", stamp()));
        thread::spawn(move || {
            let _ = fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
            let result = capture::screenshot(&gpu, HMONITOR(monitor as _), &path)
                .map(|()| path)
                .map_err(|e| e.to_string());
            post_saved(hwnd, Saved::Shot(result));
        });
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if self.paused {
            drop(self.session.take());
            self.rec.stop();
            log("recording paused");
        } else {
            self.monitor = target_monitor(&self.cfg, self.fallback);
            self.begin();
            log("recording resumed");
        }
        self.publish();
    }

    fn saved(&mut self, saved: Saved) {
        let (ok_title, fail_title, result) = match saved {
            Saved::Clip(r) => (
                t().clip_saved,
                t().clip_failed,
                r.map(|(p, bytes)| (p, Some(bytes))),
            ),
            Saved::Shot(r) => (t().shot_saved, t().shot_failed, r.map(|p| (p, None))),
        };
        match result {
            Ok((path, bytes)) => {
                if self.cfg.sound {
                    beep(true);
                }
                if self.cfg.notify {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let size = bytes
                        .map(|b| format!(" · {:.0} MB", b as f64 / 1e6))
                        .unwrap_or_default();
                    self.tray.notify(
                        ok_title,
                        &format!("{name}{size}\n{}", t().click_to_open),
                        false,
                    );
                }
                self.last_saved = Some(path);
            }
            Err(e) => {
                if self.cfg.sound {
                    beep(false);
                }
                self.tray.notify(fail_title, &e, true);
            }
        }
    }

    fn menu(&mut self) {
        let tx = t();
        let label = |s: &str, k: Hotkey| {
            if k.is_set() {
                format!("{s}\t{}", k.label())
            } else {
                s.to_string()
            }
        };
        let items = vec![
            (MENU_CLIP, label(tx.save_clip, self.cfg.clip_key), true),
            (MENU_SHOT, label(tx.screenshot, self.cfg.shot_key), true),
            (
                MENU_PAUSE,
                label(
                    if self.paused { tx.resume } else { tx.pause },
                    self.cfg.pause_key,
                ),
                true,
            ),
            (0, String::new(), true),
            (MENU_CLIPS, tx.open_clips.to_string(), true),
            (MENU_SHOTS, tx.open_shots.to_string(), true),
            (0, String::new(), true),
            (MENU_SETTINGS, tx.settings.to_string(), true),
            (MENU_QUIT, tx.quit.to_string(), true),
        ];
        match self.tray.menu(&items) {
            MENU_CLIP => self.save_clip(),
            MENU_SHOT => self.screenshot(),
            MENU_PAUSE => self.toggle_pause(),
            MENU_CLIPS => open_folder(&self.cfg.clips_dir),
            MENU_SHOTS => open_folder(&self.cfg.shots_dir),
            MENU_SETTINGS => open_settings(),
            MENU_QUIT => {
                // Encoders may still hold the newest segment; whatever cannot be deleted now goes on the next start.
                let _ = fs::remove_dir_all(env::temp_dir().join("Clipper"));
                std::process::exit(0)
            }
            _ => {}
        }
    }

    fn tick(&mut self) {
        if self.paused {
            return;
        }
        // Follow the focused window to another monitor once it has stayed there for ~1 s.
        let m = target_monitor(&self.cfg, self.fallback);
        self.candidate = if m == self.candidate.0 {
            (m, self.candidate.1.saturating_add(1))
        } else {
            (m, 1)
        };
        let moved = m != self.monitor && self.candidate.1 >= 2;
        // Windows ends a session on display changes (resolution, unplugging, sleep).
        let dead = self.session.as_ref().is_none_or(|s| s.is_closed());
        if moved || dead {
            if moved {
                log("following focus to another monitor");
            } else {
                self.fallback = fallback_monitor(&self.cfg);
            }
            self.monitor = if moved {
                m
            } else {
                target_monitor(&self.cfg, self.fallback)
            };
            self.begin();
        }
    }
}

/// Holds the single-instance lock. Returns false when another Clipper already runs.
fn single_instance() -> bool {
    {
        unsafe {
            let h = CreateMutexW(null_mut(), 1, wide("Local\\ClipperTray").as_ptr());
            if !h.is_null() && GetLastError() != ERROR_ALREADY_EXISTS {
                return true; // keep the handle for the life of the process
            }
            windows_sys::Win32::Foundation::CloseHandle(h);
        }
    }
    false
}

pub fn run() {
    if !single_instance() {
        // Launching Clipper again (Start menu, shortcut) opens its settings instead.
        open_settings();
        return;
    }
    let cfg = Config::load();
    if fs::metadata(crate::config::dir().join("config.ini")).is_err() {
        let _ = cfg.save();
    }
    let segs = env::temp_dir().join("Clipper");
    let _ = fs::remove_dir_all(&segs);
    fs::create_dir_all(&segs).expect("cannot create the segment folder");

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).expect("Media Foundation is unavailable");
    }
    let gpu = Arc::new(capture::Gpu::new(&cfg.gpu).expect("no usable Direct3D 11 GPU"));
    log(&format!("using {}", gpu.name));
    let tray = Tray::new();
    let hwnd = tray.hwnd as usize;

    let mixer = audio::Mixer::new(cfg.system_audio, cfg.mic);
    audio::spawn(mixer.clone());
    let rec = Recorder::new(gpu.clone(), segs, params(&cfg), move |outcome| {
        post_saved(hwnd, Saved::Clip(outcome))
    });
    {
        let (rec, mixer) = (rec.clone(), mixer.clone());
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(20));
                rec.pump_audio(now_ticks(), &mixer);
            }
        });
    }

    let fallback = fallback_monitor(&cfg);
    let monitor = target_monitor(&cfg, fallback);
    let mut app = App {
        cfg,
        tray,
        gpu,
        rec,
        mixer,
        session: None,
        monitor,
        fallback,
        candidate: (monitor, 0),
        paused: false,
        keys_on: true,
        taken: Vec::new(),
        last_saved: None,
    };
    set_autostart(app.cfg.autostart);
    app.register_keys();
    app.warn_taken();
    app.begin();
    log(&format!(
        "Clipper running; clip {}, screenshot {}",
        app.cfg.clip_key.label(),
        app.cfg.shot_key.label()
    ));

    unsafe { SetTimer(app.tray.hwnd, 1, 500, None) };
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        match msg.message {
            WM_TIMER if msg.wParam == KEYS_TIMEOUT => {
                // The settings window never re-enabled the shortcuts (it crashed or was killed).
                unsafe { KillTimer(app.tray.hwnd, KEYS_TIMEOUT) };
                app.keys_on = true;
                app.register_keys();
            }
            WM_TIMER => app.tick(),
            WM_HOTKEY => match msg.wParam as i32 {
                HK_CLIP => app.save_clip(),
                HK_SHOT => app.screenshot(),
                HK_PAUSE => app.toggle_pause(),
                _ => {}
            },
            tray::WM_TRAY => match msg.wParam {
                NIN_SELECT | NIN_KEYSELECT => open_settings(),
                x if x == WM_CONTEXTMENU as usize => app.menu(),
                NIN_BALLOONUSERCLICK => {
                    if let Some(p) = &app.last_saved {
                        reveal(p);
                    }
                }
                _ => {}
            },
            tray::WM_SAVED => {
                let saved = unsafe { Box::from_raw(msg.lParam as *mut Saved) };
                app.saved(*saved);
            }
            tray::WM_RELOAD => app.apply(Config::load()),
            tray::WM_KEYS_OFF | tray::WM_KEYS_ON => {
                app.keys_on = msg.message == tray::WM_KEYS_ON;
                app.register_keys();
                unsafe {
                    if app.keys_on {
                        KillTimer(app.tray.hwnd, KEYS_TIMEOUT);
                    } else {
                        SetTimer(app.tray.hwnd, KEYS_TIMEOUT, 15_000, None);
                    }
                }
            }
            tray::WM_RECREATE => {
                app.tray.add();
                app.publish();
            }
            _ => unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            },
        }
    }
}
