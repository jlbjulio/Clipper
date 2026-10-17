//! Finds the game to record.
//!
//! A process counts as a game when it runs from a game library installed on this PC (Steam,
//! Epic, Riot, GOG, Ubisoft, EA, the Xbox app), when Windows itself lists it as one (the
//! GameConfigStore the Xbox Game Bar fills as games run), when its file has the name of a game
//! Windows listed (a copy elsewhere, such as a private-server client of an official game), or
//! once it has covered its whole monitor while in front, as full-screen games do. Launchers, chat apps and browsers are never
//! games, even when they sit in a library, Windows lists them, or they go full screen.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromWindow,
};
use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ, RegCloseKey,
    RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetForegroundWindow, GetWindow,
    GetWindowLongW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, WS_EX_TOOLWINDOW,
};

use crate::tray::wide;

/// Programs Windows sometimes lists as games, that sit in a game library, or that go full
/// screen (video, slides, calls), but are not games.
const NOT_GAMES: &[&str] = &[
    "applicationframehost.exe",
    "battle.net.exe",
    "brave.exe",
    "chrome.exe",
    "clipper.exe",
    "code.exe",
    "discord.exe",
    "eadesktop.exe",
    "epicgameslauncher.exe",
    "explorer.exe",
    "firefox.exe",
    "galaxyclient.exe",
    "leagueclient.exe",
    "leagueclientux.exe",
    "leagueclientuxrender.exe",
    "microsoft.media.player.exe",
    "mpc-be64.exe",
    "mpc-hc.exe",
    "mpc-hc64.exe",
    "ms-teams.exe",
    "msedge.exe",
    "msedgewebview2.exe",
    "mpv.exe",
    "obs64.exe",
    "opera.exe",
    "potplayermini.exe",
    "potplayermini64.exe",
    "powerpnt.exe",
    "riotclientservices.exe",
    "riotclientux.exe",
    "riotclientuxrender.exe",
    "robloxstudiobeta.exe",
    "spotify.exe",
    "steam.exe",
    "steamwebhelper.exe",
    "teams.exe",
    "ubisoftconnect.exe",
    "ui32.exe",
    "vlc.exe",
    "wallpaper32.exe",
    "wallpaper64.exe",
    "wmplayer.exe",
    "zoom.exe",
];

/// File names too common to identify a game on their own: runtimes and engines many programs
/// share.
const GENERIC_NAMES: &[&str] = &[
    "electron.exe",
    "game.exe",
    "java.exe",
    "javaw.exe",
    "launcher.exe",
    "love.exe",
    "nw.exe",
    "python.exe",
    "pythonw.exe",
];

/// How often the libraries and Windows' list are read again, to pick up games installed since.
const RELOAD: Duration = Duration::from_secs(60);
/// GetDriveTypeW for a local disk.
const DRIVE_FIXED: u32 = 3;

pub struct Game {
    pub hwnd: usize,
    /// The window title, for the status line.
    pub name: String,
}

pub struct Games {
    /// Lower-cased executable paths Windows lists as games.
    known: HashSet<String>,
    /// Their distinctive file names, such as wow.exe.
    known_names: HashSet<String>,
    /// Lower-cased library folders, each ending in a backslash.
    libraries: Vec<String>,
    loaded: Instant,
    /// Per process id: its executable path (empty when unreadable) and whether it is listed
    /// as a game or installed in a library.
    exes: HashMap<u32, (String, bool)>,
    /// Processes seen covering their monitor while in front.
    full_screen: HashSet<u32>,
}

struct Window {
    hwnd: HWND,
    pid: u32,
    area: i64,
}

impl Games {
    pub fn new() -> Games {
        let known = known_games();
        Games {
            known_names: file_names(&known),
            known,
            libraries: libraries(),
            loaded: Instant::now(),
            exes: HashMap::new(),
            full_screen: HashSet::new(),
        }
    }

    /// The game window to record: the focused one if it is a game, else `current` while it is
    /// still a visible game, else the biggest visible game window. Minimized games are skipped.
    pub fn find(&mut self, current: Option<usize>) -> Option<Game> {
        if self.loaded.elapsed() >= RELOAD {
            self.known = known_games();
            self.known_names = file_names(&self.known);
            self.libraries = libraries();
            self.loaded = Instant::now();
            self.exes.clear();
        }
        let windows = top_windows();
        let alive: HashSet<u32> = windows.iter().map(|w| w.pid).collect();
        self.exes.retain(|pid, _| alive.contains(pid));
        self.full_screen.retain(|pid| alive.contains(pid));

        let fg = unsafe { GetForegroundWindow() };
        if let Some(w) = windows.iter().find(|w| w.hwnd == fg)
            && covers_monitor(fg)
            && !self.excluded(w.pid)
        {
            self.full_screen.insert(w.pid);
        }

        let games: Vec<&Window> = windows
            .iter()
            .filter(|w| unsafe { IsIconic(w.hwnd) } == 0 && self.is_game(w.pid))
            .collect();
        let pick = games
            .iter()
            .find(|w| w.hwnd == fg)
            .or_else(|| games.iter().find(|w| Some(w.hwnd as usize) == current))
            .or_else(|| games.iter().max_by_key(|w| w.area))?;
        Some(Game {
            hwnd: pick.hwnd as usize,
            name: title(pick.hwnd).unwrap_or_else(|| {
                let exe = self
                    .exes
                    .get(&pick.pid)
                    .map(|e| e.0.clone())
                    .unwrap_or_default();
                Path::new(&exe)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }),
        })
    }

    fn exe(&mut self, pid: u32) -> &(String, bool) {
        let (known, names, libraries) = (&self.known, &self.known_names, &self.libraries);
        self.exes.entry(pid).or_insert_with(|| {
            let exe = exe_path(pid);
            let lower = exe.to_lowercase();
            let name = lower.rsplit('\\').next().unwrap_or_default();
            let listed = !exe.is_empty()
                && (known.contains(&lower)
                    || names.contains(name)
                    || libraries.iter().any(|l| lower.starts_with(l)));
            (exe, listed)
        })
    }

    /// An unreadable path (anti-cheat may protect the process) is not a reason to exclude it;
    /// it can still count as a game by going full screen.
    fn excluded(&mut self, pid: u32) -> bool {
        let name = Path::new(&self.exe(pid).0)
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        NOT_GAMES.contains(&name.as_str())
    }

    fn is_game(&mut self, pid: u32) -> bool {
        if self.excluded(pid) {
            return false;
        }
        self.full_screen.contains(&pid) || self.exe(pid).1
    }
}

/// Visible top-level application windows: no owner, not tool windows, minimized included.
fn top_windows() -> Vec<Window> {
    unsafe extern "system" fn collect(hwnd: HWND, list: LPARAM) -> windows_sys::core::BOOL {
        unsafe {
            if IsWindowVisible(hwnd) == 0
                || !GetWindow(hwnd, GW_OWNER).is_null()
                || GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0
            {
                return 1;
            }
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let mut r: RECT = std::mem::zeroed();
            GetWindowRect(hwnd, &mut r);
            let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
            (*(list as *mut Vec<Window>)).push(Window { hwnd, pid, area });
        }
        1
    }
    let mut list = Vec::new();
    unsafe { EnumWindows(Some(collect), &mut list as *mut Vec<Window> as LPARAM) };
    list
}

/// True when the window fills its whole monitor, as a full-screen game does. The desktop and
/// taskbar span monitors too, so they are left out.
fn covers_monitor(hwnd: HWND) -> bool {
    unsafe {
        let mut class = [0u16; 64];
        let n = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) as usize;
        if matches!(
            String::from_utf16_lossy(&class[..n]).as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        ) {
            return false;
        }
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..std::mem::zeroed()
        };
        let mut r: RECT = std::mem::zeroed();
        if monitor.is_null()
            || GetMonitorInfoW(monitor, &mut info) == 0
            || GetWindowRect(hwnd, &mut r) == 0
        {
            return false;
        }
        let m = info.rcMonitor;
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

fn title(hwnd: HWND) -> Option<String> {
    let mut buf = [0u16; 128];
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) } as usize;
    let t = String::from_utf16_lossy(&buf[..n]).trim().to_string();
    (!t.is_empty()).then_some(t)
}

fn exe_path(pid: u32) -> String {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(process);
        if ok {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            String::new()
        }
    }
}

/// Lower-cased executable paths of the games Windows has recognized on this PC.
fn known_games() -> HashSet<String> {
    subkey_values(
        HKEY_CURRENT_USER,
        r"System\GameConfigStore\Children",
        "MatchedExeFullPath",
    )
    .into_iter()
    .map(|p| p.to_lowercase())
    .collect()
}

/// The distinctive file names among `paths`.
fn file_names(paths: &HashSet<String>) -> HashSet<String> {
    paths
        .iter()
        .filter_map(|p| p.rsplit('\\').next())
        .filter(|n| !GENERIC_NAMES.contains(n) && !NOT_GAMES.contains(n))
        .map(str::to_string)
        .collect()
}

/// Folders games are installed in, lower-cased and ending in a backslash.
fn libraries() -> Vec<String> {
    let mut dirs: Vec<String> = Vec::new();
    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let files = |dir: &Path, ext: &str| -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext)))
            .collect()
    };

    // Steam: every library folder listed in libraryfolders.vdf.
    if let Some(steam) = string_value(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath") {
        let vdf = read(&Path::new(&steam).join(r"steamapps\libraryfolders.vdf"));
        for line in vdf
            .lines()
            .filter(|l| l.trim_start().starts_with("\"path\""))
        {
            if let Some(path) = line.split('"').nth(3) {
                dirs.push(format!(r"{}\steamapps\common", path.replace(r"\\", r"\")));
            }
        }
    }
    // Epic: one manifest per installed game.
    let epic = Path::new(&program_data).join(r"Epic\EpicGamesLauncher\Data\Manifests");
    for item in files(&epic, "item") {
        let text = read(&item);
        if let Some(line) = text.lines().find(|l| l.contains("\"InstallLocation\""))
            && let Some(path) = line.split('"').nth(3)
        {
            dirs.push(path.replace(r"\\", r"\"));
        }
    }
    // Riot: the default folder, plus any product installed elsewhere.
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    dirs.push(format!(r"{drive}\Riot Games"));
    let riot = Path::new(&program_data).join(r"Riot Games\Metadata");
    for product in std::fs::read_dir(riot).into_iter().flatten().flatten() {
        for yaml in files(&product.path(), "yaml") {
            for line in read(&yaml).lines() {
                if let Some(rest) = line.trim().strip_prefix("product_install_full_path:") {
                    dirs.push(rest.trim().trim_matches('"').to_string());
                }
            }
        }
    }
    // GOG, Ubisoft and EA register each game's folder.
    for (key, value) in [
        (r"SOFTWARE\WOW6432Node\GOG.com\Games", "path"),
        (
            r"SOFTWARE\WOW6432Node\Ubisoft\Launcher\Installs",
            "InstallDir",
        ),
        (r"SOFTWARE\WOW6432Node\EA Games", "Install Dir"),
    ] {
        dirs.extend(subkey_values(HKEY_LOCAL_MACHINE, key, value));
    }
    // The Xbox app installs to an XboxGames folder at the root of a drive. Only fixed drives
    // are checked: a disconnected network drive could stall the lookup.
    for letter in 'C'..='Z' {
        let root = format!(r"{letter}:\");
        let folder = format!(r"{letter}:\XboxGames");
        if unsafe { GetDriveTypeW(wide(&root).as_ptr()) } == DRIVE_FIXED
            && Path::new(&folder).is_dir()
        {
            dirs.push(folder);
        }
    }

    let mut dirs: Vec<String> = dirs
        .into_iter()
        .map(|d| {
            let d = d.replace('/', r"\").trim_end_matches('\\').to_lowercase();
            format!(r"{d}\")
        })
        // A drive root or the Riot launcher's own folder would make everything there a game.
        .filter(|d| d.matches('\\').count() >= 2 && !d.ends_with(r"\riot client\"))
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

fn string_value(root: HKEY, key: &str, value: &str) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let ok = unsafe {
        RegGetValueW(
            root,
            wide(key).as_ptr(),
            wide(value).as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    } == 0;
    let chars = (size as usize / 2).saturating_sub(1);
    (ok && chars > 0).then(|| String::from_utf16_lossy(&buf[..chars]))
}

/// The string `value` of every subkey of `key`.
fn subkey_values(root: HKEY, key: &str, value: &str) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let mut handle: HKEY = null_mut();
        if RegOpenKeyExW(root, wide(key).as_ptr(), 0, KEY_READ, &mut handle) != 0 {
            return out;
        }
        for i in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            let r = RegEnumKeyExW(
                handle,
                i,
                name.as_mut_ptr(),
                &mut len,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            );
            if r != 0 {
                break;
            }
            let sub = String::from_utf16_lossy(&name[..len as usize]);
            if let Some(v) = string_value(handle, &sub, value) {
                out.push(v);
            }
        }
        RegCloseKey(handle);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_of_a_listed_game_counts_by_name_but_runtimes_do_not() {
        let known: HashSet<String> = [
            r"c:\users\me\documents\world of warcraft 3.3.5a\wow.exe",
            r"c:\modrinth\bin\javaw.exe",
            r"c:\users\me\appdata\local\discord\app-1.0\discord.exe",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let names = file_names(&known);
        assert!(names.contains("wow.exe"));
        assert!(!names.contains("javaw.exe") && !names.contains("discord.exe"));
    }

    #[test]
    fn libraries_are_folders_never_drive_roots() {
        for d in libraries() {
            assert!(d.ends_with('\\') && d.matches('\\').count() >= 2, "{d}");
            assert_eq!(d, d.to_lowercase());
        }
    }
}
