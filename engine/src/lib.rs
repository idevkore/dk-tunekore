//! tuneKore's audio engine: output devices, a test tone (Phase 0), the
//! Analyzer's stem player (Phase 1), and the tracks every instrument and
//! clip plays on, summed into the master and its limiter. The first track is
//! the Sampler (Phase 2).
//!
//! - [`transport`]: the one beat clock (tempo, time signature, position,
//!   play, stop, loop). Every device gets a [`ProcessContext`] from it.
//! - [`track`] and [`graph`]: tracks with a source slot, a chain of devices,
//!   volume, pan, mute, solo and a meter. The main thread's [`Mixer`] owns
//!   the track list and publishes a [`snapshot`] of it after each edit.
//! - [`device`]: the [`Device`] trait that sources and effects implement.
//! - [`clip`]: the Timeline's audio clips, played by a [`ClipPlayer`] in an
//!   audio track's source slot; [`peaks`]: their waveforms.
//! - [`plugin_device`]: a hosted plugin (VST3, through `tunekore_plugins`)
//!   as an instrument in a source slot or an effect in a chain.
//!
//! Control calls (`list_outputs`, `open_output`, `close_output`,
//! `load_clip`, the track calls) come from one thread at a time; callers
//! serialize them. `set_tone`, `take_peak` and the [`Player`],
//! [`SamplerHandle`] and transport handles only touch atomics and are safe
//! from any thread. The audio callback runs on the device's thread: no
//! allocation, locks, logging or I/O there. In debug builds it runs inside
//! RealtimeSanitizer's real-time context, which aborts on any of those when
//! the build has `RTSAN_ENABLE=1` set.

pub mod clip;
pub mod device;
pub mod graph;
mod limiter;
mod input;
pub mod peaks;
mod player;
pub mod plugin_device;
mod resample;
mod sampler;
pub mod snapshot;
mod tone;
pub mod track;
pub mod transport;

#[cfg(test)]
pub(crate) mod testing;

pub use clip::{ClipFeed, ClipPlayer, ClipSet, ClipSpec, DECLICK_SECONDS};
pub use limiter::{Limiter, CEILING};
pub use input::{list_inputs, InputDevice, InputMeter, Recorder, Take, MAX_SECONDS};
pub use player::{Clip, PlayerShared, Track, MAX_TRACKS};
pub use sampler::{
    render_pattern, render_song, PadParams, Sampler, SamplerDevice, SamplerShared, BANKS, CHAIN_MAX, FILTER_BAND_PASS, FILTER_HIGH_PASS,
    FILTER_LOW_PASS, FILTER_OFF, MAX_STEPS, PADS, PREVIEW, SLOTS,
};
pub use device::{Buffers, Device, Event, Events};
pub use graph::Mixer;
pub use peaks::{Peaks, PEAK_LEVELS};
pub use plugin_device::{plugin_device, Activated, PluginDevice, PluginLink, Role as PluginRole};
/// The plugin host the engine runs plugins through.
pub use tunekore_plugins as plugins;
pub use resample::resample;
pub use tone::{Tone, ToneControl};
pub use track::{TrackId, TrackInfo, TrackKind, TrackMeter, TrackMix};
pub use transport::{LoopRange, ProcessContext, TempoMap, TimeSignature, Transport, TransportShared};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The callback works in blocks of at most this many frames.
const BLOCK_FRAMES: usize = graph::MAX_FRAMES;
/// The Sampler's track is the first, so its id is fixed.
pub const SAMPLER_TRACK: TrackId = TrackId(1);

/// Rates we offer when a device reports a range.
const COMMON_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OutputDevice {
    /// cpal's device id, stable across runs (e.g. "coreaudio:BuiltInSpeakerDevice").
    pub id: String,
    pub name: String,
    pub channels: u16,
    pub sample_rates: Vec<u32>,
    pub preferred_rate: u32,
    pub is_default: bool,
}

#[derive(Debug)]
struct Shared {
    tone: ToneControl,
    player: PlayerShared,
    transport: Arc<TransportShared>,
    sampler: Arc<SamplerShared>,
    peak: AtomicU32, // f32 bits, max since the last take
    /// Master level, linear, f32 bits.
    master: AtomicU32,
    /// Lowest limiter gain since the last take, f32 bits (1.0 = not limiting).
    limiting: AtomicU32,
    /// Errors the output stream reported since the last take. cpal can report
    /// them on the audio thread, so they are counted here and logged elsewhere.
    stream_errors: AtomicU32,
}

impl Default for Shared {
    fn default() -> Self {
        let transport = Arc::<TransportShared>::default();
        Self {
            tone: ToneControl::default(),
            player: PlayerShared::default(),
            sampler: Arc::new(SamplerShared::with_clock(transport.clone())),
            transport,
            peak: AtomicU32::new(0),
            master: AtomicU32::new(1.0f32.to_bits()),
            limiting: AtomicU32::new(1.0f32.to_bits()),
            stream_errors: AtomicU32::new(0),
        }
    }
}

impl Shared {
    fn lower_limiting(&self, g: f32) {
        let mut seen = self.limiting.load(Ordering::Relaxed);
        while g < f32::from_bits(seen) {
            match self.limiting.compare_exchange_weak(seen, g.to_bits(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(s) => seen = s,
            }
        }
    }

    fn raise_peak(&self, v: f32) {
        let mut seen = self.peak.load(Ordering::Relaxed);
        while v > f32::from_bits(seen) {
            match self.peak.compare_exchange_weak(seen, v.to_bits(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(s) => seen = s,
            }
        }
    }
}

/// Tracks to play, at their own sample rate.
#[derive(Debug, Clone, Default)]
pub struct Source {
    pub sample_rate: u32,
    pub tracks: Vec<Track>,
}

pub struct Engine {
    host: cpal::Host,
    stream: Option<cpal::Stream>,
    shared: Arc<Shared>,
    /// The control side's end of the open stream's player queues.
    link: Option<player::VoiceLink>,
    /// The open stream's sample rate.
    rate: u32,
    source: Option<Arc<Source>>,
    /// The tracks and their devices.
    mixer: Mixer,
    /// The control side's end of the Sampler device's sample queues.
    sampler_link: sampler::SamplerLink,
    /// Each slot's sample at its own rate, kept to send again when the device changes.
    samples: Vec<Option<(u32, Track)>>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        let shared = Arc::<Shared>::default();
        let mut mixer = Mixer::new();
        mixer.add_track_as(SAMPLER_TRACK, "Sampler").expect("the first track");
        mixer.set_kind(SAMPLER_TRACK, TrackKind::Sampler).expect("the first track");
        let (device, sampler_link) = sampler::device(shared.sampler.clone(), 48_000);
        mixer.set_source(SAMPLER_TRACK, Some(Box::new(device))).expect("the Sampler's track");
        Self {
            host: cpal::default_host(),
            stream: None,
            shared,
            link: None,
            rate: 0,
            source: None,
            mixer,
            sampler_link,
            samples: vec![None; SLOTS],
        }
    }

    /// The tracks and their devices. Add, remove and change tracks here;
    /// each edit reaches the audio thread as a new snapshot.
    pub fn mixer(&mut self) -> &mut Mixer {
        self.mixer.collect();
        &mut self.mixer
    }

    pub fn tracks(&self) -> Vec<TrackInfo> {
        self.mixer.tracks()
    }

    /// The transport every track runs on: tempo, time signature, play, stop,
    /// loop and position, from any thread.
    pub fn transport(&self) -> Arc<TransportShared> {
        self.shared.transport.clone()
    }

    /// Puts the Sampler back on its track after the output lost its devices.
    fn restore_sampler(&mut self) {
        let (device, link) = sampler::device(self.shared.sampler.clone(), self.rate.max(1));
        self.sampler_link = link;
        if self.mixer.track(SAMPLER_TRACK).is_none() {
            let _ = self.mixer.add_track_as(SAMPLER_TRACK, "Sampler");
            let _ = self.mixer.set_kind(SAMPLER_TRACK, TrackKind::Sampler);
        }
        let _ = self.mixer.restore_source(SAMPLER_TRACK, Box::new(device));
    }

    pub fn list_outputs(&self) -> Result<Vec<OutputDevice>, String> {
        let default_id = self.host.default_output_device().and_then(|d| d.id().ok());
        let devices = self.host.output_devices().map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for dev in devices {
            let Ok(id) = dev.id() else { continue };
            let Ok(configs) = dev.supported_output_configs() else { continue };
            let configs: Vec<_> = configs.collect();
            let channels = configs.iter().map(|c| c.channels()).max().unwrap_or(0);
            if channels == 0 {
                continue;
            }
            let mut rates: Vec<u32> = COMMON_RATES
                .into_iter()
                .filter(|r| configs.iter().any(|c| (c.min_sample_rate()..=c.max_sample_rate()).contains(r)))
                .collect();
            let preferred = dev.default_output_config().map(|c| c.sample_rate()).unwrap_or(0);
            if preferred != 0 && !rates.contains(&preferred) {
                rates.push(preferred);
                rates.sort_unstable();
            }
            let name = dev.description().map(|d| d.name().to_string()).unwrap_or_else(|_| id.to_string());
            out.push(OutputDevice {
                is_default: default_id.as_ref() == Some(&id),
                id: id.to_string(),
                name,
                channels,
                sample_rates: rates,
                preferred_rate: preferred,
            });
        }
        Ok(out)
    }

    /// Opens an output device and starts its stream at the device's own rate.
    /// `device_id` of `None` means the system default.
    pub fn open_output(&mut self, device_id: Option<&str>) -> Result<(), String> {
        self.close_output();
        let dev = match device_id {
            Some(id) => {
                let id: cpal::DeviceId = id.parse().map_err(|_| format!("bad device id {id}"))?;
                self.host.device_by_id(&id).ok_or("that output device is gone")?
            }
            None => self.host.default_output_device().ok_or("no audio output device found")?,
        };
        let supported = dev.default_output_config().map_err(|e| e.to_string())?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let stream = match format {
            cpal::SampleFormat::F32 => self.build::<f32>(&dev, &config),
            cpal::SampleFormat::I16 => self.build::<i16>(&dev, &config),
            cpal::SampleFormat::I32 => self.build::<i32>(&dev, &config),
            cpal::SampleFormat::U16 => self.build::<u16>(&dev, &config),
            other => Err(format!("unsupported sample format {other:?}")),
        };
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                // The callback (and the graph in it) was dropped: take the devices back.
                self.after_stream();
                return Err(e);
            }
        };
        if let Err(e) = stream.play() {
            drop(stream);
            self.after_stream();
            return Err(e.to_string());
        }
        self.stream = Some(stream);
        if let Some(source) = self.source.clone() {
            self.send_clip(&source)?;
        }
        for slot in 0..SLOTS {
            if self.samples[slot].is_some() {
                self.send_sample(slot)?;
            }
        }
        Ok(())
    }

    /// Puts a sample (`(sample rate, channels)`, one or two channels) on a
    /// Sampler slot, or empties it. Resamples it to the device's rate here,
    /// on the caller's thread. Without an open output it is kept and sent
    /// when one opens.
    pub fn load_sample(&mut self, slot: usize, sample: Option<(u32, Track)>) -> Result<(), String> {
        if slot >= SLOTS {
            return Err(format!("no sampler slot {slot}"));
        }
        if let Some((_, t)) = &sample {
            if t.is_empty() || t.len() > 2 || t[0].len() < 2 {
                return Err("a sample needs one or two channels of audio".into());
            }
        }
        self.samples[slot] = sample;
        if self.stream.is_some() {
            self.send_sample(slot)?;
        }
        Ok(())
    }

    /// A slot's sample at its own rate, as loaded.
    pub fn sample(&self, slot: usize) -> Option<(u32, Track)> {
        self.samples.get(slot).cloned().flatten()
    }

    fn send_sample(&mut self, slot: usize) -> Result<(), String> {
        let data = match &self.samples[slot] {
            None => None,
            Some((rate, t)) if *rate == self.rate => Some(t.clone()),
            Some((rate, t)) => Some(Arc::new(resample(t, *rate, self.rate)?)),
        };
        if self.stream.is_none() {
            return Err("no output is open".into());
        }
        self.sampler_link.send(slot, data)
    }

    /// Loads tracks into the player, replacing what it had. Resamples them to
    /// the device's rate here, on the caller's thread. Without an open output
    /// they are kept and sent when one opens.
    pub fn load_clip(&mut self, source: Arc<Source>) -> Result<(), String> {
        if source.tracks.len() > MAX_TRACKS {
            return Err(format!("the player takes at most {MAX_TRACKS} tracks"));
        }
        if source.tracks.iter().any(|t| t.is_empty() || t.len() > 2) {
            return Err("each track needs one or two channels".into());
        }
        self.shared.player.set_playing(false);
        self.source = Some(source.clone());
        if self.stream.is_some() {
            self.send_clip(&source)?;
        }
        Ok(())
    }

    /// Empties the player.
    pub fn unload_clip(&mut self) {
        self.shared.player.set_playing(false);
        self.source = None;
        if self.stream.is_some() {
            let _ = self.send(Box::default());
        }
    }

    fn send_clip(&mut self, source: &Source) -> Result<(), String> {
        let mut tracks = Vec::with_capacity(source.tracks.len());
        for t in &source.tracks {
            if source.sample_rate == self.rate {
                tracks.push(t.clone()); // same samples, no copy
            } else {
                tracks.push(Arc::new(resample(t, source.sample_rate, self.rate)?));
            }
        }
        let frames = tracks.iter().flat_map(|t| t.iter().map(Vec::len)).min().unwrap_or(0);
        self.send(Box::new(Clip { tracks, frames }))
    }

    fn send(&mut self, clip: Box<Clip>) -> Result<(), String> {
        let frames = clip.frames;
        let link = self.link.as_mut().ok_or("no output is open")?;
        link.collect();
        link.send.push(clip).map_err(|_| "the audio thread is not taking new clips".to_string())?;
        self.shared.player.loaded(frames, self.rate);
        Ok(())
    }

    /// A handle to play, pause, seek and mix the loaded clip from any thread.
    pub fn player(&self) -> Player {
        Player(self.shared.clone())
    }

    /// The open stream's sample rate, or 0.
    pub fn output_rate(&self) -> u32 {
        if self.stream.is_some() { self.rate } else { 0 }
    }

    fn build<T>(&mut self, dev: &cpal::Device, config: &cpal::StreamConfig) -> Result<cpal::Stream, String>
    where
        T: cpal::SizedSample + cpal::FromSample<f32>,
    {
        let channels = (config.channels as usize).max(1);
        let (mut output, link) = self.output(config.sample_rate, channels);
        let errors = self.shared.clone();
        let stream = dev
            .build_output_stream(
                *config,
                move |out: &mut [T], _| output.render(out),
                // cpal may call this on the audio thread (an overload notice):
                // count it here, and the app logs the count from its own thread.
                move |_err| {
                    errors.stream_errors.fetch_add(1, Ordering::Relaxed);
                },
                None,
            )
            .map_err(|e| e.to_string())?;
        self.link = Some(link);
        Ok(stream)
    }

    /// Everything the output callback runs, for a stream at `rate` that is
    /// `channels` wide: prepares every track's devices for this rate and moves
    /// them into its graph.
    fn output(&mut self, rate: u32, channels: usize) -> (Output, player::VoiceLink) {
        let (voice, link) = player::voice(rate);
        let graph = self.mixer.start(rate);
        self.shared.peak.store(0, Ordering::Relaxed);
        self.rate = rate;
        let out = Output {
            shared: self.shared.clone(),
            channels,
            tone: Tone::new(rate as f64),
            voice,
            transport: Transport::new(rate),
            graph,
            limiter: Limiter::new(rate, channels),
            scratch: vec![0.0f32; BLOCK_FRAMES * channels],
        };
        (out, link)
    }

    pub fn close_output(&mut self) {
        let was_open = self.stream.is_some();
        self.stream = None; // dropping the stream stops it and frees its clip
        self.link = None;
        self.shared.player.set_playing(false);
        if was_open {
            self.after_stream();
        }
    }

    /// Once the stream and its graph are dropped: takes the devices back, and
    /// puts the Sampler back if they were lost with the stream.
    fn after_stream(&mut self) {
        self.sampler_link.collect();
        if !self.mixer.stopped() || self.mixer.track(SAMPLER_TRACK).is_none_or(|t| !t.has_source) {
            self.restore_sampler();
        }
    }

    pub fn is_open(&self) -> bool {
        self.stream.is_some()
    }

    /// Turns the test tone on or off. Safe while the stream runs, from any thread.
    pub fn set_tone(&self, on: bool, freq_hz: f32, gain: f32) {
        self.shared.tone.set(on, freq_hz, gain);
    }

    /// A handle to the Sampler's pads, pattern and transport, from any thread.
    pub fn sampler(&self) -> SamplerHandle {
        SamplerHandle(self.shared.clone())
    }

    /// Sets the master level (linear, 0..2). Everything goes through the limiter after it.
    pub fn set_master(&self, gain: f32) {
        self.shared.master.store(gain.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn master(&self) -> f32 {
        f32::from_bits(self.shared.master.load(Ordering::Relaxed))
    }

    /// A handle that reads the output peak without the engine's lock.
    pub fn meter(&self) -> Meter {
        Meter(self.shared.clone())
    }
}

/// The output callback's state: the test tone, the Analyzer's player, the
/// transport and the graph, then the master level and the limiter.
struct Output {
    shared: Arc<Shared>,
    channels: usize,
    tone: Tone,
    voice: player::Voice,
    transport: Transport,
    graph: graph::Graph,
    limiter: Limiter,
    /// Interleaved, `BLOCK_FRAMES * channels`.
    scratch: Vec<f32>,
}

impl Output {
    /// One output callback: fills `out` (interleaved). Audio thread only:
    /// atomics, lock-free queues and preallocated buffers. In debug builds it
    /// runs in RealtimeSanitizer's real-time context.
    fn render<T: cpal::SizedSample + cpal::FromSample<f32>>(&mut self, out: &mut [T]) {
        #[cfg(debug_assertions)]
        rtsan_standalone::realtime_enter();
        let Output { shared, channels, tone, voice, transport, graph, limiter, scratch } = self;
        let channels = *channels;
        let mut peak = 0.0f32;
        for block in out.chunks_mut(BLOCK_FRAMES * channels) {
            let frames = block.len() / channels;
            let buf = &mut scratch[..block.len()];
            tone.render(&shared.tone, buf, channels);
            voice.render(&shared.player, buf, channels);
            graph.run(transport, &shared.transport, frames);
            mix_into(buf, channels, graph.master());
            let master = f32::from_bits(shared.master.load(Ordering::Relaxed));
            shared.lower_limiting(limiter.process(buf, master));
            for (o, &s) in block.iter_mut().zip(buf.iter()) {
                peak = peak.max(s.abs());
                *o = T::from_sample(s);
            }
        }
        shared.raise_peak(peak);
        #[cfg(debug_assertions)]
        rtsan_standalone::realtime_exit();
    }
}

/// Adds the graph's master (left, right) into an interleaved buffer
/// `channels` wide: a mono device gets the two halved and summed, and
/// channels past the second stay as they are.
fn mix_into(buf: &mut [f32], channels: usize, (left, right): (&[f32], &[f32])) {
    for (o, (&l, &r)) in buf.chunks_mut(channels).zip(left.iter().zip(right)) {
        if channels == 1 {
            o[0] += 0.5 * (l + r);
        } else {
            o[0] += l;
            o[1] += r;
        }
    }
}

/// Plays, pauses, seeks and mixes the loaded clip, from any thread.
#[derive(Clone)]
pub struct Player(Arc<Shared>);

impl std::ops::Deref for Player {
    type Target = PlayerShared;
    fn deref(&self) -> &PlayerShared {
        &self.0.player
    }
}

/// The Sampler's pads, pattern and transport, from any thread.
#[derive(Clone)]
pub struct SamplerHandle(Arc<Shared>);

impl std::ops::Deref for SamplerHandle {
    type Target = SamplerShared;
    fn deref(&self) -> &SamplerShared {
        &self.0.sampler
    }
}

impl Engine {
    /// A handle that reads a track's level from any thread.
    pub fn track_meter(&self, id: TrackId) -> Option<TrackMeter> {
        self.mixer.meter(id)
    }
}

/// Reads the output level from any thread.
#[derive(Clone)]
pub struct Meter(Arc<Shared>);

impl Meter {
    /// Peak output level since the last call, 0..1.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.0.peak.swap(0, Ordering::Relaxed))
    }

    /// The lowest master-limiter gain since the last call: 1.0 when it isn't limiting.
    pub fn take_limiting(&self) -> f32 {
        f32::from_bits(self.0.limiting.swap(1.0f32.to_bits(), Ordering::Relaxed))
    }

    /// How many errors the output stream reported since the last call.
    pub fn take_stream_errors(&self) -> u32 {
        self.0.stream_errors.swap(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The whole output callback, as a device runs it, with the main thread
    /// editing between blocks: nothing on the heap, and sound comes out.
    #[test]
    fn the_output_callback_never_allocates() {
        let mut engine = Engine::new();
        let (mut out, mut link) = engine.output(48_000, 2);
        let stem: Track = Arc::new(vec![vec![0.1; 96_000], vec![-0.1; 96_000]]);
        link.send.push(Box::new(player::Clip { tracks: vec![stem], frames: 96_000 })).unwrap();
        engine.shared.player.loaded(96_000, 48_000);
        let hit: Track = Arc::new(vec![(0..4_000).map(|i| if i < 50 { 0.8 } else { 0.0 }).collect()]);
        engine.sampler_link.send(0, Some(hit)).unwrap();
        let pads = engine.sampler();
        pads.set_pattern(0, 0x1111);
        pads.set_metronome(true);
        engine.set_tone(true, 440.0, 0.1);
        engine.player().set_playing(true);
        let transport = engine.transport();
        transport.set_loop(0.0, 2.0);
        transport.set_looping(true);
        transport.play();
        let extra = engine.mixer().add_track("extra").unwrap();
        let meter = engine.meter();
        let mut buf = vec![0.0f32; 512 * 2];
        let mut total = 0;
        for i in 0..400 {
            match i % 4 {
                0 => engine.mixer().set_mix(SAMPLER_TRACK, TrackMix { volume: 0.5 + (i % 3) as f32 * 0.25, ..Default::default() }).unwrap(),
                1 => engine.mixer().set_mix(extra, TrackMix { solo: i % 8 == 1, ..Default::default() }).unwrap(),
                2 => pads.trigger(0),
                _ => engine.sampler_link.collect(),
            }
            let (_, n) = testing::realtime(|| out.render(&mut buf));
            total += n;
        }
        assert_eq!(total, 0, "the callback allocated or freed {total} times");
        assert!(meter.take_peak() > 0.1, "it played");
        drop(out);
        assert!(engine.mixer.stopped(), "the devices came home");
    }

    #[test]
    fn has_a_version() {
        assert!(!VERSION.is_empty());
    }

    /// Plays a tone on the default output and checks the audio callback ran it.
    /// Skips on CI and on machines with no output device.
    #[test]
    fn tone_reaches_the_audio_callback() {
        if std::env::var_os("CI").is_some() {
            eprintln!("CI has no sound card; skipping");
            return;
        }
        let mut engine = Engine::new();
        let outputs = engine.list_outputs().unwrap();
        if outputs.is_empty() {
            eprintln!("no output devices; skipping");
            return;
        }
        assert_eq!(outputs.iter().filter(|d| d.is_default).count(), 1, "{outputs:?}");
        let meter = engine.meter();
        engine.set_tone(true, 440.0, 0.2);
        if let Err(e) = engine.open_output(None) {
            eprintln!("could not open the default output ({e}); skipping");
            return;
        }
        meter.take_peak();
        std::thread::sleep(Duration::from_millis(400));
        let peak = meter.take_peak();
        assert!(peak > 0.15 && peak <= 0.201, "peak was {peak}");

        engine.set_tone(false, 440.0, 0.2);
        std::thread::sleep(Duration::from_millis(60));
        meter.take_peak();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(meter.take_peak(), 0.0);
        engine.close_output();
        assert!(!engine.is_open());
    }

    #[test]
    fn opens_a_listed_device_by_id() {
        if std::env::var_os("CI").is_some() {
            return;
        }
        let mut engine = Engine::new();
        let Some(dev) = engine.list_outputs().unwrap().into_iter().find(|d| d.is_default) else { return };
        engine.open_output(Some(&dev.id)).unwrap();
        assert!(engine.is_open());
    }
}
