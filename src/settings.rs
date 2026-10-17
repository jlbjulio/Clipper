//! The settings window. It runs as its own short-lived process (`clipper.exe --settings`), so
//! the recorder in the tray never carries the UI's memory.

use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Layout,
    Margin, RichText, Stroke, TextStyle, Theme, Vec2,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::HSTRING;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetForegroundWindow, GetWindowTextW, PostMessageW, SW_RESTORE,
    SetForegroundWindow, ShowWindow,
};

use crate::capture;
use crate::config::{
    CLIP_LENGTHS, Config, FRAME_RATES, Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, Quality,
};
use crate::text::{Text, t};
use crate::tray::{self, wide};

const TITLE: &str = "Clipper";
const RECORDING_RED: Color32 = Color32::from_rgb(229, 57, 53);

/// What the running tray process reports about itself.
struct Status {
    recording: bool,
    monitor: String,
    gpu: String,
    taken: Vec<String>,
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
        monitor: field("monitor"),
        gpu: field("gpu"),
        taken: field("taken")
            .split(',')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
    })
}

fn tell_tray(msg: u32) {
    let hwnd = tray_window();
    if !hwnd.is_null() {
        unsafe { PostMessageW(hwnd, msg, 0, 0) };
    }
}

/// The Windows accent color, so controls match the rest of the system.
fn accent() -> Color32 {
    let mut value = 0u32;
    let mut size = 4u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(r"Software\Microsoft\Windows\DWM").as_ptr(),
            wide("AccentColor").as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut value as *mut u32).cast(),
            &mut size,
        )
    } == 0;
    if ok {
        let [r, g, b, _] = value.to_le_bytes();
        Color32::from_rgb(r, g, b)
    } else {
        Color32::from_rgb(0, 103, 192)
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

/// Maps a key pressed in the window to the virtual-key code Windows registers.
fn vk(key: egui::Key) -> Option<u32> {
    use egui::Key::*;
    let letters = [
        A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
    ];
    let digits = [Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9];
    let fkeys = [
        F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19, F20,
        F21, F22, F23, F24,
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
    listening: Option<Which>,
    message: Option<(String, bool)>,
    status: Option<Status>,
}

impl Settings {
    fn key_mut(&mut self, w: Which) -> &mut Hotkey {
        match w {
            Which::Clip => &mut self.edit.clip_key,
            Which::Shot => &mut self.edit.shot_key,
            Which::Pause => &mut self.edit.pause_key,
        }
    }

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
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(18.0);
    let color = ui.visuals().strong_text_color();
    ui.label(
        RichText::new(title)
            .text_style(TextStyle::Name("section".into()))
            .color(color),
    );
    ui.add_space(6.0);
}

fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).small().weak());
}

/// Help text under a checkbox, aligned with the checkbox's label.
fn note(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(ui.spacing().icon_width + ui.spacing().icon_spacing);
        hint(ui, text);
    });
}

const LABEL_WIDTH: f32 = 190.0;

/// Help text in the control column, wrapping to the window width.
fn row_note(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
        ui.add(egui::Label::new(RichText::new(text).small().weak()).wrap());
    });
}

/// One labeled row: a fixed-width label column keeps every control aligned.
fn row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        let size = Vec2::new(LABEL_WIDTH, ui.spacing().interact_size.y);
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        let font = TextStyle::Body.resolve(ui.style());
        let color = ui.visuals().text_color();
        ui.painter().text(
            rect.left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            font,
            color,
        );
        add(ui);
    });
}

fn choice<T: PartialEq + Copy>(ui: &mut egui::Ui, value: &mut T, options: &[(T, String)]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (v, label) in options {
            ui.selectable_value(value, *v, label.as_str());
        }
    });
}

impl Settings {
    fn header(&mut self, ui: &mut egui::Ui, tx: &Text) {
        ui.horizontal(|ui| {
            let color = ui.visuals().strong_text_color();
            ui.label(
                RichText::new(TITLE)
                    .text_style(TextStyle::Heading)
                    .color(color),
            );
        });
        ui.add_space(4.0);
        match &self.status {
            Some(s) => {
                ui.horizontal(|ui| {
                    if s.recording {
                        // A real state indicator: the recorder is running right now.
                        let (rect, _) =
                            ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
                        ui.painter()
                            .circle_filled(rect.center(), 5.0, RECORDING_RED);
                        let monitor = self
                            .monitors
                            .iter()
                            .position(|m| m.device == s.monitor)
                            .map(|i| format!(" · Monitor {}", i + 1))
                            .unwrap_or_default();
                        ui.label(format!(
                            "{}{monitor} · {}",
                            tx.status_recording
                                .replace("{secs}", &self.saved.clip_secs.to_string()),
                            s.gpu
                        ));
                    } else {
                        ui.label(tx.status_paused);
                    }
                });
            }
            None => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tx.status_off).weak());
                    if ui.button(tx.start_clipper).clicked()
                        && let Ok(exe) = std::env::current_exe()
                    {
                        let _ = std::process::Command::new(exe).spawn();
                    }
                });
            }
        }
    }

    fn body(&mut self, ui: &mut egui::Ui, tx: &Text) {
        section(ui, tx.sec_recording);
        let secs: Vec<(u32, String)> = CLIP_LENGTHS
            .iter()
            .map(|&s| (s, format!("{s} {}", tx.seconds)))
            .collect();
        row(ui, tx.clip_length, |ui| {
            choice(ui, &mut self.edit.clip_secs, &secs)
        });
        let fps: Vec<(u32, String)> = FRAME_RATES
            .iter()
            .map(|&f| (f, format!("{f} fps")))
            .collect();
        row(ui, tx.frame_rate, |ui| choice(ui, &mut self.edit.fps, &fps));
        let q = [
            (Quality::Low, tx.q_low.to_string()),
            (Quality::Medium, tx.q_medium.to_string()),
            (Quality::High, tx.q_high.to_string()),
        ];
        row(ui, tx.quality, |ui| choice(ui, &mut self.edit.quality, &q));
        let (w, h) = self.monitors.first().map_or((1920, 1080), |m| m.size);
        let mb = self.edit.quality.bitrate(w, h, self.edit.fps) as f64 * self.edit.clip_secs as f64
            / 8e6;
        row_note(ui, &tx.disk_estimate.replace("{mb}", &format!("{:.0}", mb)));
        ui.checkbox(&mut self.edit.cursor, tx.show_cursor);

        section(ui, tx.sec_hotkeys);
        let taken = self
            .status
            .as_ref()
            .map(|s| s.taken.clone())
            .unwrap_or_default();
        for (which, label, tag) in [
            (Which::Clip, tx.key_clip, "clip"),
            (Which::Shot, tx.key_shot, "shot"),
            (Which::Pause, tx.key_pause, "pause"),
        ] {
            let key = *self.key_mut(which);
            let others = [self.edit.clip_key, self.edit.shot_key, self.edit.pause_key];
            let duplicate = key.is_set() && others.iter().filter(|k| **k == key).count() > 1;
            let conflict = duplicate
                || (taken.iter().any(|t| t == tag)
                    && key
                        == *match which {
                            Which::Clip => &self.saved.clip_key,
                            Which::Shot => &self.saved.shot_key,
                            Which::Pause => &self.saved.pause_key,
                        });
            row(ui, label, |ui| {
                let listening = self.listening == Some(which);
                let text = if listening {
                    tx.press_keys.to_string()
                } else {
                    key.label()
                };
                if ui
                    .add(
                        egui::Button::new(text)
                            .min_size(Vec2::new(170.0, 28.0))
                            .selected(listening),
                    )
                    .clicked()
                {
                    self.listen(if listening { None } else { Some(which) });
                }
                if key.is_set() && ui.small_button(tx.clear).clicked() {
                    *self.key_mut(which) = Hotkey::NONE;
                }
                if conflict {
                    ui.label(
                        RichText::new(tx.key_conflict)
                            .small()
                            .color(ui.visuals().error_fg_color),
                    );
                }
            });
        }

        section(ui, tx.sec_audio);
        ui.checkbox(&mut self.edit.system_audio, tx.system_audio);
        note(ui, tx.system_audio_hint);
        ui.checkbox(&mut self.edit.mic, tx.microphone);

        section(ui, tx.sec_capture);
        ui.checkbox(&mut self.edit.follow_focus, tx.follow_focus);
        note(ui, tx.follow_focus_hint);
        let label = |i: usize, m: &capture::MonitorInfo| {
            let main = if m.primary {
                format!(" · {}", tx.primary)
            } else {
                String::new()
            };
            format!("Monitor {} · {}×{}{main}", i + 1, m.size.0, m.size.1)
        };
        let current = self
            .monitors
            .iter()
            .enumerate()
            .find(|(_, m)| m.device == self.edit.monitor)
            .or_else(|| self.monitors.iter().enumerate().find(|(_, m)| m.primary))
            .map(|(i, m)| label(i, m))
            .unwrap_or_default();
        let monitor_label = if self.edit.follow_focus {
            tx.monitor_fallback
        } else {
            tx.monitor_fixed
        };
        let monitors: Vec<(String, String)> = self
            .monitors
            .iter()
            .enumerate()
            .map(|(i, m)| (m.device.clone(), label(i, m)))
            .collect();
        row(ui, monitor_label, |ui| {
            egui::ComboBox::from_id_salt("monitor")
                .selected_text(current)
                .width(260.0)
                .show_ui(ui, |ui| {
                    for (device, text) in &monitors {
                        ui.selectable_value(&mut self.edit.monitor, device.clone(), text);
                    }
                });
        });
        let gpu_text = if self.edit.gpu.is_empty() {
            tx.gpu_auto.to_string()
        } else {
            self.edit.gpu.clone()
        };
        row(ui, tx.gpu, |ui| {
            egui::ComboBox::from_id_salt("gpu")
                .selected_text(gpu_text)
                .width(260.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.edit.gpu, String::new(), tx.gpu_auto);
                    for g in &self.gpus {
                        ui.selectable_value(&mut self.edit.gpu, g.clone(), g);
                    }
                });
        });
        row_note(ui, tx.gpu_hint);

        section(ui, tx.sec_files);
        for (label, dir) in [
            (tx.clips_folder, &mut self.edit.clips_dir),
            (tx.shots_folder, &mut self.edit.shots_dir),
        ] {
            row(ui, label, |ui| {
                ui.add(
                    egui::Label::new(RichText::new(dir.display().to_string()).weak()).truncate(),
                )
                .on_hover_text(dir.display().to_string());
            });
            row(ui, "", |ui| {
                if ui.button(tx.change).clicked()
                    && let Some(p) = pick_folder(dir)
                {
                    *dir = p;
                }
                if ui.button(tx.open).clicked() {
                    let _ = std::fs::create_dir_all(&*dir);
                    let _ = std::process::Command::new("explorer.exe")
                        .arg(&*dir)
                        .spawn();
                }
            });
        }

        section(ui, tx.sec_general);
        ui.checkbox(&mut self.edit.notify, tx.notifications);
        ui.checkbox(&mut self.edit.sound, tx.sound);
        ui.checkbox(&mut self.edit.autostart, tx.autostart);
        ui.add_space(12.0);
    }

    fn footer(&mut self, ui: &mut egui::Ui, tx: &Text, accent: Color32) {
        let dirty = self.edit != self.saved;
        ui.horizontal(|ui| {
            let (text, error) = match (&self.message, dirty) {
                (_, true) => (tx.unsaved.to_string(), false),
                (Some((m, e)), false) => (m.clone(), *e),
                (None, false) => (String::new(), false),
            };
            let color = if error {
                ui.visuals().error_fg_color
            } else {
                ui.visuals().weak_text_color()
            };
            ui.label(RichText::new(text).color(color));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let save = egui::Button::new(RichText::new(tx.save).color(Color32::WHITE).strong())
                    .fill(if dirty {
                        accent
                    } else {
                        accent.gamma_multiply(0.4)
                    })
                    .min_size(Vec2::new(110.0, 32.0));
                if ui.add_enabled(dirty, save).clicked() {
                    self.save();
                }
            });
        });
    }
}

impl eframe::App for Settings {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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
        let accent = accent();
        let fill = ui.visuals().panel_fill;
        let pad = Margin::symmetric(24, 16);
        egui::Panel::top("header")
            .frame(egui::Frame::new().fill(fill).inner_margin(pad))
            .show(ui, |ui| self.header(ui, tx));
        egui::Panel::bottom("footer")
            .frame(egui::Frame::new().fill(fill).inner_margin(pad))
            .show(ui, |ui| self.footer(ui, tx, accent));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(fill)
                    .inner_margin(Margin::symmetric(24, 0)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    self.body(ui, tx);
                });
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.listen(None);
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
    if let Some(semibold) = read("seguisb.ttf") {
        fonts.font_data.insert(
            "ui-semibold".into(),
            Arc::new(FontData::from_owned(semibold)),
        );
        let mut family = vec!["ui-semibold".to_string()];
        family.extend(fonts.families[&FontFamily::Proportional].iter().cloned());
        fonts
            .families
            .insert(FontFamily::Name("semibold".into()), family);
    } else {
        fonts.families.insert(
            FontFamily::Name("semibold".into()),
            fonts.families[&FontFamily::Proportional].clone(),
        );
    }
    ctx.set_fonts(fonts);
}

fn style(ctx: &egui::Context) {
    let accent = accent();
    let semibold = FontFamily::Name("semibold".into());
    for theme in [Theme::Dark, Theme::Light] {
        ctx.style_mut_of(theme, |s| {
            s.text_styles = [
                (TextStyle::Heading, FontId::new(22.0, semibold.clone())),
                (
                    TextStyle::Name("section".into()),
                    FontId::new(15.0, semibold.clone()),
                ),
                (TextStyle::Body, FontId::proportional(14.0)),
                (TextStyle::Button, FontId::proportional(14.0)),
                (TextStyle::Small, FontId::proportional(12.5)),
                (TextStyle::Monospace, FontId::monospace(13.0)),
            ]
            .into();
            s.spacing.item_spacing = Vec2::new(8.0, 8.0);
            s.spacing.button_padding = Vec2::new(12.0, 5.0);
            s.spacing.interact_size.y = 28.0;
            s.spacing.indent = 28.0;
            let radius = CornerRadius::same(5);
            for w in [
                &mut s.visuals.widgets.inactive,
                &mut s.visuals.widgets.hovered,
                &mut s.visuals.widgets.active,
                &mut s.visuals.widgets.open,
                &mut s.visuals.widgets.noninteractive,
            ] {
                w.corner_radius = radius;
            }
            // Body text at full contrast; egui's default grey is too faint for reading.
            s.visuals.widgets.noninteractive.fg_stroke.color = if theme == Theme::Dark {
                Color32::from_gray(225)
            } else {
                Color32::from_gray(30)
            };
            s.visuals.selection.bg_fill = accent;
            s.visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
            s.visuals.hyperlink_color = accent;
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
    let icon = egui::IconData {
        rgba: include_bytes!("../assets/icon-64.rgba").to_vec(),
        width: 64,
        height: 64,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([620.0, 760.0])
            .with_min_inner_size([520.0, 480.0])
            .with_icon(Arc::new(icon)),
        ..Default::default()
    };
    let _ = eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| {
            load_fonts(&cc.egui_ctx);
            style(&cc.egui_ctx);
            Ok(Box::new(Settings {
                edit: saved.clone(),
                saved,
                monitors: capture::monitors(),
                gpus: capture::adapters(),
                listening: None,
                message: None,
                status: read_status(),
            }))
        }),
    );
}
