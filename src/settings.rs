//! The settings window. It runs as its own short-lived process (`clipper.exe --settings`) and is
//! drawn on the CPU, so no graphics driver is loaded: about 15 MB while open, nothing after.

use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::Duration;

use egui::{
    Align, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Layout, Margin,
    RichText, Sense, Stroke, TextStyle, Theme, Vec2,
};
use egui_software_backend::{
    App, SoftwareBackend, SoftwareBackendAppConfiguration, run_app_with_software_backend,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::HSTRING;
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITORINFO,
    MonitorFromPoint,
};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RegGetValueW};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumThreadWindows, FindWindowW, GetClientRect, GetCursorPos, GetForegroundWindow,
    GetWindowRect, GetWindowTextW, IsIconic, IsZoomed, PostMessageW, SW_RESTORE, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowPos, ShowWindow, WINDOWPOS,
    WM_DPICHANGED, WM_WINDOWPOSCHANGING,
};

use crate::capture;
use crate::config::{
    CLIP_LENGTHS, Config, FRAME_RATES, Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, Quality,
};
use crate::text::{Text, t};
use crate::tray::{self, wide};

const TITLE: &str = "Clipper";
/// The record red from the app icon. Used only for the live "recording" indicator.
const RECORDING_RED: Color32 = Color32::from_rgb(229, 57, 53);
const PAUSED_GREY: Color32 = Color32::from_rgb(138, 143, 150);

/// Surface colors per theme, close to Windows 11 Settings. Text pairs keep at least 4.5:1.
struct Palette {
    window: Color32,
    card: Color32,
    border: Color32,
    text: Color32,
    secondary: Color32,
    accent: Color32,
    on_accent: Color32,
}

fn palette(dark: bool, accent: Accent) -> Palette {
    if dark {
        Palette {
            window: Color32::from_rgb(32, 32, 32),
            card: Color32::from_rgb(43, 43, 43),
            border: Color32::from_rgb(58, 58, 58),
            text: Color32::from_rgb(255, 255, 255),
            secondary: Color32::from_rgb(197, 197, 197),
            accent: accent.dark_theme,
            on_accent: Color32::BLACK,
        }
    } else {
        Palette {
            window: Color32::from_rgb(243, 243, 243),
            card: Color32::from_rgb(251, 251, 251),
            border: Color32::from_rgb(229, 229, 229),
            text: Color32::from_rgb(26, 26, 26),
            secondary: Color32::from_rgb(92, 92, 92),
            accent: accent.light_theme,
            on_accent: Color32::WHITE,
        }
    }
}

/// The Windows accent in the two shades Windows 11 itself fills controls with: Light 2 under
/// black text in dark mode, Dark 1 under white text in light mode.
#[derive(Clone, Copy)]
struct Accent {
    dark_theme: Color32,
    light_theme: Color32,
}

/// What the running tray process reports about itself.
struct Status {
    recording: bool,
    /// Games-only recording with no game open.
    waiting: bool,
    monitor: String,
    game: String,
    gpu: String,
}

fn tray_window() -> HWND {
    unsafe { FindWindowW(wide(tray::CLASS).as_ptr(), null_mut()) }
}

fn read_status() -> Option<Status> {
    let hwnd = tray_window();
    if hwnd.is_null() {
        return None;
    }
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) } as usize;
    let text = String::from_utf16_lossy(&buf[..n]);
    let field = |k: &str| {
        text.split(';')
            .find_map(|p| p.strip_prefix(k)?.strip_prefix('='))
            .unwrap_or("")
            .to_string()
    };
    Some(Status {
        recording: field("state") == "recording",
        waiting: field("state") == "waiting",
        monitor: field("monitor"),
        game: field("game"),
        gpu: field("gpu"),
    })
}

fn tell_tray(msg: u32) {
    let hwnd = tray_window();
    if !hwnd.is_null() {
        unsafe { PostMessageW(hwnd, msg, 0, 0) };
    }
}

fn system_accent() -> Accent {
    let mut palette = [0u8; 32];
    let mut size = palette.len() as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent").as_ptr(),
            wide("AccentPalette").as_ptr(),
            RRF_RT_REG_BINARY,
            null_mut(),
            palette.as_mut_ptr().cast(),
            &mut size,
        )
    } == 0;
    // Entries are RGBA, lightest first: 1 = Light 2, 4 = Dark 1.
    let shade =
        |i: usize| Color32::from_rgb(palette[i * 4], palette[i * 4 + 1], palette[i * 4 + 2]);
    if ok && size >= 32 {
        Accent {
            dark_theme: shade(1),
            light_theme: shade(4),
        }
    } else {
        // Windows' default blue.
        Accent {
            dark_theme: Color32::from_rgb(153, 235, 255),
            light_theme: Color32::from_rgb(0, 90, 158),
        }
    }
}
fn pick_folder(start: &Path) -> Option<PathBuf> {
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dialog
            .SetOptions(dialog.GetOptions().ok()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
            .ok()?;
        if let Ok(folder) =
            SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(start.as_os_str()), None)
        {
            let _ = dialog.SetFolder(&folder);
        }
        let owner = windows::Win32::Foundation::HWND(GetForegroundWindow());
        dialog.Show(Some(owner)).ok()?;
        let name = dialog
            .GetResult()
            .ok()?
            .GetDisplayName(SIGDN_FILESYSPATH)
            .ok()?;
        let path = name.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(name.0 as _));
        path
    }
}

/// Maps a key pressed in the window to the virtual-key code Windows uses.
fn vk(key: egui::Key) -> Option<u32> {
    use egui::Key::*;
    let letters = [
        A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
    ];
    let digits = [Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9];
    let fkeys = [
        F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19, F20,
    ];
    if let Some(i) = letters.iter().position(|k| *k == key) {
        return Some(0x41 + i as u32);
    }
    if let Some(i) = digits.iter().position(|k| *k == key) {
        return Some(0x30 + i as u32);
    }
    if let Some(i) = fkeys.iter().position(|k| *k == key) {
        return Some(0x70 + i as u32);
    }
    Some(match key {
        Insert => 0x2D,
        Delete => 0x2E,
        Home => 0x24,
        End => 0x23,
        PageUp => 0x21,
        PageDown => 0x22,
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Which {
    Clip,
    Shot,
    Pause,
}

struct Settings {
    saved: Config,
    edit: Config,
    monitors: Vec<capture::MonitorInfo>,
    gpus: Vec<String>,
    outputs: Vec<String>,
    inputs: Vec<String>,
    listening: Option<Which>,
    message: Option<(String, bool)>,
    status: Option<Status>,
    accent: Accent,
    icon: egui::TextureHandle,
}

// ---------------------------------------------------------------------------------------------
// Widgets

fn focus_ring(ui: &egui::Ui, response: &egui::Response, radius: CornerRadius, p: &Palette) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            response.rect.expand(3.0),
            radius,
            Stroke::new(2.0_f32, p.text),
            egui::StrokeKind::Outside,
        );
    }
}

/// A Windows 11 style on/off switch, keyboard-focusable.
fn switch(ui: &mut egui::Ui, on: &mut bool, p: &Palette) {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(40.0, 20.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, *on, ""));
    let t = ui.ctx().animate_bool_with_time(response.id, *on, 0.12);
    let painter = ui.painter();
    let radius = CornerRadius::same(10);
    if *on {
        painter.rect_filled(rect, radius, p.accent);
    } else {
        painter.rect_stroke(
            rect.shrink(0.5),
            radius,
            Stroke::new(1.0_f32, p.secondary),
            egui::StrokeKind::Inside,
        );
    }
    let x = egui::lerp(rect.left() + 10.0..=rect.right() - 10.0, t);
    let knob = if *on { p.on_accent } else { p.secondary };
    painter.circle_filled(
        egui::pos2(x, rect.center().y),
        if response.hovered() { 6.0 } else { 5.0 },
        knob,
    );
    focus_ring(ui, &response, radius, p);
}

/// Mutually exclusive options; the chosen one carries the accent. Laid out right to left.
fn segmented<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    value: &mut T,
    options: &[(T, String)],
    p: &Palette,
) {
    ui.spacing_mut().item_spacing.x = 4.0;
    for (v, label) in options.iter().rev() {
        let selected = *value == *v;
        let button = egui::Button::new(RichText::new(label).color(if selected {
            p.on_accent
        } else {
            p.text
        }))
        .fill(if selected {
            p.accent
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(
            1.0_f32,
            if selected { p.accent } else { p.border },
        ))
        .corner_radius(CornerRadius::same(4))
        .min_size(Vec2::new(0.0, 30.0));
        let response = ui.add(button);
        focus_ring(ui, &response, CornerRadius::same(4), p);
        if response.clicked() {
            *value = *v;
        }
    }
}

/// A shortcut shown as key caps ("Alt" "G"); click it, then press the new combination.
fn key_caps(
    ui: &mut egui::Ui,
    key: Hotkey,
    listening: bool,
    prompt: &str,
    p: &Palette,
) -> egui::Response {
    let parts: Vec<String> = if listening {
        vec![prompt.to_string()]
    } else if !key.is_set() {
        vec!["—".to_string()]
    } else {
        key.label().split('+').map(str::to_string).collect()
    };
    let font = TextStyle::Button.resolve(ui.style());
    let galleys: Vec<_> = parts
        .iter()
        .map(|s| ui.painter().layout_no_wrap(s.clone(), font.clone(), p.text))
        .collect();
    let (pad, gap) = (9.0, 6.0);
    let width = galleys.iter().map(|g| g.size().x + pad * 2.0).sum::<f32>()
        + gap * (galleys.len() as f32 - 1.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width.max(60.0), 30.0), Sense::click());
    let edge = if listening || response.hovered() {
        p.accent
    } else {
        p.border
    };
    let painter = ui.painter();
    let mut x = rect.right() - width;
    for g in galleys {
        let cap = egui::Rect::from_min_size(
            egui::pos2(x, rect.top()),
            Vec2::new(g.size().x + pad * 2.0, rect.height()),
        );
        painter.rect(
            cap,
            CornerRadius::same(5),
            p.window,
            Stroke::new(1.0_f32, edge),
            egui::StrokeKind::Inside,
        );
        // A heavier bottom edge reads as a physical key.
        painter.hline(
            cap.x_range().shrink(3.0),
            cap.bottom() - 1.5,
            Stroke::new(2.0_f32, edge),
        );
        painter.galley(cap.center() - g.size() / 2.0, g, p.text);
        x += cap.width() + gap;
    }
    focus_ring(ui, &response, CornerRadius::same(5), p);
    response
}

/// A device list with "Windows default" first. A saved device that is not connected right now
/// stays selectable, so opening settings never silently changes the choice.
fn device_choice(ui: &mut egui::Ui, id: &str, value: &mut String, devices: &[String], auto: &str) {
    let shown = if value.is_empty() {
        auto.to_string()
    } else {
        value.clone()
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(shown)
        .width(250.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(value, String::new(), auto);
            let missing = (!value.is_empty() && !devices.contains(value)).then(|| value.clone());
            for name in devices.iter().cloned().chain(missing) {
                ui.selectable_value(value, name.clone(), name);
            }
        });
}

/// A settings card: a titled group of rows on a raised surface.
fn card(ui: &mut egui::Ui, title: &str, p: &Palette, rows: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(22.0);
    ui.label(
        RichText::new(title)
            .text_style(TextStyle::Name("section".into()))
            .color(p.text),
    );
    ui.add_space(8.0);
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(16, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            rows(ui);
        });
}

/// One row: title (and an optional detail line) on the left, the control on the right.
fn row(
    ui: &mut egui::Ui,
    title: &str,
    detail: Option<&str>,
    p: &Palette,
    first: bool,
    control: impl FnOnce(&mut egui::Ui),
) {
    if !first {
        let y = ui.cursor().top();
        ui.painter()
            .hline(ui.max_rect().x_range(), y, Stroke::new(1.0_f32, p.border));
    }
    let height = if detail.is_some() { 60.0 } else { 52.0 };
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), height),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_height(height);
            ui.vertical(|ui| {
                ui.add_space(if detail.is_some() { 11.0 } else { 16.0 });
                ui.label(RichText::new(title).color(p.text));
                if let Some(d) = detail {
                    ui.label(RichText::new(d).small().color(p.secondary));
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), control);
        },
    );
}

// ---------------------------------------------------------------------------------------------

impl Settings {
    fn key_mut(&mut self, w: Which) -> &mut Hotkey {
        match w {
            Which::Clip => &mut self.edit.clip_key,
            Which::Shot => &mut self.edit.shot_key,
            Which::Pause => &mut self.edit.pause_key,
        }
    }

    /// While a shortcut is being recorded the tray stops reacting to its own shortcuts.
    fn listen(&mut self, w: Option<Which>) {
        if self.listening.is_none() && w.is_some() {
            tell_tray(tray::WM_KEYS_OFF);
        } else if self.listening.is_some() && w.is_none() {
            tell_tray(tray::WM_KEYS_ON);
        }
        self.listening = w;
    }

    fn capture_key(&mut self, ctx: &egui::Context) {
        let Some(which) = self.listening else { return };
        let pressed = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => Some((*key, *modifiers)),
                _ => None,
            })
        });
        let Some((key, m)) = pressed else { return };
        if key == egui::Key::Escape {
            self.listen(None);
            return;
        }
        if let Some(code) = vk(key) {
            let mods = if m.alt { MOD_ALT } else { 0 }
                | if m.ctrl { MOD_CONTROL } else { 0 }
                | if m.shift { MOD_SHIFT } else { 0 };
            *self.key_mut(which) = Hotkey { mods, vk: code };
            self.listen(None);
        }
    }

    fn save(&mut self) {
        match self.edit.save() {
            Ok(()) => {
                self.saved = self.edit.clone();
                tell_tray(tray::WM_RELOAD);
                self.message = Some((t().saved.to_string(), false));
            }
            Err(e) => self.message = Some((format!("{}: {e}", t().save_failed), true)),
        }
    }

    fn monitor_label(&self, i: usize, m: &capture::MonitorInfo) -> String {
        let main = if m.primary {
            format!(" · {}", t().primary)
        } else {
            String::new()
        };
        format!("Monitor {} · {}×{}{main}", i + 1, m.size.0, m.size.1)
    }

    fn header(&mut self, ui: &mut egui::Ui, tx: &Text, p: &Palette) {
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(&self.icon).fit_to_exact_size(Vec2::splat(36.0)));
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(TITLE)
                        .text_style(TextStyle::Heading)
                        .color(p.text),
                );
                ui.horizontal(|ui| match &self.status {
                    Some(s) => {
                        // The dot marks a real state: red while recording, grey while paused.
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                        ui.painter().circle_filled(
                            rect.center(),
                            4.5,
                            if s.recording {
                                RECORDING_RED
                            } else {
                                PAUSED_GREY
                            },
                        );
                        let text = if s.recording {
                            let what = if !s.game.is_empty() {
                                format!(" · {}", s.game)
                            } else {
                                self.monitors
                                    .iter()
                                    .position(|m| m.device == s.monitor)
                                    .map(|i| format!(" · Monitor {}", i + 1))
                                    .unwrap_or_default()
                            };
                            let secs = self.saved.clip_secs.to_string();
                            format!(
                                "{}{what} · {}",
                                tx.status_recording.replace("{secs}", &secs),
                                s.gpu
                            )
                        } else if s.waiting {
                            tx.status_waiting.to_string()
                        } else {
                            tx.status_paused.to_string()
                        };
                        ui.label(RichText::new(text).color(p.secondary));
                    }
                    None => {
                        ui.label(RichText::new(tx.status_off).color(p.secondary));
                    }
                });
                if self.status.is_none() {
                    ui.add_space(4.0);
                    if ui.button(tx.start_clipper).clicked()
                        && let Ok(exe) = std::env::current_exe()
                    {
                        let _ = std::process::Command::new(exe).spawn();
                    }
                }
            });
        });
    }

    fn body(&mut self, ui: &mut egui::Ui, tx: &Text, p: &Palette) {
        card(ui, tx.sec_recording, p, |ui| {
            let secs: Vec<(u32, String)> = CLIP_LENGTHS
                .iter()
                .map(|&s| (s, format!("{s} {}", tx.seconds)))
                .collect();
            row(ui, tx.clip_length, None, p, true, |ui| {
                segmented(ui, &mut self.edit.clip_secs, &secs, p)
            });
            let fps: Vec<(u32, String)> = FRAME_RATES
                .iter()
                .map(|&f| (f, format!("{f} fps")))
                .collect();
            row(ui, tx.frame_rate, None, p, false, |ui| {
                segmented(ui, &mut self.edit.fps, &fps, p)
            });
            let (w, h) = self.monitors.first().map_or((1920, 1080), |m| m.size);
            let preset = |q: Quality, fps: u32| q.bitrate(w, h, fps);
            let bitrate = if self.edit.bitrate_kbps > 0 {
                self.edit.bitrate_kbps * 1000
            } else {
                preset(self.edit.quality, self.edit.fps)
            };
            let mb = bitrate as f64 * self.edit.clip_secs as f64 / 8e6;
            let estimate = tx.disk_estimate.replace("{mb}", &format!("{mb:.0}"));
            // The three presets, then Custom: a bitrate of the user's own.
            const CUSTOM: usize = 3;
            let mut choice = if self.edit.bitrate_kbps > 0 {
                CUSTOM
            } else {
                Quality::ALL
                    .iter()
                    .position(|q| *q == self.edit.quality)
                    .unwrap_or(1)
            };
            let q = [
                (0, tx.q_low.to_string()),
                (1, tx.q_medium.to_string()),
                (2, tx.q_high.to_string()),
                (CUSTOM, tx.q_custom.to_string()),
            ];
            row(ui, tx.quality, Some(&estimate), p, false, |ui| {
                segmented(ui, &mut choice, &q, p)
            });
            match choice {
                // Custom starts from what the current preset uses, rounded to 500 kbps.
                CUSTOM if self.edit.bitrate_kbps == 0 => {
                    let kbps = preset(self.edit.quality, self.edit.fps) / 1000;
                    self.edit.bitrate_kbps = ((kbps + 250) / 500 * 500).clamp(
                        *crate::config::BITRATES.start(),
                        *crate::config::BITRATES.end(),
                    );
                }
                CUSTOM => {}
                i => {
                    self.edit.quality = Quality::ALL[i];
                    self.edit.bitrate_kbps = 0;
                }
            }
            if self.edit.bitrate_kbps > 0 {
                row(ui, tx.bitrate, None, p, false, |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.edit.bitrate_kbps)
                            .range(crate::config::BITRATES)
                            .speed(100.0)
                            .suffix(" kbps"),
                    );
                });
            }
            row(ui, tx.show_cursor, None, p, false, |ui| {
                switch(ui, &mut self.edit.cursor, p)
            });
        });

        card(ui, tx.sec_hotkeys, p, |ui| {
            let keys = [
                (Which::Clip, tx.key_clip),
                (Which::Shot, tx.key_shot),
                (Which::Pause, tx.key_pause),
            ];
            for (n, (which, label)) in keys.into_iter().enumerate() {
                let key = *self.key_mut(which);
                let all = [self.edit.clip_key, self.edit.shot_key, self.edit.pause_key];
                let duplicate = key.is_set() && all.iter().filter(|k| **k == key).count() > 1;
                row(
                    ui,
                    label,
                    duplicate.then_some(tx.key_conflict),
                    p,
                    n == 0,
                    |ui| {
                        let listening = self.listening == Some(which);
                        if key_caps(ui, key, listening, tx.press_keys, p).clicked() {
                            self.listen(if listening { None } else { Some(which) });
                        }
                        if key.is_set()
                            && !listening
                            && ui
                                .add(
                                    egui::Button::new(RichText::new(tx.clear).color(p.secondary))
                                        .frame(false),
                                )
                                .clicked()
                        {
                            *self.key_mut(which) = Hotkey::NONE;
                        }
                    },
                );
            }
        });

        card(ui, tx.sec_audio, p, |ui| {
            row(ui, tx.system_audio, None, p, true, |ui| {
                switch(ui, &mut self.edit.system_audio, p)
            });
            if self.edit.system_audio {
                row(ui, tx.output_device, None, p, false, |ui| {
                    device_choice(
                        ui,
                        "output",
                        &mut self.edit.system_device,
                        &self.outputs,
                        tx.device_auto,
                    );
                });
            }
            row(ui, tx.microphone, None, p, false, |ui| {
                switch(ui, &mut self.edit.mic, p)
            });
            if self.edit.mic {
                row(ui, tx.input_device, None, p, false, |ui| {
                    device_choice(
                        ui,
                        "input",
                        &mut self.edit.mic_device,
                        &self.inputs,
                        tx.device_auto,
                    );
                });
            }
        });

        card(ui, tx.sec_capture, p, |ui| {
            row(ui, tx.record_desktop, None, p, true, |ui| {
                switch(ui, &mut self.edit.record_desktop, p)
            });
            row(ui, tx.follow_focus, None, p, false, |ui| {
                switch(ui, &mut self.edit.follow_focus, p)
            });
            let current = self
                .monitors
                .iter()
                .enumerate()
                .find(|(_, m)| m.device == self.edit.monitor)
                .or_else(|| self.monitors.iter().enumerate().find(|(_, m)| m.primary))
                .map(|(i, m)| self.monitor_label(i, m))
                .unwrap_or_default();
            let options: Vec<(String, String)> = self
                .monitors
                .iter()
                .enumerate()
                .map(|(i, m)| (m.device.clone(), self.monitor_label(i, m)))
                .collect();
            let label = if self.edit.follow_focus {
                tx.monitor_fallback
            } else {
                tx.monitor_fixed
            };
            row(ui, label, None, p, false, |ui| {
                egui::ComboBox::from_id_salt("monitor")
                    .selected_text(current)
                    .width(250.0)
                    .show_ui(ui, |ui| {
                        for (device, text) in &options {
                            ui.selectable_value(&mut self.edit.monitor, device.clone(), text);
                        }
                    });
            });
            let gpu_text = if self.edit.gpu.is_empty() {
                tx.gpu_auto.to_string()
            } else {
                self.edit.gpu.clone()
            };
            row(ui, tx.gpu, None, p, false, |ui| {
                egui::ComboBox::from_id_salt("gpu")
                    .selected_text(gpu_text)
                    .width(250.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.edit.gpu, String::new(), tx.gpu_auto);
                        for g in &self.gpus {
                            ui.selectable_value(&mut self.edit.gpu, g.clone(), g);
                        }
                    });
            });
        });

        card(ui, tx.sec_files, p, |ui| {
            let folders = [
                (tx.clips_folder, &mut self.edit.clips_dir),
                (tx.shots_folder, &mut self.edit.shots_dir),
            ];
            for (n, (label, dir)) in folders.into_iter().enumerate() {
                let shown = dir.display().to_string();
                row(ui, label, Some(&shown), p, n == 0, |ui| {
                    if ui.button(tx.open).clicked() {
                        let _ = std::fs::create_dir_all(&*dir);
                        let _ = std::process::Command::new("explorer.exe")
                            .arg(&*dir)
                            .spawn();
                    }
                    if ui.button(tx.change).clicked()
                        && let Some(path) = pick_folder(dir)
                    {
                        *dir = path;
                    }
                });
            }
        });

        card(ui, tx.sec_general, p, |ui| {
            row(ui, tx.notifications, None, p, true, |ui| {
                switch(ui, &mut self.edit.notify, p)
            });
            row(ui, tx.autostart, None, p, false, |ui| {
                switch(ui, &mut self.edit.autostart, p)
            });
        });
        ui.add_space(16.0);
    }

    fn footer(&mut self, ui: &mut egui::Ui, tx: &Text, p: &Palette) {
        let dirty = self.edit != self.saved;
        ui.horizontal(|ui| {
            let (text, error) = match (&self.message, dirty) {
                (_, true) => (tx.unsaved.to_string(), false),
                (Some((m, e)), false) => (m.clone(), *e),
                (None, false) => (String::new(), false),
            };
            ui.label(RichText::new(text).color(if error {
                ui.visuals().error_fg_color
            } else {
                p.secondary
            }));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = RichText::new(tx.save)
                    .color(if dirty { p.on_accent } else { p.secondary })
                    .strong();
                let save = egui::Button::new(label)
                    .fill(if dirty { p.accent } else { p.card })
                    .stroke(Stroke::new(
                        1.0_f32,
                        if dirty { p.accent } else { p.border },
                    ))
                    .corner_radius(CornerRadius::same(4))
                    .min_size(Vec2::new(120.0, 32.0));
                let response = ui.add_enabled(dirty, save);
                focus_ring(ui, &response, CornerRadius::same(4), p);
                if response.clicked() {
                    self.save();
                }
            });
        });
    }
}

impl App for Settings {
    fn ui(&mut self, ui: &mut egui::Ui, _backend: &mut SoftwareBackend) {
        let ctx = ui.ctx().clone();
        self.capture_key(&ctx);
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::S)) && self.edit != self.saved
        {
            self.save();
        }
        // The tray's status changes on its own (pause, monitor switch); poll it gently.
        self.status = read_status();
        ctx.request_repaint_after(Duration::from_secs(1));

        let tx = t();
        let p = palette(ui.visuals().dark_mode, self.accent);
        egui::Panel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(p.window)
                    // The right edge lines up with the cards (central margin plus scroll bar room).
                    .inner_margin(Margin {
                        left: 28,
                        right: 36,
                        top: 18,
                        bottom: 18,
                    }),
            )
            .show_separator_line(false)
            .show_inside(ui, |ui| self.header(ui, tx, &p));
        egui::Panel::bottom("footer")
            .frame(
                egui::Frame::new()
                    .fill(p.window)
                    .inner_margin(Margin::symmetric(28, 14)),
            )
            .show_inside(ui, |ui| self.footer(ui, tx, &p));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.window).inner_margin(Margin {
                left: 28,
                right: 20,
                top: 0,
                bottom: 0,
            }))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    // Leave room for the scroll bar so it never sits on top of a card.
                    ui.set_width(ui.available_width() - 16.0);
                    self.body(ui, tx, &p);
                });
            });
    }

    fn on_exit(&mut self, _ctx: &egui::Context) {
        self.listen(None);
        save_placement();
    }
}

const MIN_SIZE: Vec2 = Vec2::new(560.0, 480.0);

fn placement_path() -> PathBuf {
    crate::config::dir().join("window.ini")
}

fn dpi_scale(monitor: HMONITOR) -> f32 {
    let (mut dpi, mut unused) = (96u32, 96u32);
    unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut unused) };
    dpi as f32 / 96.0
}

fn work_area(monitor: HMONITOR) -> RECT {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    unsafe { GetMonitorInfoW(monitor, &mut info) };
    info.rcWork
}

/// The settings window: the one on this thread with our title (winit keeps hidden helper
/// windows on the same thread).
fn own_window() -> HWND {
    unsafe extern "system" fn titled(hwnd: HWND, found: LPARAM) -> windows_sys::core::BOOL {
        let mut buf = [0u16; 16];
        let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) } as usize;
        if String::from_utf16_lossy(&buf[..n]) == TITLE {
            unsafe { *(found as *mut HWND) = hwnd };
            return 0;
        }
        1
    }
    let mut hwnd: HWND = null_mut();
    unsafe {
        EnumThreadWindows(
            GetCurrentThreadId(),
            Some(titled),
            &mut hwnd as *mut HWND as LPARAM,
        )
    };
    hwnd
}

/// Puts the not yet painted window where it was last closed, while that spot is on a connected
/// monitor, or else centred on the monitor under the mouse at a height that fits there.
///
/// Everything here is in screen pixels: egui's points are converted by winit with the scale of
/// whichever monitor the window starts on, which lands it in the wrong place when monitors
/// use different scales.
fn place_window() {
    let hwnd = own_window();
    if hwnd.is_null() {
        return;
    }
    unsafe { SetWindowSubclass(hwnd, Some(keep_suggested_rect), 1, 0) };
    let saved: Option<[i32; 4]> = std::fs::read_to_string(placement_path())
        .ok()
        .and_then(|t| {
            let v: Vec<i32> = t
                .split_whitespace()
                .filter_map(|n| n.parse().ok())
                .collect();
            v.try_into().ok()
        });
    let flags = SWP_NOZORDER | SWP_NOACTIVATE;
    unsafe {
        if let Some([x, y, w, h]) = saved {
            // The title bar has to land on a monitor, or the window could not be dragged back.
            let monitor = MonitorFromPoint(
                POINT {
                    x: x + 120,
                    y: y + 16,
                },
                MONITOR_DEFAULTTONULL,
            );
            if !monitor.is_null() {
                let work = work_area(monitor);
                // Move first: crossing to a monitor with another scale makes winit resize the
                // window, so the size is set once it is there.
                SetWindowPos(hwnd, null_mut(), x, y, 0, 0, flags | SWP_NOSIZE);
                let (w, h) = (w.min(work.right - work.left), h.min(work.bottom - work.top));
                SetWindowPos(hwnd, null_mut(), x, y, w, h, flags);
                return;
            }
        }
        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let work = work_area(monitor);
        SetWindowPos(
            hwnd,
            null_mut(),
            work.left,
            work.top,
            0,
            0,
            flags | SWP_NOSIZE,
        );
        let s = dpi_scale(monitor);
        let (work_w, work_h) = (work.right - work.left, work.bottom - work.top);
        // Leave room for the title bar and a margin above and below.
        let client = Vec2::new(640.0, (work_h as f32 / s - 96.0).min(780.0)).max(MIN_SIZE) * s;
        let (mut outer, mut inner): (RECT, RECT) = (std::mem::zeroed(), std::mem::zeroed());
        GetWindowRect(hwnd, &mut outer);
        GetClientRect(hwnd, &mut inner);
        let w = client.x as i32 + (outer.right - outer.left) - inner.right;
        let h = client.y as i32 + (outer.bottom - outer.top) - inner.bottom;
        let x = work.left + (work_w - w) / 2;
        let y = work.top + ((work_h - h) / 2).max(0);
        SetWindowPos(hwnd, null_mut(), x, y, w, h, flags);
    }
}

thread_local! {
    /// The rect Windows suggested for the scale change being handled, if any.
    static SUGGESTED: std::cell::Cell<Option<RECT>> = const { std::cell::Cell::new(None) };
}

/// winit 0.30 answers a scale change by moving the window to a rect of its own, shifted to keep
/// the cursor's spot on the title bar. Dragged across the edge between monitors with different
/// scales, that rect puts most of the window back on the old monitor: the scale flips back
/// inside the first change, and the window grows or shrinks on every round. Windows' suggested
/// rect is built to stay on the new monitor, so winit's move is redirected there while winit
/// still takes the new scale.
unsafe extern "system" fn keep_suggested_rect(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    unsafe {
        match msg {
            WM_DPICHANGED => {
                SUGGESTED.set(Some(*(lparam as *const RECT)));
                let result = DefSubclassProc(hwnd, msg, wparam, lparam);
                SUGGESTED.set(None);
                result
            }
            WM_WINDOWPOSCHANGING => {
                if let Some(r) = SUGGESTED.get() {
                    let pos = &mut *(lparam as *mut WINDOWPOS);
                    pos.x = r.left;
                    pos.y = r.top;
                    pos.cx = r.right - r.left;
                    pos.cy = r.bottom - r.top;
                    pos.flags &= !(SWP_NOMOVE | SWP_NOSIZE);
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

/// Remembers the window's spot for next time. A maximized or minimized window keeps the
/// previous record, since its normal size is the one worth restoring.
fn save_placement() {
    let hwnd = own_window();
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        if hwnd.is_null()
            || IsZoomed(hwnd) != 0
            || IsIconic(hwnd) != 0
            || GetWindowRect(hwnd, &mut r) == 0
        {
            return;
        }
        let _ = std::fs::write(
            placement_path(),
            format!(
                "{} {} {} {}
",
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top
            ),
        );
    }
}

fn load_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let read = |name: &str| std::fs::read(Path::new(r"C:\Windows\Fonts").join(name)).ok();
    // Segoe UI Variable is the Windows 11 UI font; plain Segoe UI covers Windows 10.
    if let Some(regular) = read("SegUIVar.ttf").or_else(|| read("segoeui.ttf")) {
        fonts
            .font_data
            .insert("ui".into(), Arc::new(FontData::from_owned(regular)));
        fonts
            .families
            .get_mut(&FontFamily::Proportional)
            .unwrap()
            .insert(0, "ui".into());
    }
    let mut semibold = fonts.families[&FontFamily::Proportional].clone();
    if let Some(bytes) = read("seguisb.ttf") {
        fonts
            .font_data
            .insert("ui-semibold".into(), Arc::new(FontData::from_owned(bytes)));
        semibold.insert(0, "ui-semibold".into());
    }
    fonts
        .families
        .insert(FontFamily::Name("semibold".into()), semibold);
    ctx.set_fonts(fonts);
}

fn style(ctx: &egui::Context, accent: Accent) {
    let semibold = FontFamily::Name("semibold".into());
    for theme in [Theme::Dark, Theme::Light] {
        let p = palette(theme == Theme::Dark, accent);
        ctx.style_mut_of(theme, |s| {
            s.text_styles = [
                (TextStyle::Heading, FontId::new(20.0, semibold.clone())),
                (
                    TextStyle::Name("section".into()),
                    FontId::new(14.0, semibold.clone()),
                ),
                (TextStyle::Body, FontId::proportional(14.0)),
                (TextStyle::Button, FontId::proportional(14.0)),
                (TextStyle::Small, FontId::proportional(12.0)),
                (TextStyle::Monospace, FontId::monospace(13.0)),
            ]
            .into();
            s.spacing.item_spacing = Vec2::new(8.0, 2.0);
            s.spacing.button_padding = Vec2::new(12.0, 6.0);
            s.spacing.interact_size.y = 30.0;
            s.visuals.panel_fill = p.window;
            s.visuals.window_fill = p.card;
            s.visuals.extreme_bg_color = p.window;
            s.visuals.selection.bg_fill = p.accent;
            s.visuals.selection.stroke = Stroke::new(1.0_f32, p.on_accent);
            s.visuals.widgets.noninteractive.fg_stroke.color = p.text;
            s.visuals.widgets.noninteractive.bg_stroke.color = p.border;
            let widgets = &mut s.visuals.widgets;
            for w in [
                &mut widgets.inactive,
                &mut widgets.hovered,
                &mut widgets.active,
                &mut widgets.open,
            ] {
                w.corner_radius = CornerRadius::same(4);
                w.fg_stroke.color = p.text;
            }
            widgets.inactive.weak_bg_fill = p.card;
            widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
            widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.secondary);
        });
    }
}

fn already_open() -> bool {
    unsafe {
        let h = CreateMutexW(null_mut(), 1, wide("Local\\ClipperSettings").as_ptr());
        if h.is_null() || GetLastError() != ERROR_ALREADY_EXISTS {
            return false;
        }
        let other = FindWindowW(null_mut(), wide(TITLE).as_ptr());
        if !other.is_null() {
            ShowWindow(other, SW_RESTORE);
            SetForegroundWindow(other);
        }
        true
    }
}

pub fn run() {
    if already_open() {
        return;
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let saved = Config::load();
    let (outputs, inputs) = crate::audio::device_names();
    let rgba = include_bytes!("../assets/icon-64.rgba").to_vec();
    let viewport = egui::ViewportBuilder::default()
        .with_title(TITLE)
        .with_inner_size([640.0, 780.0])
        .with_min_inner_size(MIN_SIZE)
        .with_icon(Arc::new(egui::IconData {
            rgba: rgba.clone(),
            width: 64,
            height: 64,
        }));
    let config = SoftwareBackendAppConfiguration::new().viewport_builder(viewport);
    let accent = system_accent();
    let _ = run_app_with_software_backend(config, move |ctx| {
        // The window exists but has not been painted yet: placing it now means it never shows
        // up anywhere else first.
        place_window();
        load_fonts(&ctx);
        style(&ctx, accent);
        let image = egui::ColorImage::from_rgba_unmultiplied([64, 64], &rgba);
        Settings {
            edit: saved.clone(),
            saved: saved.clone(),
            monitors: capture::monitors(),
            gpus: capture::adapters(),
            outputs: outputs.clone(),
            inputs: inputs.clone(),
            listening: None,
            message: None,
            status: read_status(),
            accent,
            icon: ctx.load_texture("app-icon", image, egui::TextureOptions::LINEAR),
        }
    });
}
