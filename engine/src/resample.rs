//! Sample-rate conversion for whole clips, off the audio thread.

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};

/// Resamples planar audio (`channels[c][frame]`) from `from` Hz to `to` Hz.
/// The result has `round(frames * to / from)` frames and no added delay.
pub fn resample(channels: &[Vec<f32>], from: u32, to: u32) -> Result<Vec<Vec<f32>>, String> {
    if from == to || channels.is_empty() {
        return Ok(channels.to_vec());
    }
    let n = channels.len();
    let frames = channels[0].len();
    let want = ((frames as u64 * to as u64 + from as u64 / 2) / from as u64) as usize;
    if frames == 0 {
        return Ok(vec![Vec::new(); n]);
    }
    let mut r = Fft::<f32>::new(from as usize, to as usize, 1024, n, FixedSync::Input).map_err(|e| e.to_string())?;
    let input = SequentialSliceOfVecs::new(channels, n, frames).map_err(|e| e.to_string())?;
    let out = r.process_all(&input, frames, None).map_err(|e| e.to_string())?;
    let data = out.take_data(); // interleaved; any short tail stays silent
    let mut planar = vec![vec![0.0f32; want]; n];
    for (i, frame) in data.chunks_exact(n).take(want).enumerate() {
        for (c, &s) in frame.iter().enumerate() {
            planar[c][i] = s;
        }
    }
    Ok(planar)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_length_timing_and_level() {
        let from = 44_100;
        let to = 48_000;
        let frames = 44_100;
        let sine: Vec<f32> = (0..frames).map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / from as f32).sin() * 0.5).collect();
        let out = resample(&[sine], from, to).unwrap();
        assert_eq!(out[0].len(), 48_000);
        // Compare against the ideal sine at the new rate, away from the edges.
        let err = (4_800..43_200)
            .map(|i| (out[0][i] - (i as f32 * 440.0 * std::f32::consts::TAU / to as f32).sin() * 0.5).abs())
            .fold(0.0, f32::max);
        assert!(err < 1e-3, "max error {err}: not aligned or not the same level");
    }

    #[test]
    fn same_rate_is_a_copy() {
        let x = vec![vec![1.0, 2.0, 3.0]];
        assert_eq!(resample(&x, 48_000, 48_000).unwrap(), x);
    }
}
