use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample};

use crate::log;

pub const RATE: u32 = 48_000;
/// Frames a source may hold beyond what the pump asks for; older audio is dropped to keep latency low.
const JITTER: usize = RATE as usize / 25;

type Queue = Arc<Mutex<VecDeque<[f32; 2]>>>;

/// Every capture stream (system loopbacks + microphone), resampled to 48 kHz stereo.
#[derive(Clone)]
pub struct Mixer {
    sources: Arc<Mutex<Vec<Queue>>>,
    pub system: Arc<AtomicBool>,
    pub mic: Arc<AtomicBool>,
    /// Chosen devices by name; empty means automatic.
    pub output: Arc<Mutex<String>>,
    pub input: Arc<Mutex<String>>,
}

impl Mixer {
    pub fn new(system: bool, mic: bool, output: &str, input: &str) -> Self {
        Self {
            sources: Arc::default(),
            system: Arc::new(AtomicBool::new(system)),
            mic: Arc::new(AtomicBool::new(mic)),
            output: Arc::new(Mutex::new(output.to_string())),
            input: Arc::new(Mutex::new(input.to_string())),
        }
    }

    /// Mixes exactly `frames` stereo frames into 16-bit PCM, filling gaps with silence
    /// (WASAPI loopback delivers nothing while the system is quiet).
    pub fn take(&self, frames: usize) -> Vec<u8> {
        let mut mix = vec![[0f32; 2]; frames];
        for q in self.sources.lock().unwrap().iter() {
            let mut q = q.lock().unwrap();
            let excess = q.len().saturating_sub(frames + JITTER);
            q.drain(..excess);
            let n = frames.min(q.len());
            for (m, s) in mix.iter_mut().zip(q.drain(..n)) {
                m[0] += s[0];
                m[1] += s[1];
            }
        }
        mix.iter()
            .flatten()
            .flat_map(|s| ((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes())
            .collect()
    }
}

pub fn device_name(d: &cpal::Device) -> String {
    d.description()
        .map(|d| d.name().to_string())
        .unwrap_or_default()
}

/// Names of the playback and recording devices, for the settings window.
pub fn device_names() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let names =
        |it: Option<Vec<cpal::Device>>| it.unwrap_or_default().iter().map(device_name).collect();
    (
        names(host.output_devices().ok().map(|i| i.collect())),
        names(host.input_devices().ok().map(|i| i.collect())),
    )
}

/// What to loop back for "system audio". A chosen device wins while it is connected.
/// Otherwise: when the Windows default output is one of SteelSeries Sonar's virtual devices,
/// all of Sonar's outputs (Gaming, Chat, Media...), since apps are spread across them and Sonar
/// holds the real speakers in exclusive mode where loopback hears nothing. In every other case
/// the default output, which is where apps play.
fn system_outputs(host: &cpal::Host, chosen: &str) -> Vec<cpal::Device> {
    let outputs: Vec<cpal::Device> = host
        .output_devices()
        .map(|i| i.collect())
        .unwrap_or_default();
    if let Some(d) = outputs
        .iter()
        .find(|d| !chosen.is_empty() && device_name(d) == chosen)
    {
        return vec![d.clone()];
    }
    let default = host.default_output_device();
    if default
        .as_ref()
        .is_some_and(|d| device_name(d).contains("Sonar"))
    {
        // Sonar's "Microphone" output carries the processed mic, already captured as input.
        let sonar = |d: &&cpal::Device| {
            device_name(d).contains("Sonar") && !device_name(d).contains("Microphone")
        };
        return outputs.iter().filter(sonar).cloned().collect();
    }
    default.into_iter().collect()
}

/// The microphone: the chosen one while connected, otherwise the Windows default.
fn microphone(host: &cpal::Host, chosen: &str) -> Option<cpal::Device> {
    let chosen_device = (!chosen.is_empty())
        .then(|| {
            host.input_devices()
                .ok()?
                .find(|d| device_name(d) == chosen)
        })
        .flatten();
    chosen_device.or_else(|| host.default_input_device())
}

/// How often a source that failed to open is tried again.
const RETRY: Duration = Duration::from_secs(10);

/// What the audio thread records.
enum Source {
    /// A playback device through WASAPI loopback (`true`), or a microphone (`false`).
    Device(cpal::Device, bool),
    /// Every program's audio straight from Windows (per-process loopback), except the process
    /// tree of `exclude`. Not tied to any playback device, so headphones coming and going
    /// never interrupt it, and it opens no connection on a mixer's channels.
    Apps { exclude: u32 },
}

impl Source {
    /// Stable while the source is the same device, so unchanged sources stay open.
    fn id(&self) -> String {
        match self {
            Source::Device(d, loopback) => format!("{}:{:?}", kind(*loopback), d.id().ok()),
            Source::Apps { exclude } => format!("apps:{exclude}"),
        }
    }

    fn describe(&self) -> String {
        match self {
            Source::Device(d, loopback) => format!("{} {}", kind(*loopback), device_name(d)),
            Source::Apps { .. } => "system: every program".to_string(),
        }
    }

    fn open(&self, q: Queue, lost: Arc<AtomicBool>) -> Result<Handle, String> {
        match self {
            Source::Device(d, loopback) => open(d, *loopback, q, lost)
                .map(Handle::Device)
                .map_err(|e| e.to_string()),
            Source::Apps { exclude } => apps::start(*exclude, q, lost).map(Handle::Apps),
        }
    }
}

/// Keeps a source recording until dropped.
enum Handle {
    Device(#[allow(dead_code)] cpal::Stream),
    Apps(#[allow(dead_code)] apps::Capture),
}

/// SteelSeries Sonar's engine, which plays the processed microphone into Sonar's virtual
/// "Microphone" output; recording it too would put the voice in clips twice.
fn sonar_engine() -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        let mut more = Process32FirstW(snap, &mut entry) != 0;
        while more {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            if String::from_utf16_lossy(&entry.szExeFile[..len])
                .eq_ignore_ascii_case("SteelSeriesSonar.exe")
            {
                found = Some(entry.th32ProcessID);
                break;
            }
            more = Process32NextW(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
        found
    }
}

/// Keeps the capture streams open. Each source is opened and closed on its own, so a
/// microphone change never interrupts the system audio. A source that fails to open (Windows
/// can refuse one for a moment while the default device changes) is retried every 10 s.
pub fn spawn(mixer: Mixer) {
    thread::spawn(move || {
        let host = cpal::default_host();
        let lost = Arc::new(AtomicBool::new(false));
        let mut open: Vec<(String, Handle, Queue)> = Vec::new();
        let mut retry: HashMap<String, Instant> = HashMap::new();
        // Per-process loopback needs Windows 11 (or Windows 10 build 20348); without it the
        // playback devices are recorded as before.
        let mut apps_unavailable = false;
        loop {
            let mut wanted: Vec<Source> = Vec::new();
            if mixer.system.load(Ordering::Acquire) {
                let chosen = mixer.output.lock().unwrap().clone();
                if chosen.is_empty() && !apps_unavailable {
                    let exclude = sonar_engine().unwrap_or_else(std::process::id);
                    wanted.push(Source::Apps { exclude });
                } else {
                    wanted.extend(
                        system_outputs(&host, &chosen)
                            .into_iter()
                            .map(|d| Source::Device(d, true)),
                    );
                }
            }
            if mixer.mic.load(Ordering::Acquire) {
                let chosen = mixer.input.lock().unwrap().clone();
                wanted.extend(microphone(&host, &chosen).map(|d| Source::Device(d, false)));
            }
            // A source lost its device: reopen everything.
            let mut changed = false;
            if lost.swap(false, Ordering::AcqRel) {
                open.clear();
                changed = true;
            }
            let ids: Vec<String> = wanted.iter().map(Source::id).collect();
            let before = open.len();
            open.retain(|(id, _, _)| ids.contains(id));
            changed |= open.len() != before;
            retry.retain(|id, _| ids.contains(id));
            for (source, id) in wanted.iter().zip(ids) {
                if open.iter().any(|(o, _, _)| *o == id)
                    || retry.get(&id).is_some_and(|at| Instant::now() < *at)
                {
                    continue;
                }
                let q = Queue::default();
                match source.open(q.clone(), lost.clone()) {
                    Ok(handle) => {
                        log(&format!("audio {}", source.describe()));
                        retry.remove(&id);
                        open.push((id, handle, q));
                        changed = true;
                    }
                    Err(e) if matches!(source, Source::Apps { .. }) => {
                        log(&format!(
                            "per-program audio capture unavailable ({e}); recording playback devices"
                        ));
                        apps_unavailable = true;
                    }
                    Err(e) => {
                        // Logged once per source; retries stay quiet until one works.
                        if !retry.contains_key(&id) {
                            log(&format!("audio open failed ({}): {e}", source.describe()));
                        }
                        retry.insert(id, Instant::now() + RETRY);
                    }
                }
            }
            if changed {
                *mixer.sources.lock().unwrap() = open.iter().map(|(_, _, q)| q.clone()).collect();
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}

fn kind(loopback: bool) -> &'static str {
    if loopback { "system" } else { "mic" }
}

fn open(
    dev: &cpal::Device,
    loopback: bool,
    q: Queue,
    lost: Arc<AtomicBool>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    // Opening an input stream on an output device puts WASAPI in loopback mode.
    let cfg = if loopback {
        dev.default_output_config()?
    } else {
        dev.default_input_config()?
    };
    let (ch, rate, fmt) = (
        cfg.channels() as usize,
        cfg.sample_rate(),
        cfg.sample_format(),
    );
    let config = cfg.config();
    let on_err = move |e: cpal::Error| {
        // Xruns are harmless glitches; only a vanished device needs the stream rebuilt.
        if matches!(
            e.kind(),
            ErrorKind::DeviceNotAvailable | ErrorKind::DeviceChanged | ErrorKind::StreamInvalidated
        ) {
            log(&format!("audio device lost: {e}"));
            lost.store(true, Ordering::Release);
        }
    };
    let stream = match fmt {
        SampleFormat::F32 => build::<f32>(dev, config, ch, rate, q, on_err)?,
        SampleFormat::I16 => build::<i16>(dev, config, ch, rate, q, on_err)?,
        SampleFormat::I32 => build::<i32>(dev, config, ch, rate, q, on_err)?,
        f => return Err(format!("unsupported sample format {f:?}").into()),
    };
    stream.play()?;
    Ok(stream)
}

fn build<T>(
    dev: &cpal::Device,
    config: cpal::StreamConfig,
    ch: usize,
    rate: u32,
    q: Queue,
    on_err: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut rs = Resampler::new(rate);
    dev.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            let mut q = q.lock().unwrap();
            for f in data.chunks_exact(ch) {
                let l = f32::from_sample_(f[0]);
                let r = if ch > 1 { f32::from_sample_(f[1]) } else { l };
                rs.push([l, r], &mut q);
            }
            let excess = q.len().saturating_sub(RATE as usize / 5);
            q.drain(..excess);
        },
        on_err,
        None,
    )
}

/// Linear resampler to 48 kHz; good enough for game audio and voice, and allocation-free.
struct Resampler {
    step: f64,
    pos: f64,
    prev: [f32; 2],
}

impl Resampler {
    fn new(rate: u32) -> Self {
        Self {
            step: rate as f64 / RATE as f64,
            pos: 0.0,
            prev: [0.0; 2],
        }
    }

    fn push(&mut self, cur: [f32; 2], out: &mut VecDeque<[f32; 2]>) {
        while self.pos < 1.0 {
            let t = self.pos as f32;
            out.push_back([
                self.prev[0] + (cur[0] - self.prev[0]) * t,
                self.prev[1] + (cur[1] - self.prev[1]) * t,
            ]);
            self.pos += self.step;
        }
        self.pos -= 1.0;
        self.prev = cur;
    }
}

/// Windows' per-process loopback: the audio every program plays, mixed by Windows itself.
mod apps {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::core::{IUnknown, Interface, implement, w};

    use super::{Queue, RATE};

    /// AUDIOCLIENT_ACTIVATION_PARAMS for AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK.
    #[repr(C)]
    struct ActivationParams {
        kind: i32,
        target_pid: u32,
        mode: i32,
    }
    const PROCESS_LOOPBACK: i32 = 1;
    const EXCLUDE_TARGET_TREE: i32 = 1;

    /// A PROPVARIANT holding a VT_BLOB, laid out as the Windows headers declare it.
    #[repr(C)]
    struct BlobVariant {
        vt: u16,
        reserved: [u16; 3],
        size: u32,
        data: *const ActivationParams,
    }
    const VT_BLOB: u16 = 65;

    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct Activated(mpsc::Sender<windows::core::Result<IUnknown>>);

    impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
        fn ActivateCompleted(
            &self,
            op: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            let result = (|| unsafe {
                let (mut hr, mut itf) = (windows::core::HRESULT::default(), None);
                op.ok()?.GetActivateResult(&mut hr, &mut itf)?;
                hr.ok()?;
                itf.ok_or_else(|| windows::core::Error::from_hresult(AUDCLNT_E_DEVICE_INVALIDATED))
            })();
            let _ = self.0.send(result);
            Ok(())
        }
    }

    /// 48 kHz stereo 16-bit; Windows converts whatever the programs play.
    fn format() -> WAVEFORMATEX {
        WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        }
    }

    fn activate(exclude: u32) -> windows::core::Result<(IAudioClient, IAudioCaptureClient)> {
        let params = ActivationParams {
            kind: PROCESS_LOOPBACK,
            target_pid: exclude,
            mode: EXCLUDE_TARGET_TREE,
        };
        let blob = BlobVariant {
            vt: VT_BLOB,
            reserved: [0; 3],
            size: size_of::<ActivationParams>() as u32,
            data: &params,
        };
        let (tx, rx) = mpsc::channel();
        let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
        unsafe {
            ActivateAudioInterfaceAsync(
                w!(r"VAD\Process_Loopback"),
                &IAudioClient::IID,
                Some(&blob as *const BlobVariant as *const PROPVARIANT),
                &handler,
            )?;
            let client: IAudioClient = rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| windows::core::Error::from_hresult(AUDCLNT_E_DEVICE_INVALIDATED))??
                .cast()?;
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                2_000_000,
                0,
                &format(),
                None,
            )?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok((client, capture))
        }
    }

    /// The recording thread; stops and joins when dropped.
    pub struct Capture {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// Starts recording every program except `exclude`'s process tree into `q`.
    pub fn start(exclude: u32, q: Queue, lost: Arc<AtomicBool>) -> Result<Capture, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let quit = stop.clone();
        let thread = thread::spawn(move || {
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let (client, capture) = match activate(exclude) {
                Ok(c) => {
                    let _ = ready_tx.send(Ok(()));
                    c
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            while !quit.load(Ordering::Acquire) {
                if let Err(e) = drain(&capture, &q) {
                    crate::log(&format!("audio device lost: {e}"));
                    lost.store(true, Ordering::Release);
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = unsafe { client.Stop() };
        });
        ready_rx
            .recv()
            .map_err(|_| "capture thread ended".to_string())??;
        Ok(Capture {
            stop,
            thread: Some(thread),
        })
    }

    /// Moves every captured packet into `q` as stereo floats.
    fn drain(capture: &IAudioCaptureClient, q: &Queue) -> windows::core::Result<()> {
        unsafe {
            while capture.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let mut queue = q.lock().unwrap();
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    queue.extend(std::iter::repeat_n([0f32; 2], frames as usize));
                } else {
                    let pcm = std::slice::from_raw_parts(data as *const i16, frames as usize * 2);
                    queue.extend(
                        pcm.as_chunks::<2>()
                            .0
                            .iter()
                            .map(|s| [s[0] as f32 / 32768.0, s[1] as f32 / 32768.0]),
                    );
                }
                let excess = queue.len().saturating_sub(RATE as usize / 5);
                queue.drain(..excess);
                drop(queue);
                capture.ReleaseBuffer(frames)?;
            }
        }
        Ok(())
    }
}
