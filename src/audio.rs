use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
}

impl Mixer {
    pub fn new(system: bool, mic: bool) -> Self {
        Self {
            sources: Arc::default(),
            system: Arc::new(AtomicBool::new(system)),
            mic: Arc::new(AtomicBool::new(mic)),
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

/// What to loop back for "system audio". SteelSeries Sonar holds the physical device in
/// exclusive mode, where loopback hears nothing, so when Sonar is present its virtual outputs
/// (Gaming, Chat, Media...) are captured instead; otherwise the default output.
fn system_outputs(host: &cpal::Host) -> Vec<cpal::Device> {
    let name = |d: &cpal::Device| {
        d.description()
            .map(|d| d.name().to_string())
            .unwrap_or_default()
    };
    let sonar: Vec<_> = host
        .output_devices()
        // Sonar's "Microphone" render endpoint carries the processed mic, already captured as input.
        .map(|it| {
            it.filter(|d| name(d).contains("Sonar") && !name(d).contains("Microphone"))
                .collect()
        })
        .unwrap_or_default();
    if sonar.is_empty() {
        host.default_output_device().into_iter().collect()
    } else {
        sonar
    }
}

/// Keeps the capture streams open, reopening them when devices come and go (e.g. AirPods
/// connect) or a stream loses its device.
pub fn spawn(mixer: Mixer) {
    thread::spawn(move || {
        let host = cpal::default_host();
        let lost = Arc::new(AtomicBool::new(false));
        let mut current = Vec::new();
        let mut streams: Vec<cpal::Stream> = Vec::new();
        loop {
            // Disabled sources are simply not opened, so toggling one counts as a device change.
            let mut devices: Vec<(cpal::Device, bool)> = Vec::new();
            if mixer.system.load(Ordering::Acquire) {
                devices.extend(system_outputs(&host).into_iter().map(|d| (d, true)));
            }
            if mixer.mic.load(Ordering::Acquire) {
                devices.extend(host.default_input_device().map(|d| (d, false)));
            }
            let ids: Vec<_> = devices.iter().map(|(d, _)| d.id().ok()).collect();
            if ids != current || lost.swap(false, Ordering::AcqRel) {
                streams.clear();
                let mut queues = Vec::new();
                for (dev, loopback) in &devices {
                    let name = dev
                        .description()
                        .map(|d| d.name().to_string())
                        .unwrap_or_default();
                    let q = Queue::default();
                    match open(dev, *loopback, q.clone(), lost.clone()) {
                        Ok(s) => {
                            log(&format!(
                                "audio {}: {name}",
                                if *loopback { "system" } else { "mic" }
                            ));
                            streams.push(s);
                            queues.push(q);
                        }
                        Err(e) => log(&format!("audio open failed ({name}): {e}")),
                    }
                }
                *mixer.sources.lock().unwrap() = queues;
                current = ids;
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
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
