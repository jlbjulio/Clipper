#![windows_subsystem = "windows"]

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

pub fn log(msg: &str) {
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
    // Keep one previous run's log for bug reports; never let it grow without bound.
    let _ = fs::rename(&log_path, dir.join("clipper.old.log"));
    let _ = LOG.set(log_path);
    // Release builds abort on panic; leave the reason in the log for bug reports.
    std::panic::set_hook(Box::new(|info| log(&format!("crash: {info}"))));
    app::run();
}
