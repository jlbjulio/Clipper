# Clipper

Instant replay for Windows that stays out of the way. Clipper keeps the last 30 seconds of your screen and sound, and saves them as an MP4 when you press a shortcut. It lives in the notification area and uses about 35 MB of memory and 1% CPU while recording.

- **Save the last 15 to 120 seconds** with `Alt+F10`, take a screenshot with `Alt+F12`, pause with `Alt+F9`. All shortcuts can be changed.
- **Records the monitor you are using.** With several monitors, Clipper follows the focused window and switches about a second after you move to another screen.
- **Game, Discord and microphone audio in one track**, kept in sync with the picture (measured within ±10 ms).
- **Hardware encoding only.** Frames go from the screen to the GPU's video encoder without passing through system memory, so games keep their frame rate.
- **Clips play everywhere:** H.264 + AAC in a standard MP4, ready for Discord, browsers and phones.
- **No services, no overlay, no account.** One 5 MB program. Settings open in a separate window that frees its memory when you close it.

## Requirements

- Windows 10 version 2004 or later, or Windows 11.
- A GPU with a hardware H.264 encoder. Tested on AMD (VCN) and NVIDIA (NVENC); Intel Quick Sync uses the same Windows encoder interface but has not been tested yet.

## Install

1. Download `clipper.exe` from the [releases page](../../releases) and put it anywhere, for example `%LOCALAPPDATA%\Clipper`.
2. Run it. A record icon appears next to the clock and recording starts.
3. Right-click the icon for the menu, or left-click it for settings.

Clipper starts with Windows unless you turn that off in settings. To uninstall, choose **Quit Clipper** from the menu, delete the file, and optionally delete `%APPDATA%\Clipper` (settings and log).

## Where things go

| What | Default location |
| --- | --- |
| Clips | `Videos\Clipper` |
| Screenshots | `Pictures\Clipper` |
| Settings | `%APPDATA%\Clipper\config.ini` (plain text, editable) |
| Log | `%APPDATA%\Clipper\clipper.log` |
| Replay buffer | `%TEMP%\Clipper`, about 40 seconds of video in 10-second files, rewritten as it records |

## Laptops with two graphics cards

On laptops with integrated and dedicated graphics, each monitor is wired to one of them. Clipper's **Automatic** setting records every monitor with the card it is plugged into, which keeps the full frame rate. Recording a monitor attached to an NVIDIA card costs about 300 MB of memory while that monitor is recorded, because of the NVIDIA driver; the integrated card needs about 35 MB. If memory matters more than frame rate on that monitor, choose the integrated card in settings: those monitors are then recorded at about 30 fps.

## How it works

- **Capture:** Windows.Graphics.Capture on a Direct3D 11 device created on the GPU that drives the monitor.
- **Encoding:** a Media Foundation sink writer with the hardware H.264 encoder (variable bitrate, a keyframe every second) and AAC audio. The GPU converts BGRA to NV12 itself.
- **Audio:** WASAPI loopback of the system output and the default microphone through [cpal](https://github.com/RustAudio/cpal), resampled to 48 kHz and mixed on the same clock as the video frames. With SteelSeries Sonar installed, Clipper records Sonar's virtual outputs, because Sonar holds the speakers in exclusive mode.
- **Replay buffer:** short MP4 segments on disk, so memory does not grow with clip length.
- **Saving:** a small built-in MP4 remuxer (`src/mp4.rs`) joins the segments without re-encoding. An edit list makes the clip start exactly at the requested second, even though decoding starts on an earlier keyframe.
- **Settings window:** [egui](https://github.com/emilk/egui), started as a separate process (`clipper.exe --settings`).

## Build

Requires Rust 1.88 or newer with the MSVC toolchain, and the Windows SDK (for the resource compiler).

```powershell
cargo build --release
# target\release\clipper.exe
```

### Tests

```powershell
cargo test --release          # config parsing and the MP4 remuxer
pwsh tests/e2e.ps1            # records real clips on this PC and checks them
```

The end-to-end script needs `ffmpeg` and `ffprobe` on `PATH` for its measurements only. It plays a test video full screen for about three minutes, so leave the PC alone while it runs. It checks clip length on a static and a moving screen, frame rate, bitrate, audio/video sync, screenshots, pause and resume, live settings reload, memory and CPU.

## License

Clipper is free software under the [GNU General Public License v3.0 or later](LICENSE): you can use, study, change and share it, and anything you distribute that is built on it must stay open under the same license.
