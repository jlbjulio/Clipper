use std::collections::VecDeque;
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

/// How often a device that failed to open is tried again.
const RETRY: Duration = Duration::from_secs(10);

/// Keeps the capture streams open, reopening them when devices come and go (e.g. AirPods
/// connect) or a stream loses its device. A device that fails to open (Windows can refuse it
/// for a moment while the default device changes) is retried every 10 s on its own, without
/// interrupting the streams that work; otherwise it would stay silent until the next change.
pub fn spawn(mixer: Mixer) {
    thread::spawn(move || {
        let host = cpal::default_host();
        let lost = Arc::new(AtomicBool::new(false));
        let mut current = Vec::new();
        let mut streams: Vec<cpal::Stream> = Vec::new();
        let mut failed: Vec<(cpal::Device, bool)> = Vec::new();
        let mut retry_at = Instant::now();
        loop {
            // Disabled sources are simply not opened, so toggling one counts as a device change.
            let mut devices: Vec<(cpal::Device, bool)> = Vec::new();
            if mixer.system.load(Ordering::Acquire) {
                let chosen = mixer.output.lock().unwrap().clone();
                devices.extend(
                    system_outputs(&host, &chosen)
                        .into_iter()
                        .map(|d| (d, true)),
                );
            }
            if mixer.mic.load(Ordering::Acquire) {
                let chosen = mixer.input.lock().unwrap().clone();
                devices.extend(microphone(&host, &chosen).map(|d| (d, false)));
            }
            let ids: Vec<_> = devices.iter().map(|(d, _)| d.id().ok()).collect();
            // Read the flag every pass, so a loss that coincides with a device change does not
            // trigger a second rebuild two seconds later.
            let was_lost = lost.swap(false, Ordering::AcqRel);
            if ids != current || was_lost {
                streams.clear();
                failed.clear();
                let mut queues = Vec::new();
                for (dev, loopback) in devices {
                    let q = Queue::default();
                    match open(&dev, loopback, q.clone(), lost.clone()) {
                        Ok(s) => {
                            log(&format!("audio {}: {}", kind(loopback), device_name(&dev)));
                            streams.push(s);
                            queues.push(q);
                        }
                        Err(e) => {
                            log(&format!("audio open failed ({}): {e}", device_name(&dev)));
                            failed.push((dev, loopback));
                        }
                    }
                }
                *mixer.sources.lock().unwrap() = queues;
                current = ids;
                retry_at = Instant::now() + RETRY;
            } else if !failed.is_empty() && Instant::now() >= retry_at {
                failed.retain(|(dev, loopback)| {
                    let q = Queue::default();
                    match open(dev, *loopback, q.clone(), lost.clone()) {
                        Ok(s) => {
                            log(&format!(
                                "audio {} recovered: {}",
                                kind(*loopback),
                                device_name(dev)
                            ));
                            streams.push(s);
                            mixer.sources.lock().unwrap().push(q);
                            false
                        }
                        Err(_) => true,
                    }
                });
                retry_at = Instant::now() + RETRY;
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
