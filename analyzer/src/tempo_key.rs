//! Tempo and key detection for the Analyzer.
//!
//! Both work on at most 90 seconds from the middle of the song, mixed to
//! mono and low-passed down to about 11 kHz, so a four-minute song takes a
//! fraction of a second.
//!
//! **Tempo.** An onset-strength envelope from the spectral flux (the rise in
//! log magnitude between neighbouring STFT frames, summed over 30 Hz to
//! 5 kHz) is autocorrelated. Each candidate tempo from 70 to 180 BPM is
//! scored by a comb over the autocorrelation at one to four beat lengths,
//! weighted by a broad preference for tempi near 125 BPM (it only settles
//! doubles and halves, which the comb alone can't), then refined to 0.1 BPM
//! with a finer comb out to eight beats.
//!
//! **Key.** A chromagram from an 8192-point STFT: the spectral peaks between
//! 55 Hz and 2 kHz are placed on the semitone grid after correcting for the
//! recording's tuning, weighted by log magnitude, and summed into twelve
//! pitch classes. The chromagram is correlated (Pearson) with Temperley's
//! major and minor key profiles in all twelve rotations; the best of the 24
//! wins. (The Krumhansl-Kessler profiles were tried first; they lean towards
//! major and called minor progressions such as Am-Dm-E-Am major.)

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use std::f32::consts::PI;

/// The tempo range the estimate is folded into.
pub const MIN_BPM: f32 = 70.0;
pub const MAX_BPM: f32 = 180.0;

/// At most this much audio is analysed, from the middle of the song.
const MAX_SECONDS: f64 = 90.0;
/// The analysis works at about this rate (an integer fraction of the song's).
const WORK_RATE: f64 = 11_025.0;
/// Less audio than this has no tempo or key worth reporting.
const MIN_SECONDS: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempoEstimate {
    /// Beats per minute, `MIN_BPM..=MAX_BPM`, to 0.1.
    pub bpm: f32,
    /// 0 to 1: how strongly the beat repeats. Clean electronic or pop
    /// material is above about 0.6; a value under 0.3 is a guess.
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeyEstimate {
    /// The tonic's pitch class, 0 = C.
    pub tonic: u8,
    pub minor: bool,
    /// "A minor", "F♯ major".
    pub name: String,
    /// The Camelot wheel code producers use: "8A" is A minor, "8B" C major.
    pub camelot: String,
    /// 0 to 1: how well the best key fits, and how far ahead of the runner-up.
    pub confidence: f32,
}

// ---------------------------------------------------------------- shared

/// The middle (at most) 90 seconds as mono at about 11 kHz, with its rate.
fn prepare(channels: &[Vec<f32>], rate: u32) -> Option<(Vec<f32>, f32)> {
    let frames = channels.iter().map(Vec::len).min()?;
    if rate == 0 || (frames as f64) < MIN_SECONDS * rate as f64 {
        return None;
    }
    let n = frames.min((MAX_SECONDS * rate as f64) as usize);
    let start = (frames - n) / 2;
    let inv = 1.0 / channels.len() as f32;
    let mono: Vec<f32> = (start..start + n)
        .map(|i| {
            let s = channels.iter().map(|c| c[i]).sum::<f32>() * inv;
            if s.is_finite() {
                s
            } else {
                0.0
            }
        })
        .collect();
    let factor = (rate as f64 / WORK_RATE).round().max(1.0) as usize;
    Some((decimate(&mono, factor), rate as f32 / factor as f32))
}

/// Keeps every `factor`th sample after a windowed-sinc low-pass that leaves
/// the new Nyquist band alone.
fn decimate(x: &[f32], factor: usize) -> Vec<f32> {
    if factor <= 1 {
        return x.to_vec();
    }
    let taps = 8 * factor + 1;
    let half = taps / 2;
    let fc = 0.45 / factor as f32; // cycles per input sample
    let mut h: Vec<f32> = (0..taps)
        .map(|i| {
            let t = i as f32 - half as f32;
            let sinc = if t == 0.0 { 2.0 * fc } else { (2.0 * PI * fc * t).sin() / (PI * t) };
            let w = 0.42 - 0.5 * (2.0 * PI * i as f32 / (taps - 1) as f32).cos() + 0.08 * (4.0 * PI * i as f32 / (taps - 1) as f32).cos();
            sinc * w
        })
        .collect();
    let sum: f32 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= sum);
    (0..x.len() / factor)
        .map(|j| {
            let centre = j * factor;
            let from = half.saturating_sub(centre);
            let to = taps.min(x.len() + half - centre);
            let base = centre + from - half;
            h[from..to].iter().zip(&x[base..]).map(|(a, b)| a * b).sum()
        })
        .collect()
}

fn hann(n: usize) -> Vec<f32> {
    (0..n).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / n as f32).cos()).collect()
}

// ----------------------------------------------------------------- tempo

const FRAME: usize = 512;
const HOP: usize = 128;
/// The middle of the preferred tempo range, in BPM, and its width in octaves.
const PRIOR_CENTRE: f32 = 125.0;
const PRIOR_WIDTH: f32 = 0.9;

/// The tempo of the song, folded into 70 to 180 BPM. `None` when there is
/// less than a few seconds of audio or nothing that sounds like a beat.
pub fn tempo(channels: &[Vec<f32>], rate: u32) -> Option<TempoEstimate> {
    let (x, work_rate) = prepare(channels, rate)?;
    let (env, fps) = onset_envelope(&x, work_rate)?;
    let max_t = 60.0 * fps / MIN_BPM;
    let r = autocorrelation(&env, (8.0 * max_t) as usize + 4)?;

    // Coarse: every 0.25 BPM, one to four beats, weighted by the prior.
    let step = 0.25;
    let count = ((MAX_BPM - MIN_BPM) / step) as usize + 1;
    let scores: Vec<(f32, f32)> = (0..count)
        .map(|i| {
            let bpm = MIN_BPM + i as f32 * step;
            (bpm, comb(&r, 60.0 * fps / bpm, 4))
        })
        .collect();
    let (coarse, raw) = scores.iter().map(|&(b, s)| (b, s)).max_by(|a, b| (a.1 * prior(a.0)).total_cmp(&(b.1 * prior(b.0))))?;
    if raw <= 0.0 {
        return None;
    }

    // Fine: within a BPM of that, every 0.02 BPM, one to eight beats.
    let (mut best, mut best_score) = (coarse, f32::MIN);
    let mut bpm = (coarse - 1.0).max(MIN_BPM);
    while bpm <= (coarse + 1.0).min(MAX_BPM) {
        let s = comb(&r, 60.0 * fps / bpm, 8);
        if s > best_score {
            (best, best_score) = (bpm, s);
        }
        bpm += 0.02;
    }

    // Confidence: the repeat strength at the chosen tempo, and how far that
    // stands above the typical candidate.
    let mut all: Vec<f32> = scores.iter().map(|s| s.1).collect();
    all.sort_by(f32::total_cmp);
    let median = all[all.len() / 2];
    let strength = raw.clamp(0.0, 1.0);
    let prominence = if raw > 0.0 { ((raw - median) / raw).clamp(0.0, 1.0) } else { 0.0 };
    let confidence = (strength.sqrt() * prominence).clamp(0.0, 1.0);
    Some(TempoEstimate { bpm: (best * 10.0).round() / 10.0, confidence })
}

fn prior(bpm: f32) -> f32 {
    let octaves = (bpm / PRIOR_CENTRE).log2() / PRIOR_WIDTH;
    (-0.5 * octaves * octaves).exp()
}

/// Spectral flux per hop, with the local mean removed, and the hops per second.
fn onset_envelope(x: &[f32], rate: f32) -> Option<(Vec<f32>, f32)> {
    if x.len() < FRAME * 8 {
        return None;
    }
    let bin_hz = rate / FRAME as f32;
    let lo = (30.0 / bin_hz).ceil() as usize;
    let hi = ((5000.0 / bin_hz) as usize).min(FRAME / 2);
    let window = hann(FRAME);
    let fft = FftPlanner::<f32>::new().plan_fft_forward(FRAME);
    let frames = (x.len() - FRAME) / HOP + 1;
    let mut buf = vec![Complex::new(0.0f32, 0.0); FRAME];
    let mut scratch = vec![Complex::new(0.0f32, 0.0); fft.get_inplace_scratch_len()];
    let mut prev = vec![0.0f32; hi - lo];
    let mut flux = Vec::with_capacity(frames);
    for t in 0..frames {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = Complex::new(x[t * HOP + i] * window[i], 0.0);
        }
        fft.process_with_scratch(&mut buf, &mut scratch);
        let mut f = 0.0;
        for (p, bin) in prev.iter_mut().zip(&buf[lo..hi]) {
            let level = (1.0 + 1000.0 * bin.norm() / FRAME as f32).ln();
            f += (level - *p).max(0.0);
            *p = level;
        }
        flux.push(if t == 0 { 0.0 } else { f });
    }
    let fps = rate / HOP as f32;

    // Subtract a half-second running mean, keep the rises.
    let half = (fps * 0.25) as usize;
    let mut prefix = vec![0.0f64; flux.len() + 1];
    for (i, f) in flux.iter().enumerate() {
        prefix[i + 1] = prefix[i] + *f as f64;
    }
    let env: Vec<f32> = (0..flux.len())
        .map(|i| {
            let (a, b) = (i.saturating_sub(half), (i + half + 1).min(flux.len()));
            let mean = ((prefix[b] - prefix[a]) / (b - a) as f64) as f32;
            (flux[i] - mean).max(0.0)
        })
        .collect();
    let energy = env.iter().map(|v| v * v).sum::<f32>() / env.len() as f32;
    if energy < 1e-4 {
        return None; // silence, or too flat to have a beat
    }
    Some((env, fps))
}

/// The autocorrelation of `x` (mean removed) at lags `0..=max_lag`, with the
/// lag-0 value 1 and each lag averaged over the pairs it has.
fn autocorrelation(x: &[f32], max_lag: usize) -> Option<Vec<f32>> {
    let n = x.len();
    if n < 2 * max_lag {
        return None;
    }
    let mean = x.iter().sum::<f32>() / n as f32;
    let size = (2 * n).next_power_of_two();
    let mut buf: Vec<Complex<f32>> = x.iter().map(|v| Complex::new(v - mean, 0.0)).collect();
    buf.resize(size, Complex::new(0.0, 0.0));
    let mut planner = FftPlanner::<f32>::new();
    planner.plan_fft_forward(size).process(&mut buf);
    buf.iter_mut().for_each(|c| *c = Complex::new(c.norm_sqr(), 0.0));
    planner.plan_fft_inverse(size).process(&mut buf);
    let zero = buf[0].re / n as f32;
    if zero <= 0.0 {
        return None;
    }
    Some((0..=max_lag).map(|l| buf[l].re / (n - l) as f32 / zero).collect())
}

/// Catmull-Rom interpolation of `r` at the fractional lag `x`.
fn at(r: &[f32], x: f32) -> f32 {
    let i = x.floor() as usize;
    let f = x - i as f32;
    let p0 = r[i.saturating_sub(1)];
    let (p1, p2) = (r[i], r[i + 1]);
    let p3 = r[(i + 2).min(r.len() - 1)];
    0.5 * (2.0 * p1 + (p2 - p0) * f + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * f * f + (3.0 * p1 - p0 - 3.0 * p2 + p3) * f * f * f)
}

/// The mean autocorrelation at one to `beats` periods of `t` hops.
fn comb(r: &[f32], t: f32, beats: usize) -> f32 {
    let mut sum = 0.0;
    let mut used = 0;
    for k in 1..=beats {
        let lag = k as f32 * t;
        if lag + 2.0 >= r.len() as f32 {
            break;
        }
        sum += at(r, lag);
        used += 1;
    }
    if used == 0 {
        0.0
    } else {
        sum / used as f32
    }
}

// ------------------------------------------------------------------- key

const KEY_FFT: usize = 8192;
const KEY_HOP: usize = 2048;
const LOW_HZ: f32 = 55.0;
const HIGH_HZ: f32 = 2000.0;

/// Temperley's key profiles (Kostka-Payne corpus): how often each semitone
/// above the tonic turns up in a major or a minor key. Their thirds, 4.5
/// against 2, tell major from minor more sharply than Krumhansl-Kessler's.
const MAJOR: [f32; 12] = [5.0, 2.0, 3.5, 2.0, 4.5, 4.0, 2.0, 4.5, 2.0, 3.5, 1.5, 4.0];
const MINOR: [f32; 12] = [5.0, 2.0, 3.5, 4.5, 2.0, 4.0, 2.0, 4.5, 3.5, 2.0, 1.5, 4.0];

const MAJOR_NAMES: [&str; 12] = ["C", "D♭", "D", "E♭", "E", "F", "F♯", "G", "A♭", "A", "B♭", "B"];
const MINOR_NAMES: [&str; 12] = ["C", "C♯", "D", "E♭", "E", "F", "F♯", "G", "G♯", "A", "B♭", "B"];

/// The key of the song, one of 24. `None` for under a few seconds of audio,
/// silence, or a signal with no pitch content.
pub fn key(channels: &[Vec<f32>], rate: u32) -> Option<KeyEstimate> {
    let (x, work_rate) = prepare(channels, rate)?;
    let chroma = chromagram(&x, work_rate)?;
    let mut ranked: Vec<(f32, u8, bool)> = Vec::with_capacity(24);
    for tonic in 0..12u8 {
        for minor in [false, true] {
            let profile = if minor { &MINOR } else { &MAJOR };
            let rotated: Vec<f32> = (0..12).map(|pc| profile[(pc + 12 - tonic as usize) % 12]).collect();
            ranked.push((pearson(&chroma, &rotated)?, tonic, minor));
        }
    }
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    let (best, tonic, minor) = ranked[0];
    let runner_up = ranked[1].0;
    if best <= 0.0 {
        return None;
    }
    let confidence = best.clamp(0.0, 1.0) * (0.5 + 0.5 * ((best - runner_up) / 0.15).clamp(0.0, 1.0));
    Some(KeyEstimate {
        tonic,
        minor,
        name: format!("{} {}", if minor { MINOR_NAMES } else { MAJOR_NAMES }[tonic as usize], if minor { "minor" } else { "major" }),
        camelot: camelot(tonic, minor),
        confidence,
    })
}

/// The Camelot code: the number runs round the circle of fifths from C major
/// (8B); a minor key shares its relative major's number, with an A.
pub fn camelot(tonic: u8, minor: bool) -> String {
    let major_tonic = if minor { (tonic as usize + 3) % 12 } else { tonic as usize };
    let number = (major_tonic * 7 % 12 + 7) % 12 + 1;
    format!("{number}{}", if minor { 'A' } else { 'B' })
}

fn pearson(a: &[f32], b: &[f32]) -> Option<f32> {
    let n = a.len() as f32;
    let (ma, mb) = (a.iter().sum::<f32>() / n, b.iter().sum::<f32>() / n);
    let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        ab += (x - ma) * (y - mb);
        aa += (x - ma) * (x - ma);
        bb += (y - mb) * (y - mb);
    }
    if aa <= 1e-12 || bb <= 1e-12 {
        None
    } else {
        Some(ab / (aa * bb).sqrt())
    }
}

/// Pitch-class energy over the whole signal, 0 = C.
fn chromagram(x: &[f32], rate: f32) -> Option<[f32; 12]> {
    if x.len() < KEY_FFT * 2 {
        return None;
    }
    let bin_hz = rate / KEY_FFT as f32;
    let lo = (LOW_HZ / bin_hz).ceil() as usize;
    let hi = ((HIGH_HZ / bin_hz) as usize).min(KEY_FFT / 2 - 2);
    let window = hann(KEY_FFT);
    let fft = FftPlanner::<f32>::new().plan_fft_forward(KEY_FFT);
    let mut buf = vec![Complex::new(0.0f32, 0.0); KEY_FFT];
    let mut scratch = vec![Complex::new(0.0f32, 0.0); fft.get_inplace_scratch_len()];
    let mut mags = vec![0.0f32; hi + 2];

    // Spectral peaks per frame as (semitone, weight), loud frames only.
    let mut peaks: Vec<(f32, f32)> = Vec::new();
    let mut frame_ends: Vec<usize> = Vec::new();
    let mut loudest = 0.0f32;
    let mut frame_max: Vec<f32> = Vec::new();
    let frames = (x.len() - KEY_FFT) / KEY_HOP + 1;
    for t in 0..frames {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = Complex::new(x[t * KEY_HOP + i] * window[i], 0.0);
        }
        fft.process_with_scratch(&mut buf, &mut scratch);
        for (m, b) in mags.iter_mut().zip(&buf[..hi + 2]) {
            *m = b.norm();
        }
        let max = mags[lo..hi].iter().cloned().fold(0.0f32, f32::max);
        loudest = loudest.max(max);
        frame_max.push(max);
        if max > 0.0 {
            for i in lo.max(1)..hi {
                let (a, b, c) = (mags[i - 1], mags[i], mags[i + 1]);
                if b > a && b >= c && b > max * 0.003 {
                    // Parabolic interpolation on the log magnitude.
                    let (la, lb, lc) = (a.max(1e-12).ln(), b.ln(), c.max(1e-12).ln());
                    let denom = la - 2.0 * lb + lc;
                    let delta = if denom < 0.0 { (0.5 * (la - lc) / denom).clamp(-0.5, 0.5) } else { 0.0 };
                    let hz = (i as f32 + delta) * bin_hz;
                    let semitone = 69.0 + 12.0 * (hz / 440.0).log2();
                    peaks.push((semitone, (1.0 + 5.0 * b / max).ln()));
                }
            }
        }
        frame_ends.push(peaks.len());
    }
    if loudest <= 0.0 {
        return None;
    }

    // The recording's tuning: the weighted circular mean of how far the peaks
    // sit from the semitone grid. Skipped when the peaks don't agree.
    let (mut s, mut c, mut total) = (0.0f32, 0.0f32, 0.0f32);
    for &(st, w) in &peaks {
        let angle = 2.0 * PI * (st - st.round());
        s += w * angle.sin();
        c += w * angle.cos();
        total += w;
    }
    let tuning = if total > 0.0 && s.hypot(c) / total > 0.15 { s.atan2(c) / (2.0 * PI) } else { 0.0 };

    let mut chroma = [0.0f32; 12];
    let mut start = 0;
    for (t, &end) in frame_ends.iter().enumerate() {
        // Frames far below the loudest are silence or noise floor.
        if frame_max[t] >= loudest * 0.003 {
            let mut frame = [0.0f32; 12];
            for &(st, w) in &peaks[start..end] {
                let pc = ((st - tuning).round() as i64).rem_euclid(12) as usize;
                frame[pc] += w;
            }
            let sum: f32 = frame.iter().sum();
            if sum > 0.0 {
                chroma.iter_mut().zip(&frame).for_each(|(c, f)| *c += f / sum);
            }
        }
        start = end;
    }
    if chroma.iter().sum::<f32>() <= 0.0 {
        return None;
    }
    Some(chroma)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44_100;

    /// Deterministic noise in -1..1.
    struct Noise(u32);
    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0 as f32 / u32::MAX as f32 * 2.0 - 1.0
        }
    }

    fn add_kick(out: &mut [f32], at: usize, gain: f32) {
        for i in 0..(RATE as usize / 6).min(out.len().saturating_sub(at)) {
            let t = i as f32 / RATE as f32;
            let phase = 2.0 * PI * (50.0 * t + 100.0 * (1.0 - (-t * 30.0).exp()) / 30.0);
            out[at + i] += gain * phase.sin() * (-t * 22.0).exp();
        }
    }

    fn add_snare(out: &mut [f32], at: usize, noise: &mut Noise, gain: f32) {
        for i in 0..(RATE as usize / 8).min(out.len().saturating_sub(at)) {
            let t = i as f32 / RATE as f32;
            out[at + i] += gain * (0.7 * noise.next() + 0.3 * (2.0 * PI * 190.0 * t).sin()) * (-t * 35.0).exp();
        }
    }

    fn add_hat(out: &mut [f32], at: usize, noise: &mut Noise, gain: f32) {
        let mut last = 0.0;
        for i in 0..(RATE as usize / 30).min(out.len().saturating_sub(at)) {
            let t = i as f32 / RATE as f32;
            let n = noise.next();
            out[at + i] += gain * (n - last) * (-t * 150.0).exp(); // first difference: bright
            last = n;
        }
    }

    /// Kick on 1 and 3, snare on 2 and 4, hats on the eighths.
    fn beat(bpm: f32, seconds: f32) -> Vec<f32> {
        let mut out = vec![0.0f32; (seconds * RATE as f32) as usize];
        let mut noise = Noise(0x1234_5678);
        let beat_frames = 60.0 / bpm * RATE as f32;
        let mut n = 0;
        loop {
            let at = (n as f32 * beat_frames / 2.0) as usize;
            if at >= out.len() {
                break;
            }
            let eighth = n % 2 == 1;
            let beat_in_bar = (n / 2) % 4;
            if !eighth {
                if beat_in_bar % 2 == 0 {
                    add_kick(&mut out, at, 0.9);
                } else {
                    add_snare(&mut out, at, &mut noise, 0.6);
                }
            }
            add_hat(&mut out, at, &mut noise, if eighth { 0.12 } else { 0.2 });
            n += 1;
        }
        out
    }

    /// Four on the floor: a kick on every beat, hats between them.
    fn four_on_the_floor(bpm: f32, seconds: f32) -> Vec<f32> {
        let mut out = vec![0.0f32; (seconds * RATE as f32) as usize];
        let mut noise = Noise(0x0bad_cafe);
        let beat_frames = 60.0 / bpm * RATE as f32;
        let mut n = 0;
        while (n as f32 * beat_frames) < out.len() as f32 {
            add_kick(&mut out, (n as f32 * beat_frames) as usize, 0.9);
            add_hat(&mut out, ((n as f32 + 0.5) * beat_frames) as usize, &mut noise, 0.25);
            n += 1;
        }
        out
    }

    /// One short kick every `bpm`, nothing else.
    fn kicks(bpm: f32, seconds: f32) -> Vec<f32> {
        let mut out = vec![0.0f32; (seconds * RATE as f32) as usize];
        let beat_frames = 60.0 / bpm * RATE as f32;
        let mut n = 0;
        while (n as f32 * beat_frames) < out.len() as f32 {
            add_kick(&mut out, (n as f32 * beat_frames) as usize, 0.9);
            n += 1;
        }
        out
    }

    fn assert_tempo(audio: Vec<f32>, expected: f32) {
        let est = tempo(&[audio], RATE).expect("a tempo");
        assert!((est.bpm - expected).abs() <= 1.0, "expected {expected}, found {} (confidence {})", est.bpm, est.confidence);
        assert!(est.confidence > 0.5, "confidence {} for {expected} BPM", est.confidence);
    }

    #[test]
    fn finds_the_tempo_of_a_drum_pattern() {
        for bpm in [90.0, 100.0, 120.0, 128.0, 140.0] {
            assert_tempo(beat(bpm, 24.0), bpm);
        }
    }

    #[test]
    fn finds_four_on_the_floor_up_to_174() {
        for bpm in [90.0, 128.0, 150.0, 174.0] {
            assert_tempo(four_on_the_floor(bpm, 24.0), bpm);
        }
    }

    #[test]
    fn finds_a_tempo_between_whole_numbers() {
        assert_tempo(beat(123.4, 24.0), 123.4);
    }

    #[test]
    fn folds_slow_and_fast_kicks_into_range() {
        assert_tempo(kicks(60.0, 30.0), 120.0);
        assert_tempo(kicks(200.0, 20.0), 100.0);
        assert_tempo(kicks(76.0, 30.0), 76.0);
    }

    #[test]
    fn uses_stereo_and_ignores_a_short_or_silent_song() {
        let a = beat(110.0, 20.0);
        let est = tempo(&[a.clone(), a.iter().map(|v| v * 0.5).collect()], RATE).unwrap();
        assert!((est.bpm - 110.0).abs() <= 1.0);
        assert!(tempo(&[vec![0.0; RATE as usize * 20]], RATE).is_none());
        assert!(tempo(&[beat(120.0, 2.0)], RATE).is_none());
        assert!(tempo(&[], RATE).is_none());
    }

    #[test]
    fn noise_is_not_confident() {
        let mut noise = Noise(99);
        let audio: Vec<f32> = (0..RATE as usize * 20).map(|_| noise.next() * 0.3).collect();
        if let Some(est) = tempo(&[audio], RATE) {
            assert!(est.confidence < 0.35, "noise got confidence {}", est.confidence);
        }
    }

    /// A note with a few harmonics, for chords.
    fn add_note(out: &mut [f32], start: usize, len: usize, midi: f32, gain: f32) {
        let hz = 440.0 * 2f32.powf((midi - 69.0) / 12.0);
        for i in 0..len.min(out.len().saturating_sub(start)) {
            let t = i as f32 / RATE as f32;
            let env = (i as f32 / 400.0).min(1.0) * (1.0 - i as f32 / len as f32).min(1.0);
            let mut v = 0.0;
            for h in 1..=5 {
                v += (2.0 * PI * hz * h as f32 * t).sin() / h as f32;
            }
            out[start + i] += gain * env * v;
        }
    }

    /// Chords from the given roots (midi, bass) and third sizes, a second and
    /// a half each, the whole progression repeated to fill `seconds`.
    fn progression(chords: &[(f32, bool)], seconds: f32, detune: f32) -> Vec<f32> {
        let mut out = vec![0.0f32; (seconds * RATE as f32) as usize];
        let len = (1.5 * RATE as f32) as usize;
        let mut n = 0;
        while n * len < out.len() {
            let (root, minor) = chords[n % chords.len()];
            let at = n * len;
            add_note(&mut out, at, len, root - 24.0 + detune, 0.5); // bass
            add_note(&mut out, at, len, root + detune, 0.3);
            add_note(&mut out, at, len, root + if minor { 3.0 } else { 4.0 } + detune, 0.25);
            add_note(&mut out, at, len, root + 7.0 + detune, 0.25);
            n += 1;
        }
        out
    }

    fn assert_key(audio: Vec<f32>, name: &str, camelot: &str) {
        let est = key(&[audio], RATE).expect("a key");
        assert_eq!((est.name.as_str(), est.camelot.as_str()), (name, camelot), "confidence {}", est.confidence);
        assert!(est.confidence > 0.4, "confidence {} for {name}", est.confidence);
    }

    // Midi: C4 = 60, A3 = 57.
    #[test]
    fn finds_c_major() {
        // C, F, G, C, then Am, F, G, C
        let chords = [(60.0, false), (65.0, false), (67.0, false), (60.0, false), (57.0 + 12.0, true), (65.0, false), (67.0, false), (60.0, false)];
        assert_key(progression(&chords, 40.0, 0.0), "C major", "8B");
    }

    #[test]
    fn finds_a_minor() {
        // Am, Dm, Em (E major at the cadence), Am: A and E in the bass and chords
        let chords = [(57.0, true), (62.0, true), (64.0, false), (57.0, true), (57.0, true), (65.0, false), (64.0, false), (57.0, true)];
        assert_key(progression(&chords, 40.0, 0.0), "A minor", "8A");
    }

    #[test]
    fn finds_other_keys_and_tolerates_tuning() {
        // I, IV, V, I in F♯ major, a third of a semitone flat
        let f = 54.0;
        let chords = [(f, false), (f + 5.0, false), (f + 7.0, false), (f, false)];
        assert_key(progression(&chords, 30.0, -0.35), "F♯ major", "2B");
        // i, iv, V, i in D minor
        let d = 62.0;
        let chords = [(d, true), (d + 5.0, true), (d + 7.0, false), (d, true)];
        assert_key(progression(&chords, 30.0, 0.0), "D minor", "7A");
    }

    #[test]
    fn silence_and_short_audio_have_no_key() {
        assert!(key(&[vec![0.0; RATE as usize * 20]], RATE).is_none());
        assert!(key(&[progression(&[(60.0, false)], 1.0, 0.0)], RATE).is_none());
        assert!(key(&[], RATE).is_none());
    }

    #[test]
    fn finds_all_24_keys() {
        for root in 48..60 {
            let r = root as f32;
            let major = [(r, false), (r + 5.0, false), (r + 7.0, false), (r, false), (r + 9.0, true), (r + 5.0, false), (r + 7.0, false), (r, false)];
            let minor = [(r, true), (r + 5.0, true), (r + 7.0, false), (r, true), (r + 8.0, false), (r + 5.0, true), (r + 7.0, false), (r, true)];
            for (chords, is_minor) in [(major, false), (minor, true)] {
                let est = key(&[progression(&chords, 15.0, 0.0)], RATE).unwrap();
                assert_eq!((est.tonic, est.minor), (root as u8 % 12, is_minor), "root {root}: found {} ({:.2})", est.name, est.confidence);
            }
        }
    }

    #[test]
    fn camelot_codes() {
        assert_eq!(camelot(0, false), "8B"); // C major
        assert_eq!(camelot(9, true), "8A"); // A minor
        assert_eq!(camelot(7, false), "9B"); // G major
        assert_eq!(camelot(5, false), "7B"); // F major
        assert_eq!(camelot(6, false), "2B"); // F♯ major
        assert_eq!(camelot(11, true), "10A"); // B minor
        assert_eq!(camelot(8, true), "1A"); // G♯ minor
        assert_eq!(camelot(1, true), "12A"); // C♯ minor
        assert_eq!(camelot(2, true), "7A"); // D minor
    }

    #[test]
    fn decimating_keeps_the_band_and_drops_the_rest() {
        let n = RATE as usize;
        let tone = |hz: f32| -> Vec<f32> { (0..n).map(|i| (2.0 * PI * hz * i as f32 / RATE as f32).sin()).collect() };
        let rms = |v: &[f32]| (v[1000..v.len() - 1000].iter().map(|x| x * x).sum::<f32>() / (v.len() - 2000) as f32).sqrt();
        assert!(rms(&decimate(&tone(1000.0), 4)) > 0.69);
        assert!(rms(&decimate(&tone(8000.0), 4)) < 0.01); // would alias to 3 kHz
    }

    /// Four minutes, stereo, at 44.1 kHz: `cargo test -p tunekore-analyzer
    /// --release -- --ignored --nocapture` prints how long each takes.
    #[test]
    #[ignore]
    fn timing_for_a_four_minute_song() {
        let drums = beat(128.0, 240.0);
        let chords = [(57.0, true), (62.0, true), (64.0, false), (57.0, true)];
        let harmony = progression(&chords, 240.0, 0.0);
        let left: Vec<f32> = drums.iter().zip(&harmony).map(|(a, b)| a + b).collect();
        let right = left.clone();
        let song = [left, right];
        let t = std::time::Instant::now();
        let tempo = tempo(&song, RATE).unwrap();
        let tempo_time = t.elapsed();
        let t = std::time::Instant::now();
        let key = key(&song, RATE).unwrap();
        let key_time = t.elapsed();
        println!("tempo {:?} in {tempo_time:?}; key {} {} ({:.2}) in {key_time:?}", tempo, key.name, key.camelot, key.confidence);
        assert!((tempo.bpm - 128.0).abs() <= 1.0);
        assert_eq!(key.name, "A minor");
    }
}
