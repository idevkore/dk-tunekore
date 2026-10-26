//! Sampling from an audio input: a [`Recorder`] opens a cpal input stream and
//! records until it is stopped.
//!
//! The input callback converts each block to `f32` in a scratch buffer made
//! before the stream starts, tracks the peak for a meter, and pushes the
//! samples into an `rtrb` ring buffer. It never allocates, locks or does I/O.
//! A worker thread drains the ring into one `Vec` per channel while
//! recording, up to [`MAX_SECONDS`].
//!
//! The guard against a panicking callback, the "owed silence" that keeps a
//! take's timing after an overrun, the NaN guard and the stream-lost check
//! are adapted from soundCraft (github.com/storytold/soundcraft,
//! crates/playback/src/record.rs), MIT OR Apache-2.0, Copyright (c) 2026
//! ArtCraft Team and the SoundCraft contributors. soundCraft's ring is
//! atomics-only and its writer streams to disk; ours is `rtrb` and the take
//! stays in memory.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A take stops growing after this long.
pub const MAX_SECONDS: usize = 10 * 60;

/// The ring holds this much audio between the callback and the worker.
const RING_SECONDS: usize = 4;
/// How often the worker drains the ring.
const DRAIN_EVERY: Duration = Duration::from_millis(10);
/// An input that sends nothing for this long counts as gone.
const STALL: Duration = Duration::from_secs(2);
/// The callback converts at most this many frames at a time.
const BLOCK_FRAMES: usize = 8192;

const GONE: u8 = 1;
const BROKEN: u8 = 2;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InputDevice {
    /// cpal's device id, stable across runs.
    pub id: String,
    pub name: String,
    pub channels: u16,
    pub preferred_rate: u32,
    pub is_default: bool,
}

/// Lists the audio inputs, like `Engine::list_outputs` lists outputs. Needs
/// no microphone permission: it only reads device names.
pub fn list_inputs() -> Result<Vec<InputDevice>, String> {
    let host = cpal::default_host();
    let default_id = host.default_input_device().and_then(|d| d.id().ok());
    let mut out = Vec::new();
    for dev in host.input_devices().map_err(explain)? {
        let Ok(id) = dev.id() else { continue };
        let Ok(config) = dev.default_input_config() else { continue };
        if config.channels() == 0 {
            continue;
        }
        let name = dev.description().map(|d| d.name().to_string()).unwrap_or_else(|_| id.to_string());
        out.push(InputDevice {
            is_default: default_id.as_ref() == Some(&id),
            id: id.to_string(),
            name,
            channels: config.channels(),
            preferred_rate: config.sample_rate(),
        });
    }
    Ok(out)
}

/// A cpal error as a sentence for the person using the app.
fn explain(e: cpal::Error) -> String {
    if e.kind() == cpal::ErrorKind::PermissionDenied {
        "tuneKore may not use the microphone. Allow it in System Settings > Privacy & Security > Microphone, then try again.".into()
    } else {
        e.to_string()
    }
}

// ---- The audio callback's side ----

/// What the callback and the control side both see.
#[derive(Default)]
struct Shared {
    /// Largest sample magnitude since the last take, as f32 bits. Magnitudes
    /// are never negative, so the bits sort like the numbers.
    peak: AtomicU32,
    /// Samples lost while the ring was full.
    dropped: AtomicUsize,
    /// Callbacks so far, to tell an input that stopped sending.
    beats: AtomicU64,
    /// Set when the stream can't go on: `GONE` or `BROKEN`.
    fault: AtomicU8,
    /// Frames the worker has taken so far.
    frames: AtomicUsize,
    /// The take reached [`MAX_SECONDS`].
    full: AtomicBool,
}

/// The callback's end of the ring.
struct Capture {
    ring: rtrb::Producer<f32>,
    /// Samples that didn't fit, queued as silence first so the take keeps its timing.
    owed: usize,
}

impl Capture {
    /// Queues `data` (interleaved). Never allocates or blocks. Returns how
    /// many samples didn't fit.
    fn push(&mut self, data: &[f32]) -> usize {
        if self.owed > 0 {
            let n = self.owed.min(self.ring.slots());
            if n > 0 {
                if let Ok(chunk) = self.ring.write_chunk_uninit(n) {
                    self.owed -= chunk.fill_from_iter(std::iter::repeat_n(0.0f32, n));
                }
            }
        }
        let mut kept = 0;
        if self.owed == 0 {
            let n = data.len().min(self.ring.slots());
            if n > 0 {
                if let Ok(chunk) = self.ring.write_chunk_uninit(n) {
                    kept = chunk.fill_from_iter(data[..n].iter().copied());
                }
            }
        }
        let lost = data.len() - kept;
        self.owed += lost;
        lost
    }
}

/// An input sample, or silence for NaN and infinities, which would poison the take.
fn finite(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

fn raise_peak(peak: &AtomicU32, v: f32) {
    peak.fetch_max(v.to_bits(), Ordering::Relaxed);
}

// ---- The control side ----

/// One take's audio so far, one `Vec` per channel.
pub struct Take {
    channels: Vec<Vec<f32>>,
    /// The start of a frame a drain ended in the middle of.
    carry: Vec<f32>,
    max_frames: usize,
}

impl Take {
    pub fn new(channels: usize, max_frames: usize) -> Self {
        Self { channels: vec![Vec::new(); channels.max(1)], carry: Vec::new(), max_frames }
    }

    pub fn frames(&self) -> usize {
        self.channels[0].len()
    }

    /// Whether the take has reached its cap and ignores more input.
    pub fn is_full(&self) -> bool {
        self.frames() >= self.max_frames
    }

    /// Adds interleaved samples, splitting them by channel. A drain can end
    /// mid-frame: the rest is kept for the next one, so channels never shift.
    pub fn feed(&mut self, samples: &[f32]) {
        let n = self.channels.len();
        let mut samples = samples;
        if !self.carry.is_empty() {
            let need = (n - self.carry.len()).min(samples.len());
            self.carry.extend_from_slice(&samples[..need]);
            samples = &samples[need..];
            if self.carry.len() < n {
                return;
            }
            if !self.is_full() {
                for (c, &s) in self.carry.iter().enumerate() {
                    self.channels[c].push(s);
                }
            }
            self.carry.clear();
        }
        let room = self.max_frames.saturating_sub(self.frames());
        let whole = samples.len() / n;
        for frame in samples.chunks_exact(n).take(room.min(whole)) {
            for (c, &s) in frame.iter().enumerate() {
                self.channels[c].push(s);
            }
        }
        // A frame cut by the end of this block waits for the next block.
        self.carry.extend_from_slice(&samples[whole * n..]);
    }

    /// The audio, one `Vec` per channel.
    pub fn into_channels(self) -> Vec<Vec<f32>> {
        self.channels
    }
}

/// Moves everything in the ring into the take. Returns the samples moved.
fn drain(ring: &mut rtrb::Consumer<f32>, take: &mut Take) -> usize {
    let n = ring.slots();
    if n == 0 {
        return 0;
    }
    let Ok(chunk) = ring.read_chunk(n) else { return 0 };
    let (a, b) = chunk.as_slices();
    take.feed(a);
    take.feed(b);
    chunk.commit_all();
    n
}

/// Reads the input level from any thread.
#[derive(Clone)]
pub struct InputMeter(Arc<Shared>);

impl InputMeter {
    /// Peak input level since the last call, 0..1 (over 1 when the input clips).
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.0.peak.swap(0, Ordering::Relaxed))
    }

    /// Seconds recorded so far, given the take's sample rate.
    pub fn seconds(&self, rate: u32) -> f32 {
        self.0.frames.load(Ordering::Relaxed) as f32 / rate.max(1) as f32
    }

    /// Whether the take hit its 10-minute cap.
    pub fn is_full(&self) -> bool {
        self.0.full.load(Ordering::Relaxed)
    }
}

/// An input stream that records into memory until stopped.
pub struct Recorder {
    stream: Option<cpal::Stream>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Take>>,
    rate: u32,
    channels: usize,
    device_name: String,
    /// The callback count last seen, and since when.
    seen: (u64, Instant),
}

impl Recorder {
    /// Opens `device_id` (`None` = the system default input) at the device's
    /// own rate and starts recording at once.
    pub fn start(device_id: Option<&str>) -> Result<Recorder, String> {
        let host = cpal::default_host();
        let dev = match device_id {
            Some(id) => {
                let id: cpal::DeviceId = id.parse().map_err(|_| format!("bad device id {id}"))?;
                host.device_by_id(&id).ok_or("that input device is gone")?
            }
            None => host.default_input_device().ok_or("no audio input device found")?,
        };
        let device_name = dev.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "the input".into());
        let supported = dev.default_input_config().map_err(explain)?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let channels = (config.channels as usize).max(1);
        let rate = config.sample_rate;

        let shared = Arc::new(Shared::default());
        let (producer, mut consumer) = rtrb::RingBuffer::new((rate as usize * channels * RING_SECONDS).max(1024));
        let capture = Capture { ring: producer, owed: 0 };
        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(&dev, &config, &shared, capture),
            cpal::SampleFormat::I16 => build::<i16>(&dev, &config, &shared, capture),
            cpal::SampleFormat::I32 => build::<i32>(&dev, &config, &shared, capture),
            cpal::SampleFormat::U16 => build::<u16>(&dev, &config, &shared, capture),
            other => Err(format!("unsupported input sample format {other:?}")),
        }?;

        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (shared, stop) = (shared.clone(), stop.clone());
            let max_frames = rate as usize * MAX_SECONDS;
            std::thread::Builder::new()
                .name("input drain".into())
                .spawn(move || {
                    let mut take = Take::new(channels, max_frames);
                    loop {
                        let stopping = stop.load(Ordering::Acquire);
                        drain(&mut consumer, &mut take);
                        shared.frames.store(take.frames(), Ordering::Relaxed);
                        shared.full.store(take.is_full(), Ordering::Relaxed);
                        if stopping {
                            return take;
                        }
                        std::thread::sleep(DRAIN_EVERY);
                    }
                })
                .map_err(|e| e.to_string())?
        };
        stream.play().map_err(explain)?;
        Ok(Recorder { stream: Some(stream), shared, stop, worker: Some(worker), rate, channels, device_name, seen: (0, Instant::now()) })
    }

    /// The device's sample rate: the take's rate.
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// A handle to read the level and the time from any thread.
    pub fn meter(&self) -> InputMeter {
        InputMeter(self.shared.clone())
    }

    /// Why the input can't go on, if it can't: the device went away or
    /// stopped sending audio, or its callback hit a bug.
    pub fn trouble(&mut self) -> Option<String> {
        let name = &self.device_name;
        match self.shared.fault.load(Ordering::Relaxed) {
            GONE => return Some(format!("{name} is no longer available")),
            BROKEN => return Some("the audio input hit an internal error".into()),
            _ => {}
        }
        let beats = self.shared.beats.load(Ordering::Relaxed);
        if beats != self.seen.0 {
            self.seen = (beats, Instant::now());
        }
        (self.seen.1.elapsed() >= STALL).then(|| format!("{name} stopped sending audio"))
    }

    /// Samples the ring lost to a slow worker, as milliseconds.
    pub fn lost_ms(&self) -> u64 {
        let frames = self.shared.dropped.load(Ordering::Relaxed) / self.channels;
        frames as u64 * 1000 / u64::from(self.rate.max(1))
    }

    /// Stops the stream and returns the take: the sample rate and one `Vec`
    /// of samples per channel.
    pub fn stop(mut self) -> Result<(u32, Vec<Vec<f32>>), String> {
        self.stream = None; // dropping the stream ends the callbacks
        self.stop.store(true, Ordering::Release);
        let take = self.worker.take().ok_or("already stopped")?.join().map_err(|_| "the input worker failed".to_string())?;
        if take.frames() == 0 {
            return Err("nothing was recorded".into());
        }
        Ok((self.rate, take.into_channels()))
    }

    /// Stops the stream and throws the take away.
    pub fn cancel(mut self) {
        self.stream = None;
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stream = None;
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn build<T>(dev: &cpal::Device, config: &cpal::StreamConfig, shared: &Arc<Shared>, mut capture: Capture) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::Sample;
    let (sh, errors) = (shared.clone(), shared.clone());
    let channels = (config.channels as usize).max(1);
    let mut scratch = vec![0.0f32; BLOCK_FRAMES * channels];
    let chunk = scratch.len();
    dev.build_input_stream(
        *config,
        // Audio thread: the scratch buffer, atomics and the ring only.
        move |data: &[T], _| {
            sh.beats.fetch_add(1, Ordering::Relaxed);
            if sh.fault.load(Ordering::Relaxed) == BROKEN {
                return;
            }
            let work = std::panic::AssertUnwindSafe(|| {
                for part in data.chunks(chunk) {
                    let s = &mut scratch[..part.len()];
                    let mut peak = 0.0f32;
                    for (o, x) in s.iter_mut().zip(part) {
                        *o = finite(f32::from_sample(*x));
                        peak = peak.max(o.abs());
                    }
                    raise_peak(&sh.peak, peak);
                    let lost = capture.push(s);
                    if lost > 0 {
                        sh.dropped.fetch_add(lost, Ordering::Relaxed);
                    }
                }
            });
            if std::panic::catch_unwind(work).is_err() {
                sh.fault.store(BROKEN, Ordering::Relaxed);
            }
        },
        // cpal calls this off the audio thread.
        move |e: cpal::Error| {
            // A rerouted default device (`DeviceChanged`) and an xrun are not faults.
            let kind = e.kind();
            if kind == cpal::ErrorKind::DeviceNotAvailable || kind == cpal::ErrorKind::StreamInvalidated {
                let _ = errors.fault.compare_exchange(0, GONE, Ordering::Relaxed, Ordering::Relaxed);
            }
            eprintln!("input stream error: {e}");
        },
        None,
    )
    .map_err(explain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(n: usize) -> (Capture, rtrb::Consumer<f32>) {
        let (p, c) = rtrb::RingBuffer::new(n);
        (Capture { ring: p, owed: 0 }, c)
    }

    #[test]
    fn a_take_splits_interleaved_samples_by_channel() {
        let mut t = Take::new(2, 100);
        t.feed(&[1.0, -1.0, 2.0, -2.0]);
        assert_eq!(t.frames(), 2);
        assert_eq!(t.into_channels(), vec![vec![1.0, 2.0], vec![-1.0, -2.0]]);
    }

    #[test]
    fn a_frame_cut_between_drains_stays_whole() {
        let mut t = Take::new(3, 100);
        t.feed(&[1.0, 2.0, 3.0, 4.0]); // one frame and the start of the next
        t.feed(&[5.0]);
        assert_eq!(t.frames(), 1);
        t.feed(&[6.0, 7.0, 8.0, 9.0, 10.0]);
        assert_eq!(t.into_channels(), vec![vec![1.0, 4.0, 7.0], vec![2.0, 5.0, 8.0], vec![3.0, 6.0, 9.0]]);
        // 10.0 is the start of a fourth frame that never finished.
    }

    #[test]
    fn a_take_stops_at_its_cap() {
        let mut t = Take::new(2, 3);
        t.feed(&[0.0; 4]);
        assert!(!t.is_full());
        t.feed(&[1.0; 8]);
        assert!(t.is_full());
        assert_eq!(t.frames(), 3);
        t.feed(&[2.0; 8]);
        assert_eq!(t.frames(), 3);
    }

    #[test]
    fn the_ring_drains_into_the_take_in_order_across_the_wrap() {
        let (mut cap, mut cons) = ring(8);
        let mut take = Take::new(2, 100);
        let mut next = 0.0;
        // Odd-sized pushes make the ring wrap and cut frames.
        for len in [5, 3, 7, 2, 6, 4, 5] {
            let block: Vec<f32> = (0..len).map(|i| next + i as f32).collect();
            next += len as f32;
            assert_eq!(cap.push(&block), 0);
            drain(&mut cons, &mut take);
        }
        let all: Vec<f32> = (0..next as usize).map(|i| i as f32).collect();
        let want: Vec<Vec<f32>> = (0..2).map(|c| all.iter().skip(c).step_by(2).copied().take(all.len() / 2).collect()).collect();
        assert_eq!(take.into_channels(), want);
    }

    #[test]
    fn a_full_ring_owes_the_lost_samples_as_silence() {
        let (mut cap, mut cons) = ring(8);
        let mut take = Take::new(1, 100);
        assert_eq!(cap.push(&[1.0; 6]), 0);
        assert_eq!(cap.push(&[2.0; 4]), 2); // two fit, two are lost
        drain(&mut cons, &mut take);
        assert_eq!(cap.push(&[3.0; 2]), 0);
        drain(&mut cons, &mut take);
        assert_eq!(cap.owed, 0);
        assert_eq!(take.into_channels()[0], [vec![1.0; 6], vec![2.0; 2], vec![0.0; 2], vec![3.0; 2]].concat());
    }

    #[test]
    fn nan_and_infinity_record_as_silence() {
        assert_eq!(finite(f32::NAN), 0.0);
        assert_eq!(finite(f32::INFINITY), 0.0);
        assert_eq!(finite(f32::NEG_INFINITY), 0.0);
        assert_eq!(finite(-0.5), -0.5);
    }

    #[test]
    fn the_peak_keeps_the_largest_and_resets() {
        let shared = Arc::new(Shared::default());
        let meter = InputMeter(shared.clone());
        raise_peak(&shared.peak, 0.25);
        raise_peak(&shared.peak, 0.75);
        raise_peak(&shared.peak, 0.5);
        assert_eq!(meter.take_peak(), 0.75);
        assert_eq!(meter.take_peak(), 0.0);
    }

    /// Lists the inputs. Skips on CI. Opening a stream is left out: it asks
    /// macOS for the microphone, so set `TUNEKORE_TEST_INPUT=1` to try one.
    #[test]
    fn lists_inputs_and_records_when_asked() {
        if std::env::var_os("CI").is_some() {
            eprintln!("CI has no sound card; skipping");
            return;
        }
        let inputs = list_inputs().unwrap();
        if inputs.is_empty() {
            eprintln!("no input devices; skipping");
            return;
        }
        assert!(inputs.iter().filter(|d| d.is_default).count() <= 1, "{inputs:?}");
        if std::env::var_os("TUNEKORE_TEST_INPUT").is_none() {
            return;
        }
        let rec = Recorder::start(None).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let (rate, channels) = rec.stop().unwrap();
        assert!(rate >= 8000);
        assert!(channels[0].len() > rate as usize / 4);
    }
}
