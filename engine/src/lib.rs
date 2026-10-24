//! musiKore's audio engine: output devices, a test tone (Phase 0) and the
//! Analyzer's stem player (Phase 1).
//!
//! Control calls (`list_outputs`, `open_output`, `close_output`, `load_clip`)
//! come from one thread at a time; callers serialize them. `set_tone`,
//! `take_peak` and the [`Player`] handle only touch atomics and are safe from
//! any thread. The audio callback runs on the device's thread: no allocation,
//! locks, logging or I/O there.

mod player;
mod resample;
mod tone;

pub use player::{Clip, PlayerShared, Track, MAX_TRACKS};
pub use resample::resample;
pub use tone::{Tone, ToneControl};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The callback works in blocks of at most this many frames.
const BLOCK_FRAMES: usize = 4096;

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

#[derive(Debug, Default)]
struct Shared {
    tone: ToneControl,
    player: PlayerShared,
    peak: AtomicU32, // f32 bits, max since the last take
}

impl Shared {
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
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self { host: cpal::default_host(), stream: None, shared: Arc::default(), link: None, rate: 0, source: None }
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
        }?;
        stream.play().map_err(|e| e.to_string())?;
        self.stream = Some(stream);
        if let Some(source) = self.source.clone() {
            self.send_clip(&source)?;
        }
        Ok(())
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
        let shared = self.shared.clone();
        let channels = (config.channels as usize).max(1);
        let rate = config.sample_rate;
        let mut tone = Tone::new(rate as f64);
        let (mut voice, link) = player::voice(rate);
        let mut scratch = vec![0.0f32; BLOCK_FRAMES * channels];
        shared.peak.store(0, Ordering::Relaxed);
        let stream = dev
            .build_output_stream(
                config.clone(),
                // Audio thread: atomics, lock-free queues and the scratch buffer only.
                move |out: &mut [T], _| {
                    let mut peak = 0.0f32;
                    for block in out.chunks_mut(BLOCK_FRAMES * channels) {
                        let buf = &mut scratch[..block.len()];
                        tone.render(&shared.tone, buf, channels);
                        voice.render(&shared.player, buf, channels);
                        for (o, &s) in block.iter_mut().zip(buf.iter()) {
                            peak = peak.max(s.abs());
                            *o = T::from_sample(s);
                        }
                    }
                    shared.raise_peak(peak);
                },
            // cpal calls this off the audio thread.
                |err| eprintln!("audio stream error: {err}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        self.link = Some(link);
        self.rate = rate;
        Ok(stream)
    }

    pub fn close_output(&mut self) {
        self.stream = None; // dropping the stream stops it and frees its clip
        self.link = None;
        self.shared.player.set_playing(false);
    }

    pub fn is_open(&self) -> bool {
        self.stream.is_some()
    }

    /// Turns the test tone on or off. Safe while the stream runs, from any thread.
    pub fn set_tone(&self, on: bool, freq_hz: f32, gain: f32) {
        self.shared.tone.set(on, freq_hz, gain);
    }

    /// A handle that reads the output peak without the engine's lock.
    pub fn meter(&self) -> Meter {
        Meter(self.shared.clone())
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

/// Reads the output level from any thread.
#[derive(Clone)]
pub struct Meter(Arc<Shared>);

impl Meter {
    /// Peak output level since the last call, 0..1.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.0.peak.swap(0, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
