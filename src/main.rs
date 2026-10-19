#![windows_subsystem = "windows"]

mod amf;
mod app;
mod audio;
mod capture;
mod config;
mod encoder;
mod games;
mod hotkeys;
mod mp4;
mod nvenc;
mod overlay;
mod recorder;
mod settings;
mod text;
mod tray;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

static LOG: OnceLock<PathBuf> = OnceLock::new();
/// The argument Windows passes when it restarts Clipper after a crash, followed by how many
/// restarts in a row this is.
const RESTARTED: &str = "--restarted";
const MAX_RESTARTS: u32 = 3;

/// How many crash restarts in a row led to this run: 0 for a normal start.
fn restarts(args: &[String]) -> u32 {
    args.iter().position(|a| a == RESTARTED).map_or(0, |i| {
        args.get(i + 1).and_then(|n| n.parse().ok()).unwrap_or(1)
    })
}

/// Appends a line to the log. A message repeated within a second is skipped, so an error that
/// recurs every frame costs one file write a second, not sixty.
pub fn log(msg: &str) {
    static LAST: std::sync::Mutex<Option<(String, std::time::Instant)>> =
        std::sync::Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap();
        if let Some((text, at)) = &*last
            && text == msg
            && at.elapsed() < std::time::Duration::from_secs(1)
        {
            return;
        }
        *last = Some((msg.to_string(), std::time::Instant::now()));
    }
    if let Some(path) = LOG.get()
        && let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = writeln!(f, "{} {msg}", stamp());
    }
}

/// Local time as `2026-10-16 22-05-08`, safe for file names.
pub fn stamp() -> String {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!(
        "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// QPC time in 100 ns units: the clock Windows.Graphics.Capture stamps frames with.
pub fn now_ticks() -> i64 {
    let (mut count, mut freq) = (0i64, 0i64);
    unsafe {
        QueryPerformanceCounter(&mut count);
        QueryPerformanceFrequency(&mut freq);
    }
    (count as i128 * 10_000_000 / freq as i128) as i64
}

fn main() {
    let dir = config::dir();
    let _ = fs::create_dir_all(&dir);
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--settings") {
        settings::run();
        return;
    }
    let log_path = dir.join("clipper.log");
    let restarts = restarts(&args);
    // Keep one previous run's log for bug reports; never let it grow without bound. After a
    // crash restart the log carries on, so the crash stays in it.
    if restarts == 0 {
        let _ = fs::rename(&log_path, dir.join("clipper.old.log"));
    }
    let _ = LOG.set(log_path);
    // Release builds abort on panic; leave the reason in the log for bug reports.
    std::panic::set_hook(Box::new(|info| log(&format!("crash: {info}"))));
    if restarts > 0 {
        log(&format!(
            "Clipper stopped unexpectedly and Windows restarted it ({restarts} in a row)"
        ));
    }
    // If Clipper ever crashes or hangs (a GPU driver fault included) after running a minute,
    // Windows starts it again instead of leaving the user without replays, up to three times in
    // a row so a fault that keeps coming back does not loop forever. Not after updates or
    // reboots: start with Windows already covers those.
    if restarts < MAX_RESTARTS {
        let restart: Vec<u16> = format!("{RESTARTED} {}", restarts + 1)
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            use windows_sys::Win32::System::Recovery::{
                RESTART_NO_PATCH, RESTART_NO_REBOOT, RegisterApplicationRestart,
            };
            RegisterApplicationRestart(restart.as_ptr(), RESTART_NO_PATCH | RESTART_NO_REBOOT);
        }
    }
    capture::yield_gpu();
    app::run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_restarts_are_counted_from_the_arguments() {
        let args = |s: &[&str]| s.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(restarts(&args(&[])), 0);
        assert_eq!(restarts(&args(&["--restarted"])), 1);
        assert_eq!(restarts(&args(&["--restarted", "2"])), 2);
        assert_eq!(restarts(&args(&["--restarted", "junk"])), 1);
    }
}
