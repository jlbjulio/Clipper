use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

use crate::audio::{Mixer, RATE};
use crate::capture::Gpu;
use crate::config::Quality;
use crate::encoder::Encoder;
use crate::log;
use crate::mp4;

const SEG_SECS: i64 = 10;
/// How long to wait before building an encoder again after one failed, so a broken driver
/// costs one attempt a second instead of one per frame.
const RETRY: std::time::Duration = std::time::Duration::from_secs(1);
/// `TimeSpan` units (100 ns) per second.
pub const TICKS: i64 = 10_000_000;

/// The error a clip request gets when there is no recorded history to cut it from.
pub const NOTHING_RECORDED: &str = "nothing recorded yet";

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Params {
    pub clip_secs: u32,
    pub fps: u32,
    pub quality: Quality,
    /// Overrides `quality` when non-zero.
    pub bitrate_kbps: u32,
}

/// What happened to a requested clip, reported back to the tray.
pub type Outcome = Result<(PathBuf, u64), String>;

/// A ready encoder: the GPU it runs on, the encoder, its file and the frame duration.
type Built = (Arc<Gpu>, Encoder, PathBuf, i64);

/// The segment currently being encoded. Timestamps are QPC-based, the same clock as `now_ticks()`.
struct Active {
    gpu: Arc<Gpu>,
    enc: Encoder,
    path: PathBuf,
    frame: i64,
    first: Option<i64>,
    last: i64,
    next_due: i64,
    audio_sent: u64,
    ticked: i64,
}

impl Active {
    fn new((gpu, enc, path, frame): Built) -> Self {
        Self {
            gpu,
            enc,
            path,
            frame,
            first: None,
            last: 0,
            next_due: 0,
            audio_sent: 0,
            ticked: 0,
        }
    }
}

struct Segment {
    path: PathBuf,
    start: i64,
    end: i64,
}

enum Job {
    Finish(Active),
    Save { end: i64, out: PathBuf },
}

/// Records the screen into short MP4 segments on disk and keeps only the last clip's worth,
/// so memory stays flat no matter how long it runs.
pub struct Recorder {
    gpu: Mutex<Arc<Gpu>>,
    dir: PathBuf,
    seq: AtomicU64,
    size: Mutex<(u32, u32)>,
    params: Mutex<Params>,
    clip_secs: Arc<AtomicU64>,
    active: Mutex<Option<Active>>,
    // Built shortly before a rotation so switching segments never stalls the capture thread.
    spare: Mutex<Option<Built>>,
    refilling: AtomicBool,
    /// Bumped whenever the spare is thrown away, so one still being built for the old size,
    /// GPU or settings is not kept.
    generation: AtomicU64,
    /// When building an encoder last failed.
    build_failed: Mutex<Option<std::time::Instant>>,
    /// Off between `stop` and `run`: a frame still in flight from a closed capture session must
    /// not start a new segment.
    running: AtomicBool,
    jobs: Sender<Job>,
}

impl Recorder {
    /// Encoding starts with the first frame, once the size of the captured picture is known.
    pub fn new(
        gpu: Arc<Gpu>,
        dir: PathBuf,
        params: Params,
        on_saved: impl Fn(Outcome) + Send + 'static,
    ) -> Arc<Self> {
        let (jobs, rx) = mpsc::channel();
        let clip_secs = Arc::new(AtomicU64::new(params.clip_secs as u64));
        let secs = clip_secs.clone();
        thread::spawn(move || finisher(rx, &secs, on_saved));
        Arc::new(Self {
            gpu: Mutex::new(gpu),
            dir,
            seq: AtomicU64::new(0),
            size: Mutex::new((0, 0)),
            params: Mutex::new(params),
            clip_secs,
            active: Mutex::new(None),
            spare: Mutex::new(None),
            refilling: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            build_failed: Mutex::new(None),
            running: AtomicBool::new(false),
            jobs,
        })
    }

    fn backing_off(&self) -> bool {
        self.build_failed
            .lock()
            .unwrap()
            .is_some_and(|at| at.elapsed() < RETRY)
    }

    fn new_encoder(&self) -> Result<Built, BoxError> {
        if self.backing_off() {
            return Err("the last encoder setup failed; retrying shortly".into());
        }
        let gpu = self.gpu.lock().unwrap().clone();
        let path = self.dir.join(format!(
            "seg_{}.mp4",
            self.seq.fetch_add(1, Ordering::Relaxed)
        ));
        let (w, h) = *self.size.lock().unwrap();
        let mut p = *self.params.lock().unwrap();
        if gpu.software || !crate::capture::hardware_encoder() {
            // CPU encoding: keep it light enough to leave the game its processor.
            p.fps = p.fps.min(30);
            p.quality = Quality::Low;
            p.bitrate_kbps = 0;
        }
        let bitrate = if p.bitrate_kbps > 0 {
            p.bitrate_kbps * 1000
        } else {
            p.quality.bitrate(w, h, p.fps)
        };
        let enc = Encoder::new(&gpu, &path, w, h, p.fps, bitrate).inspect_err(|_| {
            *self.build_failed.lock().unwrap() = Some(std::time::Instant::now());
            // The sink writer may have created the file before a later step failed.
            let _ = fs::remove_file(&path);
        })?;
        Ok((gpu, enc, path, TICKS / p.fps as i64))
    }

    /// Throws away the spare encoder, built for an old size, GPU or setting, along with any
    /// spare still being built, and deletes its unused file.
    fn discard_spare(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some((_, enc, path, _)) = self.spare.lock().unwrap().take() {
            drop(enc);
            let _ = fs::remove_file(path);
        }
    }

    fn refill_spare(self: &Arc<Self>) {
        if self.spare.lock().unwrap().is_some()
            || self.backing_off()
            || self.refilling.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let me = self.clone();
        let generation = self.generation.load(Ordering::Acquire);
        thread::spawn(move || {
            match me.new_encoder() {
                Ok(e) => {
                    let mut spare = me.spare.lock().unwrap();
                    // Kept only if nothing changed the size, GPU or settings meanwhile.
                    if me.generation.load(Ordering::Acquire) == generation {
                        *spare = Some(e);
                    } else {
                        drop(spare);
                        let (_, enc, path, _) = e;
                        drop(enc);
                        let _ = fs::remove_file(path);
                    }
                }
                Err(e) => log(&format!("encoder setup failed: {e}")),
            }
            me.refilling.store(false, Ordering::Release);
        });
    }

    fn finish(&self, old: Option<Active>) {
        if let Some(old) = old {
            let _ = self.jobs.send(Job::Finish(old));
        }
    }

    /// Closes the running segment (it gets finalized in the background) and starts a new one.
    pub fn cut(self: &Arc<Self>) {
        let spare = self.spare.lock().unwrap().take();
        let next = match spare.map_or_else(|| self.new_encoder(), Ok) {
            Ok(e) => Some(Active::new(e)),
            Err(e) => {
                log(&format!("encoder setup failed: {e}"));
                None
            }
        };
        let old = std::mem::replace(&mut *self.active.lock().unwrap(), next);
        self.finish(old);
    }

    /// Accepts frames again; the next one starts a segment.
    pub fn run(&self) {
        self.running.store(true, Ordering::Release);
    }

    /// Stops encoding (pause, or nothing to record); the recorded history stays available for
    /// a clip.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
        self.discard_spare();
        let old = self.active.lock().unwrap().take();
        self.finish(old);
    }

    /// Matches the encoder to the captured picture and starts encoding if it was stopped.
    fn set_size(self: &Arc<Self>, size: (u32, u32)) {
        let changed = std::mem::replace(&mut *self.size.lock().unwrap(), size) != size;
        if changed || self.active.lock().unwrap().is_none() {
            // A clip never spans two sizes: mp4::join keeps the run that covers most of it.
            self.discard_spare();
            self.cut();
        }
    }

    /// Moves encoding to another GPU; call it before starting a capture session on that GPU.
    pub fn set_gpu(self: &Arc<Self>, gpu: Arc<Gpu>) {
        if Arc::ptr_eq(&self.gpu.lock().unwrap(), &gpu) {
            return;
        }
        *self.gpu.lock().unwrap() = gpu;
        self.discard_spare();
        if self.active.lock().unwrap().is_some() {
            self.cut();
        }
    }

    /// Applies new recording settings from the next segment on.
    pub fn set_params(self: &Arc<Self>, p: Params) {
        self.clip_secs.store(p.clip_secs as u64, Ordering::Release);
        let changed = std::mem::replace(&mut *self.params.lock().unwrap(), p) != p;
        if changed && self.active.lock().unwrap().is_some() {
            self.discard_spare();
            self.cut();
        }
    }

    /// Saves the last `clip_secs` seconds ending at `end` to `out`.
    pub fn save(self: &Arc<Self>, end: i64, out: PathBuf) {
        if self.active.lock().unwrap().is_some() {
            self.cut();
        }
        let _ = self.jobs.send(Job::Save { end, out });
    }

    /// Encodes the `size` picture in the top-left corner of `tex`. The first frame, or one of
    /// another size (a resized window), starts a segment with an encoder of that size.
    pub fn on_frame(self: &Arc<Self>, tex: &ID3D11Texture2D, ts: i64, size: (u32, u32)) {
        if !self.running.load(Ordering::Acquire) {
            return;
        }
        if *self.size.lock().unwrap() != size || self.active.lock().unwrap().is_none() {
            self.set_size(size);
        }
        let mut guard = self.active.lock().unwrap();
        let age = guard.as_ref().and_then(|a| a.first).map_or(0, |f| ts - f);
        if age >= (SEG_SECS - 2) * TICKS {
            self.refill_spare();
        }
        if age >= SEG_SECS * TICKS
            && let Some(spare) = self.spare.lock().unwrap().take()
        {
            let old = guard.replace(Active::new(spare));
            self.finish(old);
        }
        let Some(a) = guard.as_mut() else { return };
        // Cap the frame rate on high-refresh monitors. Half a frame of slack keeps content that a
        // 144 Hz display presents at uneven 14/21 ms intervals; the average stays on target.
        if a.first.is_some() && ts < a.next_due - a.frame / 2 {
            return;
        }
        a.next_due = if ts > a.next_due + a.frame {
            ts + a.frame
        } else {
            a.next_due + a.frame
        };
        let first = *a.first.get_or_insert(ts);
        a.last = ts;
        let res = a.enc.video(&a.gpu, tex, ts - first);
        if let Err(e) = res {
            log(&format!("video encode failed: {e}"));
        }
    }

    /// Feeds the segment the audio due up to `now`, on the same clock as the video frames.
    pub fn pump_audio(&self, now: i64, mixer: &Mixer) {
        let mut guard = self.active.lock().unwrap();
        let Some(a) = guard.as_mut() else { return };
        let Some(first) = a.first else { return };
        // Frames the encoder finished since the last one went in, before any gap is marked.
        if let Err(e) = a.enc.collect() {
            log(&format!("video encode failed: {e}"));
        }
        let due = ((now - first).max(0) as i128 * RATE as i128 / TICKS as i128) as u64;
        if due <= a.audio_sent {
            return;
        }
        let pcm = mixer.take((due - a.audio_sent) as usize);
        let time = (a.audio_sent as i128 * TICKS as i128 / RATE as i128) as i64;
        a.audio_sent = due;
        // Mark the video gap while the screen is static; 100 ms of slack keeps late frames in order.
        let quiet_until = now - TICKS / 10;
        if quiet_until > a.last + a.frame && now - a.ticked >= TICKS / 4 {
            a.ticked = now;
            if let Err(e) = a.enc.video_gap(quiet_until - first) {
                log(&format!("video gap failed: {e}"));
            }
        }
        if let Err(e) = a.enc.audio(&pcm, time) {
            log(&format!("audio encode failed: {e}"));
        }
    }
}

fn finisher(rx: Receiver<Job>, clip_secs: &AtomicU64, on_saved: impl Fn(Outcome)) {
    let mut segs: VecDeque<Segment> = VecDeque::new();
    for job in rx {
        let keep = clip_secs.load(Ordering::Acquire) as i64 * TICKS;
        match job {
            Job::Finish(a) => {
                let (path, first, last) = (a.path, a.first, a.last);
                // A static screen yields no new frames, but audio keeps running up to the cut.
                let heard = (a.audio_sent as i128 * TICKS as i128 / RATE as i128) as i64;
                match (a.enc.finish(), first) {
                    (Ok(()), Some(start)) => segs.push_back(Segment {
                        path,
                        start,
                        end: (last + a.frame).max(start + heard),
                    }),
                    (r, _) => {
                        if let Err(e) = r {
                            log(&format!("segment finalize failed: {e}"));
                        }
                        let _ = fs::remove_file(path);
                    }
                }
                // Drop the oldest segment once the rest still covers a full clip.
                while segs.len() > 1 && segs[segs.len() - 1].end - segs[1].start >= keep + TICKS {
                    let _ = fs::remove_file(segs.pop_front().unwrap().path);
                }
            }
            Job::Save { end, out } => {
                let from = end - keep;
                let sources: Vec<mp4::Source> = segs
                    .iter()
                    .filter(|s| s.end > from)
                    .map(|s| mp4::Source {
                        path: s.path.clone(),
                        start: s.start,
                    })
                    .collect();
                let result = if sources.is_empty() {
                    Err(NOTHING_RECORDED.to_string())
                } else {
                    let _ = out.parent().map(fs::create_dir_all);
                    mp4::join(&sources, from, &out)
                        .and_then(|()| Ok((out.clone(), fs::metadata(&out)?.len())))
                        .map_err(|e| e.to_string())
                };
                match &result {
                    Ok((p, _)) => log(&format!("clip saved: {}", p.display())),
                    Err(e) => log(&format!("clip failed: {e}")),
                }
                on_saved(result);
            }
        }
    }
}
