//! The master limiter: a lookahead brickwall limiter on everything the
//! engine plays, so a full kit never clips the output.
//!
//! Adapted from soundCraft (github.com/storytold/soundcraft,
//! crates/dsp/src/plugins/dynamics.rs, `Maximizer`), MIT OR Apache-2.0,
//! Copyright (c) 2026 ArtCraft Team and the SoundCraft contributors.
//!
//! The gain each frame needs to keep the peak under the ceiling is held at
//! its minimum over the lookahead window, then smoothed with a moving
//! average of the same length, so the gain ramps down before the peak
//! reaches the (delayed) output. Buffers are allocated in [`Limiter::new`];
//! [`Limiter::process`] is real-time safe.

/// Output never goes above this: -0.3 dBFS.
pub const CEILING: f32 = 0.966;
const LOOKAHEAD_SECONDS: f64 = 0.0015;
/// Gain recovers with this time constant after a peak.
const RELEASE_SECONDS: f64 = 0.08;

pub struct Limiter {
    channels: usize,
    la: usize,
    /// Interleaved delay line, `la + 1` frames.
    delay: Vec<f32>,
    write: usize,
    req: Vec<f32>,
    smooth: Vec<f32>,
    idx: usize,
    sum: f64,
    held: f32,
    rel: f32,
}

impl Limiter {
    /// A limiter for `channels` interleaved channels at `rate`. Allocates: call off the audio thread.
    pub fn new(rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        let la = ((rate.max(1) as f64 * LOOKAHEAD_SECONDS).round() as usize).max(1);
        Self {
            channels,
            la,
            delay: vec![0.0; (la + 1) * channels],
            write: 0,
            req: vec![1.0; la],
            smooth: vec![1.0; la],
            idx: 0,
            sum: la as f64,
            held: 1.0,
            rel: (-1.0 / (rate.max(1) as f64 * RELEASE_SECONDS)).exp() as f32,
        }
    }

    /// The delay the lookahead adds, in frames.
    pub fn latency(&self) -> usize {
        self.la
    }

    /// Applies `gain` (the master level) and limits `buf` (interleaved) in
    /// place. Returns the lowest gain applied, 1.0 when nothing was limited.
    pub fn process(&mut self, buf: &mut [f32], gain: f32) -> f32 {
        let ch = self.channels;
        let frames = buf.len() / ch;
        let len = self.la + 1;
        let mut lowest = 1.0f32;
        for n in 0..frames {
            let frame = &mut buf[n * ch..(n + 1) * ch];
            let mut peak = 0.0f32;
            for (c, s) in frame.iter().enumerate() {
                let x = if s.is_finite() { *s * gain } else { 0.0 };
                self.delay[self.write * ch + c] = x;
                peak = peak.max(x.abs());
            }
            self.req[self.idx] = if peak > CEILING { CEILING / peak } else { 1.0 };
            let hold = self.req.iter().copied().fold(1.0f32, f32::min);
            self.held = if hold < self.held { hold } else { hold + (self.held - hold) * self.rel };
            self.sum += f64::from(self.held) - f64::from(self.smooth[self.idx]);
            self.smooth[self.idx] = self.held;
            self.idx = (self.idx + 1) % self.la;
            if self.idx == 0 {
                // Re-sum once per window to cancel floating-point drift.
                self.sum = self.smooth.iter().map(|&v| f64::from(v)).sum();
            }
            let g = ((self.sum / self.la as f64) as f32).clamp(0.0, 1.0);
            lowest = lowest.min(g);
            // The slot after the write position was written `la` frames ago.
            let read = (self.write + 1) % len;
            for (c, s) in frame.iter_mut().enumerate() {
                *s = (self.delay[read * ch + c] * g).clamp(-CEILING, CEILING);
            }
            self.write = read;
        }
        lowest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frames: usize, amp: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = amp * (i as f32 * 0.05).sin();
                [v, v]
            })
            .collect()
    }

    #[test]
    fn quiet_audio_passes_unchanged_after_the_lookahead() {
        let mut l = Limiter::new(48_000, 2);
        let input = sine(4_800, 0.5);
        let mut buf = input.clone();
        assert_eq!(l.process(&mut buf, 1.0), 1.0);
        let la = l.latency();
        for i in 0..4_800 - la {
            assert!((buf[(i + la) * 2] - input[i * 2]).abs() < 1e-6, "frame {i}");
        }
    }

    #[test]
    fn loud_audio_never_passes_the_ceiling() {
        let mut l = Limiter::new(48_000, 2);
        let mut buf = sine(48_000, 3.0);
        let lowest = l.process(&mut buf, 1.0);
        assert!(lowest < 0.4, "it limited: {lowest}");
        assert!(buf.iter().all(|x| x.abs() <= CEILING));
        // Limited, not clipped: the shape stays smooth.
        let max_step = buf.chunks(2).collect::<Vec<_>>().windows(2).map(|w| (w[1][0] - w[0][0]).abs()).fold(0.0, f32::max);
        assert!(max_step < 0.2, "step {max_step}");
    }

    #[test]
    fn master_gain_scales_and_bad_samples_are_silenced() {
        let mut l = Limiter::new(48_000, 1);
        let mut buf = vec![0.4f32; 1_000];
        buf[10] = f32::NAN;
        l.process(&mut buf, 0.5);
        let la = l.latency();
        assert_eq!(buf[10 + la], 0.0);
        assert!((buf[500] - 0.2).abs() < 1e-6);
    }
}
