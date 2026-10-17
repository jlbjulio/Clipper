//! User settings, stored as readable `key = value` lines in %APPDATA%\Clipper\config.ini.

use std::path::PathBuf;
use std::{env, fs, io};

pub const MOD_ALT: u32 = 1;
pub const MOD_CONTROL: u32 = 2;
pub const MOD_SHIFT: u32 = 4;
pub const MOD_WIN: u32 = 8;

/// A global shortcut as Windows registers it: modifier bits plus a virtual-key code.
/// `vk == 0` means "not assigned".
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

const MODS: [(u32, &str); 4] = [
    (MOD_CONTROL, "Ctrl"),
    (MOD_ALT, "Alt"),
    (MOD_SHIFT, "Shift"),
    (MOD_WIN, "Win"),
];

fn key_name(vk: u32) -> Option<String> {
    Some(match vk {
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x30..=0x39 | 0x41..=0x5A => char::from(vk as u8).to_string(),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        0x2C => "PrintScreen".into(),
        0x13 => "Pause".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x91 => "ScrollLock".into(),
        _ => return None,
    })
}

impl Hotkey {
    pub const NONE: Hotkey = Hotkey { mods: 0, vk: 0 };

    pub fn is_set(self) -> bool {
        self.vk != 0
    }

    pub fn label(self) -> String {
        if !self.is_set() {
            return "None".into();
        }
        let mut parts: Vec<String> = MODS
            .iter()
            .filter(|(bit, _)| self.mods & bit != 0)
            .map(|(_, n)| n.to_string())
            .collect();
        parts.push(key_name(self.vk).unwrap_or_else(|| format!("0x{:02X}", self.vk)));
        parts.join("+")
    }

    pub fn parse(text: &str) -> Option<Hotkey> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("none") || text.is_empty() {
            return Some(Hotkey::NONE);
        }
        let mut key = Hotkey::NONE;
        for part in text.split('+').map(str::trim) {
            if let Some((bit, _)) = MODS.iter().find(|(_, n)| n.eq_ignore_ascii_case(part)) {
                key.mods |= bit;
            } else if let Some(hex) = part.strip_prefix("0x") {
                key.vk = u32::from_str_radix(hex, 16).ok()?;
            } else {
                key.vk = (1..=0xFE)
                    .find(|&vk| key_name(vk).is_some_and(|n| n.eq_ignore_ascii_case(part)))?;
            }
        }
        key.is_set().then_some(key)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    Low,
    Medium,
    High,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Quality::Low, Quality::Medium, Quality::High];

    /// Bits per second for H.264: about 15 Mbps at 1080p60 for Medium, scaled by pixel rate.
    pub fn bitrate(self, width: u32, height: u32, fps: u32) -> u32 {
        let bits_per_pixel = match self {
            Quality::Low => 0.035,
            Quality::Medium => 0.12,
            Quality::High => 0.2,
        };
        (width as f64 * height as f64 * fps as f64 * bits_per_pixel).clamp(1e6, 100e6) as u32
    }

    fn key(self) -> &'static str {
        match self {
            Quality::Low => "low",
            Quality::Medium => "medium",
            Quality::High => "high",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Config {
    pub clip_secs: u32,
    pub fps: u32,
    pub quality: Quality,
    /// Target bitrate in kbps, overriding `quality`; 0 uses the quality preset.
    pub bitrate_kbps: u32,
    pub cursor: bool,
    pub system_audio: bool,
    pub mic: bool,
    /// Playback device to record by name; empty picks automatically.
    pub system_device: String,
    /// Microphone by name; empty uses the Windows default.
    pub mic_device: String,
    /// Record the desktop while no game is open; off records games only.
    pub record_desktop: bool,
    /// Record the monitor holding the focused window; otherwise always `monitor`.
    pub follow_focus: bool,
    /// Device name (\\.\DISPLAY1) of the fixed or fallback monitor; empty means the primary one.
    pub monitor: String,
    /// GPU used to capture and encode; empty lets Windows pick (the one driving the main display).
    pub gpu: String,
    pub clip_key: Hotkey,
    pub shot_key: Hotkey,
    pub pause_key: Hotkey,
    pub clips_dir: PathBuf,
    pub shots_dir: PathBuf,
    pub notify: bool,
    pub autostart: bool,
}

pub const CLIP_LENGTHS: [u32; 5] = [15, 30, 60, 90, 120];
pub const FRAME_RATES: [u32; 2] = [30, 60];
/// Accepted custom bitrates in kbps.
pub const BITRATES: std::ops::RangeInclusive<u32> = 1_000..=100_000;

/// Videos\Captures, where Windows' own Game Bar saves clips and screenshots. The Videos folder
/// comes from Windows, so it follows a library moved to OneDrive or another drive.
fn captures() -> PathBuf {
    use windows::Win32::UI::Shell::{FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    let videos = unsafe { SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None) }
        .ok()
        .and_then(|p| {
            let path = unsafe { p.to_string() }.ok();
            unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _)) };
            path
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("USERPROFILE").unwrap_or_default()).join("Videos")
        });
    videos.join("Captures")
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clip_secs: 30,
            fps: 60,
            quality: Quality::Medium,
            bitrate_kbps: 0,
            cursor: true,
            system_audio: true,
            mic: true,
            system_device: String::new(),
            mic_device: String::new(),
            record_desktop: false,
            follow_focus: true,
            monitor: String::new(),
            gpu: String::new(),
            clip_key: Hotkey {
                mods: MOD_ALT,
                vk: 0x47,
            }, // Alt+G
            shot_key: Hotkey {
                mods: MOD_ALT,
                vk: 0x70,
            }, // Alt+F1
            pause_key: Hotkey {
                mods: MOD_ALT,
                vk: 0x78,
            }, // Alt+F9
            clips_dir: captures(),
            shots_dir: captures(),
            notify: true,
            autostart: true,
        }
    }
}

pub fn dir() -> PathBuf {
    PathBuf::from(env::var_os("APPDATA").unwrap_or_default()).join("Clipper")
}

fn path() -> PathBuf {
    dir().join("config.ini")
}

impl Config {
    /// Reads the saved settings; missing or unreadable values keep their defaults.
    pub fn load() -> Config {
        fs::read_to_string(path())
            .map(|t| Config::parse(&t))
            .unwrap_or_default()
    }

    pub fn parse(text: &str) -> Config {
        let mut c = Config::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            let flag = || match value {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
            match key.trim() {
                "clip_seconds" => {
                    c.clip_secs = value
                        .parse()
                        .ok()
                        .filter(|v| (5..=600).contains(v))
                        .unwrap_or(c.clip_secs)
                }
                "fps" => {
                    c.fps = value
                        .parse()
                        .ok()
                        .filter(|v| FRAME_RATES.contains(v))
                        .unwrap_or(c.fps)
                }
                "quality" => {
                    c.quality = Quality::ALL
                        .into_iter()
                        .find(|q| q.key() == value)
                        .unwrap_or(c.quality)
                }
                "bitrate_kbps" => {
                    c.bitrate_kbps = value
                        .parse()
                        .ok()
                        .filter(|v| *v == 0 || BITRATES.contains(v))
                        .unwrap_or(c.bitrate_kbps)
                }
                "cursor" => c.cursor = flag().unwrap_or(c.cursor),
                "system_audio" => c.system_audio = flag().unwrap_or(c.system_audio),
                "microphone" => c.mic = flag().unwrap_or(c.mic),
                "system_audio_device" => c.system_device = value.to_string(),
                "microphone_device" => c.mic_device = value.to_string(),
                "record_desktop" => c.record_desktop = flag().unwrap_or(c.record_desktop),
                "follow_focus" => c.follow_focus = flag().unwrap_or(c.follow_focus),
                "monitor" => c.monitor = value.to_string(),
                "gpu" => c.gpu = value.to_string(),
                "clip_hotkey" => c.clip_key = Hotkey::parse(value).unwrap_or(c.clip_key),
                "screenshot_hotkey" => c.shot_key = Hotkey::parse(value).unwrap_or(c.shot_key),
                "pause_hotkey" => c.pause_key = Hotkey::parse(value).unwrap_or(c.pause_key),
                "clips_folder" if !value.is_empty() => c.clips_dir = value.into(),
                "screenshots_folder" if !value.is_empty() => c.shots_dir = value.into(),
                "notifications" => c.notify = flag().unwrap_or(c.notify),
                "start_with_windows" => c.autostart = flag().unwrap_or(c.autostart),
                _ => {}
            }
        }
        c
    }

    pub fn to_text(&self) -> String {
        format!(
            "# Clipper settings. Edit here or from the tray icon > Settings.\n\
             clip_seconds = {}\nfps = {}\nquality = {}\nbitrate_kbps = {}\ncursor = {}\n\
             system_audio = {}\nmicrophone = {}\nsystem_audio_device = {}\nmicrophone_device = {}\n\
             record_desktop = {}\nfollow_focus = {}\nmonitor = {}\ngpu = {}\n\
             clip_hotkey = {}\nscreenshot_hotkey = {}\npause_hotkey = {}\n\
             clips_folder = {}\nscreenshots_folder = {}\n\
             notifications = {}\nstart_with_windows = {}\n",
            self.clip_secs,
            self.fps,
            self.quality.key(),
            self.bitrate_kbps,
            self.cursor,
            self.system_audio,
            self.mic,
            self.system_device,
            self.mic_device,
            self.record_desktop,
            self.follow_focus,
            self.monitor,
            self.gpu,
            self.clip_key.label(),
            self.shot_key.label(),
            self.pause_key.label(),
            self.clips_dir.display(),
            self.shots_dir.display(),
            self.notify,
            self.autostart,
        )
    }

    pub fn save(&self) -> io::Result<()> {
        fs::create_dir_all(dir())?;
        // Write then rename so a crash never leaves a half-written file behind.
        let tmp = path().with_extension("tmp");
        fs::write(&tmp, self.to_text())?;
        fs::rename(tmp, path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_field() {
        let c = Config {
            clip_secs: 90,
            fps: 30,
            quality: Quality::High,
            bitrate_kbps: 16_000,
            cursor: false,
            system_audio: false,
            mic: false,
            system_device: "SteelSeries Sonar - Gaming (SteelSeries Sonar Virtual Audio Device)"
                .into(),
            mic_device: "Microphone (Blue Snowball)".into(),
            record_desktop: true,
            follow_focus: false,
            monitor: r"\\.\DISPLAY2".into(),
            gpu: "NVIDIA GeForce RTX 3050 Laptop GPU".into(),
            clip_key: Hotkey {
                mods: MOD_CONTROL | MOD_SHIFT,
                vk: 0x43,
            },
            shot_key: Hotkey::NONE,
            pause_key: Hotkey {
                mods: MOD_WIN,
                vk: 0x2C,
            },
            clips_dir: r"D:\Clips".into(),
            shots_dir: r"D:\Shots".into(),
            notify: false,
            autostart: false,
        };
        assert_eq!(Config::parse(&c.to_text()), c);
    }

    #[test]
    fn bad_values_fall_back_to_defaults() {
        let c = Config::parse(
            "fps = 75\nclip_seconds = -3\nquality = ultra\nclip_hotkey = Alt+Banana\ncursor = maybe\n\
             bitrate_kbps = 50\nrecord_desktop = sometimes",
        );
        assert_eq!(c, Config::default());
    }

    #[test]
    fn hotkey_labels_parse_back() {
        for text in [
            "Alt+F10",
            "Ctrl+Shift+S",
            "Win+PrintScreen",
            "Ctrl+Alt+Num5",
            "None",
        ] {
            assert_eq!(Hotkey::parse(text).unwrap().label(), text);
        }
        assert_eq!(
            Hotkey::parse("alt + f10"),
            Some(Hotkey {
                mods: MOD_ALT,
                vk: 0x79
            })
        );
        assert_eq!(Hotkey::parse("Alt"), None);
    }

    #[test]
    fn medium_quality_is_about_15_mbps_at_1080p60() {
        let b = Quality::Medium.bitrate(1920, 1080, 60);
        assert!((14_000_000..16_000_000).contains(&b), "{b}");
        assert!(
            Quality::Low.bitrate(1920, 1080, 60) < b && b < Quality::High.bitrate(1920, 1080, 60)
        );
    }
}
