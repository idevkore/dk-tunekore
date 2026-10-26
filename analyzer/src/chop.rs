//! Chops audio into slices at its transients, for the Sampler's pads.
//!
//! Onsets come from a rise in the energy of the signal's first difference
//! (which favours the attack of a hit over sustained low end), picked as
//! local peaks, strongest first, with a minimum gap between them.

/// Analysis hop, in frames at 44.1 or 48 kHz (about 5 ms).
const HOP: usize = 256;
/// Energy is measured over this many frames.
const WINDOW: usize = 512;
/// Two slices start at least this far apart.
const MIN_GAP_SECONDS: f64 = 0.08;
/// Each slice starts this much before the detected onset, to keep the attack.
const PRE_ROLL_SECONDS: f64 = 0.004;

/// Up to `count` slice starts (frames) in `from..to`, in order. The first is
/// the first onset found; when fewer than `count` onsets stand out, fewer
/// are returned. Silence returns none.
pub fn onsets(channels: &[Vec<f32>], rate: u32, from: usize, to: usize, count: usize) -> Vec<usize> {
    let frames = channels.first().map_or(0, Vec::len);
    let to = to.min(frames);
    if count == 0 || to <= from + WINDOW {
        return Vec::new();
    }
    // Energy of the mono first difference, per hop.
    let mono = |i: usize| channels.iter().map(|c| c[i]).sum::<f32>() / channels.len() as f32;
    let hops = (to - from - WINDOW) / HOP + 1;
    let mut energy = Vec::with_capacity(hops);
    for h in 0..hops {
        let start = from + h * HOP;
        let mut e = 0.0f64;
        let mut prev = if start > 0 { mono(start - 1) } else { 0.0 };
        for i in start..start + WINDOW {
            let x = mono(i);
            e += ((x - prev) as f64).powi(2);
            prev = x;
        }
        energy.push((e / WINDOW as f64 + 1e-10).ln());
    }
    let peak_level = energy.iter().cloned().fold(f64::MIN, f64::max);
    // Novelty: how far each hop rises above the hops just before it.
    let novelty: Vec<f64> = (0..hops)
        .map(|h| {
            let lo = h.saturating_sub(4);
            if h == lo {
                return 0.0;
            }
            let before = energy[lo..h].iter().sum::<f64>() / (h - lo) as f64;
            // Quiet hops (40 dB under the loudest) aren't onsets.
            if energy[h] < peak_level - 9.2 { 0.0 } else { (energy[h] - before).max(0.0) }
        })
        .collect();
    // Local peaks that clear a floor.
    let mut peaks: Vec<(usize, f64)> = (0..hops)
        .filter(|&h| {
            let n = novelty[h];
            n > 1.0 && (h.saturating_sub(3)..(h + 4).min(hops)).all(|k| novelty[k] <= n)
        })
        .map(|h| (h, novelty[h]))
        .collect();
    peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
    let gap = (MIN_GAP_SECONDS * rate as f64 / HOP as f64).ceil() as usize;
    let mut chosen: Vec<usize> = Vec::new();
    for (h, _) in peaks {
        if chosen.len() == count {
            break;
        }
        if chosen.iter().all(|&c| c.abs_diff(h) >= gap) {
            chosen.push(h);
        }
    }
    chosen.sort_unstable();
    let pre = (PRE_ROLL_SECONDS * rate as f64) as usize;
    // The rise shows in the window that ends just after the hit; step back to it.
    chosen.iter().map(|&h| (from + h * HOP + WINDOW / 2).saturating_sub(HOP + pre).max(from)).collect()
}

/// Slices `from..to` at `starts`: each runs to the next start, and the last
/// to `to`, but no slice is longer than `max_frames`. Returns `(start, end)` pairs.
pub fn slices(starts: &[usize], to: usize, max_frames: usize) -> Vec<(usize, usize)> {
    starts
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let next = starts.get(i + 1).copied().unwrap_or(to);
            (s, next.min(s + max_frames).max(s + 1))
        })
        .collect()
}

/// `count` equal slices of `from..to`, for audio with no clear transients.
pub fn even(from: usize, to: usize, count: usize) -> Vec<usize> {
    if count == 0 || to <= from {
        return Vec::new();
    }
    (0..count).map(|i| from + (to - from) * i / count).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// Decaying noise bursts at the given frames, over a quiet hum.
    fn hits(at: &[usize], frames: usize) -> Vec<Vec<f32>> {
        let mut seed = 7u32;
        let mut x: Vec<f32> = (0..frames).map(|i| 0.01 * (i as f32 * 0.01).sin()).collect();
        for &a in at {
            for i in 0..6_000.min(frames - a) {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let n = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                x[a + i] += n * (-(i as f32) / 900.0).exp();
            }
        }
        vec![x.clone(), x]
    }

    #[test]
    fn finds_hits_within_a_few_milliseconds() {
        let at = [4_800, 30_000, 52_000, 77_000];
        let found = onsets(&hits(&at, 96_000), RATE, 0, 96_000, 8);
        assert_eq!(found.len(), 4, "{found:?}");
        for (f, a) in found.iter().zip(at) {
            assert!(*f <= a && a - f < 800, "found {f} for a hit at {a}");
        }
    }

    #[test]
    fn keeps_the_strongest_when_asked_for_fewer() {
        let mut audio = hits(&[10_000, 40_000, 70_000], 96_000);
        for c in audio.iter_mut() {
            for s in &mut c[40_000..46_000] {
                *s *= 0.05; // the middle hit is quiet
            }
        }
        let found = onsets(&audio, RATE, 0, 96_000, 2);
        assert_eq!(found.len(), 2);
        assert!(found[0] < 10_000 && found[1] > 60_000 && found[1] < 70_000, "{found:?}");
    }

    #[test]
    fn respects_the_range() {
        let found = onsets(&hits(&[5_000, 50_000], 96_000), RATE, 20_000, 96_000, 8);
        assert_eq!(found.len(), 1);
        assert!(found[0] > 45_000);
    }

    #[test]
    fn silence_has_no_onsets() {
        assert!(onsets(&[vec![0.0; 48_000]], RATE, 0, 48_000, 8).is_empty());
    }

    #[test]
    fn slices_run_to_the_next_start_and_are_capped() {
        assert_eq!(slices(&[0, 100, 1_000], 1_500, 400), vec![(0, 100), (100, 500), (1_000, 1_400)]);
        assert_eq!(even(0, 1_000, 4), vec![0, 250, 500, 750]);
    }
}
