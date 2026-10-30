//! Waveform peaks for the Timeline's clips, worked out once per file off the
//! audio thread.
//!
//! Each level holds the lowest and highest sample (over both channels) of
//! every bucket of `frames` frames, as signed bytes (-127..127). The finest
//! level has [`PEAK_LEVELS`]`[0]` frames a bucket and each next one four
//! times as many, so the UI fetches the one nearest its zoom and never draws
//! more than a few buckets a pixel.

/// Frames per bucket at each level, finest first.
pub const PEAK_LEVELS: [usize; 6] = [16, 64, 256, 1024, 4096, 16384];

/// A file's peaks at every level.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Peaks {
    frames: usize,
    /// One per entry of [`PEAK_LEVELS`]: `[min, max]` per bucket.
    levels: Vec<Vec<[i8; 2]>>,
}

fn quantize(x: f32) -> i8 {
    (x.clamp(-1.0, 1.0) * 127.0).round() as i8
}

impl Peaks {
    /// The peaks of `channels` (`channels[c][frame]`). Slow for long files: off the audio thread.
    pub fn build(channels: &[Vec<f32>]) -> Self {
        let frames = channels.iter().map(Vec::len).min().unwrap_or(0);
        let first = PEAK_LEVELS[0];
        let mut finest = Vec::with_capacity(frames.div_ceil(first));
        for lo in (0..frames).step_by(first) {
            let hi = (lo + first).min(frames);
            let (mut min, mut max) = (f32::INFINITY, f32::NEG_INFINITY);
            for c in channels {
                for &s in &c[lo..hi] {
                    min = min.min(s);
                    max = max.max(s);
                }
            }
            finest.push([quantize(min), quantize(max)]);
        }
        let mut levels = vec![finest];
        for _ in 1..PEAK_LEVELS.len() {
            let next = levels
                .last()
                .map(|l: &Vec<[i8; 2]>| l.chunks(4).map(|c| [c.iter().map(|p| p[0]).min().unwrap_or(0), c.iter().map(|p| p[1]).max().unwrap_or(0)]).collect())
                .unwrap_or_default();
            levels.push(next);
        }
        Self { frames, levels }
    }

    /// The file's length in frames.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// The level with `PEAK_LEVELS[level]` frames a bucket, if there is one.
    pub fn level(&self, level: usize) -> Option<&[[i8; 2]]> {
        self.levels.get(level).map(Vec::as_slice)
    }

    /// A level as bytes for the UI: min then max of each bucket, as signed bytes.
    pub fn bytes(&self, level: usize) -> Vec<u8> {
        self.level(level).map_or_else(Vec::new, |l| l.iter().flat_map(|p| [p[0] as u8, p[1] as u8]).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_level_holds_the_lowest_and_highest_sample_of_its_buckets() {
        let mut left = vec![0.0f32; 1_000];
        let mut right = vec![0.0f32; 1_000];
        left[5] = 0.5;
        right[20] = -1.0;
        left[999] = 2.0; // clipped to full scale
        let p = Peaks::build(&[left, right]);
        assert_eq!(p.frames(), 1_000);
        let fine = p.level(0).unwrap();
        assert_eq!(fine.len(), 63, "1,000 frames in buckets of 16, the last one short");
        assert_eq!(fine[0], [0, 64]);
        assert_eq!(fine[1], [-127, 0]);
        assert_eq!(fine[62], [0, 127]);
        let coarse = p.level(1).unwrap();
        assert_eq!(coarse.len(), 16);
        assert_eq!(coarse[0], [-127, 64], "four fine buckets in one");
        assert_eq!(p.level(5).unwrap().len(), 1);
        assert!(p.level(6).is_none());
        assert_eq!(&p.bytes(0)[..4], &[0, 64, 129, 0], "signed bytes, min then max");
    }

    #[test]
    fn silence_and_nothing() {
        let p = Peaks::build(&[vec![0.0; 40]]);
        assert_eq!(p.level(0).unwrap(), &[[0, 0]; 3]);
        let none = Peaks::build(&[]);
        assert_eq!(none.frames(), 0);
        assert!(none.level(0).unwrap().is_empty());
    }
}
