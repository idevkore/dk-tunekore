//! Stems on the Rust side: hand-off files, the Phase 1 checks, waveform
//! peaks and WAV export.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use crate::Song;

/// The four stems, in display order.
pub const STEM_NAMES: [&str; 4] = ["vocals", "drums", "bass", "other"];

/// Writes `channels[c][frame]` as raw planar f32 little-endian.
pub fn write_planar(path: &Path, channels: &[Vec<f32>]) -> Result<(), String> {
    let f = File::create(path).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    let mut w = BufWriter::new(f);
    for ch in channels {
        for s in ch {
            w.write_all(&s.to_le_bytes()).map_err(|e| e.to_string())?;
        }
    }
    w.flush().map_err(|e| e.to_string())
}

pub fn read_planar(path: &Path, channels: usize, frames: usize) -> Result<Vec<Vec<f32>>, String> {
    let f = File::open(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let len = f.metadata().map_err(|e| e.to_string())?.len() as usize;
    if len != channels * frames * 4 {
        return Err(format!("{} has {len} bytes, expected {}", path.display(), channels * frames * 4));
    }
    let mut r = BufReader::new(f);
    let mut out = vec![vec![0.0f32; frames]; channels];
    let mut b = [0u8; 4];
    for ch in out.iter_mut() {
        for s in ch.iter_mut() {
            r.read_exact(&mut b).map_err(|e| e.to_string())?;
            *s = f32::from_le_bytes(b);
        }
    }
    Ok(out)
}

/// Separated stems, each with the song's channels, frames and rate.
#[derive(Debug, Clone)]
pub struct Stems {
    pub sample_rate: u32,
    /// In [`STEM_NAMES`] order, each `channels[c][frame]`.
    pub stems: Vec<(String, Arc<Vec<Vec<f32>>>)>,
}

impl Stems {
    pub fn read(paths: &HashMap<String, PathBuf>, song: &Song) -> Result<Self, String> {
        let mut stems = Vec::new();
        for name in STEM_NAMES {
            let path = paths.get(name).ok_or_else(|| format!("the worker returned no {name} stem"))?;
            stems.push((name.to_string(), Arc::new(read_planar(path, song.channels.len(), song.frames())?)));
        }
        Ok(Self { sample_rate: song.sample_rate, stems })
    }

    pub fn get(&self, name: &str) -> Option<&[Vec<f32>]> {
        self.stems.iter().find(|(n, _)| n == name).map(|(_, s)| s.as_slice())
    }
}

/// The Phase 1 checks: the stems line up with the song and sum back to it.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Checks {
    /// Every stem has the song's exact length and channel count.
    pub same_length: bool,
    /// Song power over the power of (song minus the sum of the stems), in dB.
    /// Higher is better; the model's error sets how high it gets.
    pub sum_snr_db: f64,
    /// Where the stems' sum best lines up with the song, in frames. 0 means
    /// sample-aligned.
    pub lag_frames: i64,
}

const MAX_LAG: i64 = 512;
const WINDOW: usize = 1 << 16;

pub fn check(song: &Song, stems: &Stems) -> Checks {
    let frames = song.frames();
    let same_length = stems
        .stems
        .iter()
        .all(|(_, s)| s.len() == song.channels.len() && s.iter().all(|c| c.len() == frames));
    let mut signal = 0.0f64;
    let mut error = 0.0f64;
    for (c, mix) in song.channels.iter().enumerate() {
        for (i, &m) in mix.iter().enumerate() {
            let sum: f32 = stems.stems.iter().map(|(_, s)| s[c].get(i).copied().unwrap_or(0.0)).sum();
            signal += (m as f64).powi(2);
            error += ((m - sum) as f64).powi(2);
        }
    }
    let sum_snr_db = if error == 0.0 { f64::INFINITY } else { 10.0 * (signal / error).log10() };

    // Lag: cross-correlate the mono mix with the mono sum around the loudest window.
    let mono = |get: &dyn Fn(usize, usize) -> f32| -> Vec<f32> {
        (0..frames).map(|i| (0..song.channels.len()).map(|c| get(c, i)).sum::<f32>()).collect()
    };
    let mix = mono(&|c, i| song.channels[c][i]);
    let sum = mono(&|c, i| stems.stems.iter().map(|(_, s)| s[c][i]).sum());
    let lag_frames = best_lag(&mix, &sum);
    Checks { same_length, sum_snr_db, lag_frames }
}

/// The shift of `b` against `a`, within ±MAX_LAG frames, that correlates best.
fn best_lag(a: &[f32], b: &[f32]) -> i64 {
    let n = a.len().min(b.len());
    if n < 2 * MAX_LAG as usize + 16 {
        return 0;
    }
    // The loudest window in hops of a quarter window.
    let win = WINDOW.min(n - 2 * MAX_LAG as usize);
    let mut start = MAX_LAG as usize;
    let mut loudest = -1.0f64;
    let mut s = MAX_LAG as usize;
    while s + win + MAX_LAG as usize <= n {
        let e: f64 = a[s..s + win].iter().map(|&x| (x as f64).powi(2)).sum();
        if e > loudest {
            loudest = e;
            start = s;
        }
        s += win / 4;
    }
    let mut best = (0i64, f64::MIN);
    for lag in -MAX_LAG..=MAX_LAG {
        let mut acc = 0.0f64;
        for i in start..start + win {
            acc += a[i] as f64 * b[(i as i64 + lag) as usize] as f64;
        }
        if acc > best.1 {
            best = (lag, acc);
        }
    }
    best.0
}

/// Peak level per bucket across channels, 0..1, for drawing a waveform.
pub fn peaks(channels: &[Vec<f32>], buckets: usize) -> Vec<f32> {
    let frames = channels.first().map_or(0, Vec::len);
    if frames == 0 || buckets == 0 {
        return vec![0.0; buckets];
    }
    (0..buckets)
        .map(|b| {
            let lo = b * frames / buckets;
            let hi = ((b + 1) * frames / buckets).max(lo + 1).min(frames);
            channels.iter().flat_map(|c| c[lo..hi].iter()).fold(0.0f32, |m, &s| m.max(s.abs())).min(1.0)
        })
        .collect()
}

/// RMS level in dBFS (-inf for silence).
pub fn rms_db(channels: &[Vec<f32>]) -> f64 {
    let n: usize = channels.iter().map(Vec::len).sum();
    if n == 0 {
        return f64::NEG_INFINITY;
    }
    let p: f64 = channels.iter().flat_map(|c| c.iter()).map(|&s| (s as f64).powi(2)).sum::<f64>() / n as f64;
    10.0 * p.log10()
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum WavFormat {
    Pcm24,
    Float32,
}

/// Writes a stem as WAV at the song's rate: 24-bit, or 32-bit float when the
/// stem goes over full scale and 24-bit would clip it.
pub fn export_wav(channels: &[Vec<f32>], sample_rate: u32, path: &Path) -> Result<WavFormat, String> {
    let peak = channels.iter().flat_map(|c| c.iter()).fold(0.0f32, |m, &s| m.max(s.abs()));
    let format = if peak > 1.0 { WavFormat::Float32 } else { WavFormat::Pcm24 };
    let spec = match format {
        WavFormat::Pcm24 => hound::WavSpec {
            channels: channels.len() as u16,
            sample_rate,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        },
        WavFormat::Float32 => hound::WavSpec {
            channels: channels.len() as u16,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    };
    let tmp = path.with_extension("wav.part");
    let mut w = hound::WavWriter::create(&tmp, spec).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    let frames = channels.first().map_or(0, Vec::len);
    for i in 0..frames {
        for c in channels {
            let s = c[i];
            let r = match format {
                WavFormat::Pcm24 => w.write_sample((s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32),
                WavFormat::Float32 => w.write_sample(s),
            };
            r.map_err(|e| e.to_string())?;
        }
    }
    w.finalize().map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(format)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(frames: usize) -> Song {
        let mut seed = 1u32;
        let l: Vec<f32> = (0..frames)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223); // noise
                (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect();
        let r: Vec<f32> = l.iter().map(|x| -x * 0.5).collect();
        Song { sample_rate: 48_000, channels: Arc::new(vec![l, r]), codec: "PCM".into(), bits: None }
    }

    fn split(song: &Song, shift: usize) -> Stems {
        let parts = [0.4f32, 0.3, 0.2, 0.1];
        let stems = STEM_NAMES
            .iter()
            .zip(parts)
            .map(|(n, p)| {
                let chans: Vec<Vec<f32>> = song
                    .channels
                    .iter()
                    .map(|c| (0..c.len()).map(|i| if i >= shift { c[i - shift] * p } else { 0.0 }).collect())
                    .collect();
                (n.to_string(), Arc::new(chans))
            })
            .collect();
        Stems { sample_rate: song.sample_rate, stems }
    }

    #[test]
    fn exact_split_passes_the_checks() {
        let s = song(200_000);
        let c = check(&s, &split(&s, 0));
        assert!(c.same_length);
        assert_eq!(c.lag_frames, 0);
        assert!(c.sum_snr_db > 120.0, "{}", c.sum_snr_db);
    }

    #[test]
    fn a_shifted_split_shows_its_lag() {
        let s = song(200_000);
        let c = check(&s, &split(&s, 37));
        assert_eq!(c.lag_frames, 37);
        assert!(c.sum_snr_db < 3.0, "{}", c.sum_snr_db);
    }

    #[test]
    fn planar_round_trip_and_wav_export() {
        let dir = tempfile::tempdir().unwrap();
        let s = song(1000);
        let p = dir.path().join("x.f32");
        write_planar(&p, &s.channels).unwrap();
        assert_eq!(read_planar(&p, 2, 1000).unwrap(), *s.channels);
        assert!(read_planar(&p, 2, 999).is_err());

        let wav = dir.path().join("x.wav");
        assert_eq!(export_wav(&s.channels, 48_000, &wav).unwrap(), WavFormat::Pcm24);
        let back = crate::decode_file(&wav).unwrap();
        assert_eq!(back.frames(), 1000);
        assert!((back.channels[0][10] - s.channels[0][10]).abs() < 1e-6);

        let loud = vec![vec![1.5f32; 100]];
        assert_eq!(export_wav(&loud, 48_000, &wav).unwrap(), WavFormat::Float32);
    }

    #[test]
    fn peaks_follow_the_level() {
        let ch = vec![(0..1000).map(|i| if i < 500 { 0.1 } else { -0.8 }).collect::<Vec<f32>>()];
        let p = peaks(&ch, 4);
        assert_eq!(p, vec![0.1, 0.1, 0.8, 0.8]);
    }
}
