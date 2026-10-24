//! Sine test tone with a gain ramp.
//!
//! Real-time safe: no allocation, locks or I/O. The control side writes the
//! atomics; the audio callback reads them once per block.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const TWO_PI: f64 = std::f64::consts::TAU;
/// A full-scale change takes this long: enough to avoid a click.
const RAMP_SECONDS: f64 = 0.010;

/// Settings shared between the control side and the audio thread.
#[derive(Debug)]
pub struct ToneControl {
    on: AtomicBool,
    freq: AtomicU32, // f32 bits
    gain: AtomicU32, // f32 bits
}

impl Default for ToneControl {
    fn default() -> Self {
        Self {
            on: AtomicBool::new(false),
            freq: AtomicU32::new(440.0f32.to_bits()),
            gain: AtomicU32::new(0.2f32.to_bits()),
        }
    }
}

impl ToneControl {
    /// Any thread.
    pub fn set(&self, on: bool, freq_hz: f32, gain: f32) {
        self.freq.store(freq_hz.clamp(20.0, 20_000.0).to_bits(), Ordering::Relaxed);
        self.gain.store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        self.on.store(on, Ordering::Release);
    }
}

/// The audio-thread half: phase and the current ramp level.
#[derive(Debug)]
pub struct Tone {
    rate: f64,
    step: f32,
    phase: f64,
    level: f32,
}

impl Tone {
    pub fn new(sample_rate: f64) -> Self {
        Self { rate: sample_rate, step: (1.0 / (sample_rate * RAMP_SECONDS)) as f32, phase: 0.0, level: 0.0 }
    }

    /// Writes the tone into every channel of an interleaved buffer and returns
    /// the block's peak. `T` is the device's sample type.
    pub fn render<T: cpal::Sample + cpal::FromSample<f32>>(
        &mut self,
        ctl: &ToneControl,
        out: &mut [T],
        channels: usize,
    ) -> f32 {
        let on = ctl.on.load(Ordering::Acquire);
        let target = if on { f32::from_bits(ctl.gain.load(Ordering::Relaxed)) } else { 0.0 };
        let inc = f32::from_bits(ctl.freq.load(Ordering::Relaxed)) as f64 / self.rate * TWO_PI;
        let mut peak = 0.0f32;
        for frame in out.chunks_mut(channels.max(1)) {
            if self.level < target {
                self.level = (self.level + self.step).min(target);
            } else if self.level > target {
                self.level = (self.level - self.step).max(target);
            }
            let mut s = 0.0f32;
            if self.level > 0.0 || on {
                s = self.level * self.phase.sin() as f32;
                self.phase += inc;
                if self.phase >= TWO_PI {
                    self.phase -= TWO_PI;
                }
            }
            peak = peak.max(s.abs());
            let v = T::from_sample(s);
            frame.fill(v);
        }
        peak
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(tone: &mut Tone, ctl: &ToneControl, n: usize) -> (Vec<f32>, f32) {
        let mut buf = vec![0.0f32; n];
        let peak = tone.render(ctl, &mut buf, 1);
        (buf, peak)
    }

    #[test]
    fn silent_when_off() {
        let ctl = ToneControl::default();
        let mut tone = Tone::new(48_000.0);
        assert_eq!(mono(&mut tone, &ctl, 4800).1, 0.0);
    }

    #[test]
    fn ramps_up_without_a_click() {
        let ctl = ToneControl::default();
        let mut tone = Tone::new(48_000.0);
        ctl.set(true, 440.0, 0.5);
        let (buf, peak) = mono(&mut tone, &ctl, 4800);
        assert!(buf[0].abs() < 0.01, "starts near zero");
        let max_step = buf.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(max_step < 0.1, "440 Hz at gain 0.5 moves at most ~0.03 a sample, got {max_step}");
        assert!(peak > 0.49 && peak <= 0.5, "peak {peak}");
    }

    #[test]
    fn ramps_down_to_exact_silence() {
        let ctl = ToneControl::default();
        let mut tone = Tone::new(48_000.0);
        ctl.set(true, 440.0, 0.5);
        mono(&mut tone, &ctl, 4800);
        ctl.set(false, 440.0, 0.5);
        mono(&mut tone, &ctl, 4800);
        assert_eq!(mono(&mut tone, &ctl, 4800).1, 0.0);
    }

    #[test]
    fn plays_the_right_frequency() {
        let ctl = ToneControl::default();
        let mut tone = Tone::new(48_000.0);
        ctl.set(true, 1000.0, 0.5);
        let (sec, _) = mono(&mut tone, &ctl, 48_000);
        let crossings = sec.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        assert!((1995..=2005).contains(&crossings), "{crossings} zero crossings");
    }

    #[test]
    fn fills_every_channel() {
        let ctl = ToneControl::default();
        let mut tone = Tone::new(48_000.0);
        ctl.set(true, 440.0, 0.5);
        let mut buf = vec![0.0f32; 2 * 1000];
        tone.render(&ctl, &mut buf, 2);
        assert!(buf.chunks(2).all(|f| f[0] == f[1]));
    }
}
