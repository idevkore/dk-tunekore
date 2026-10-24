//! A fixed test song: 120 BPM, 4/4, one note per beat (C4 E4 G4 C5), each half a beat long.
//! Shared by the VST3 and AU hosts so both plugins get the same notes and transport.

use std::sync::atomic::{AtomicU64, Ordering};

pub const TEMPO: f64 = 120.0;
pub const PITCHES: [u8; 4] = [60, 64, 67, 72];

/// Musical position of the current block, readable from plugin callbacks.
#[derive(Default)]
pub struct SharedTime {
    beats: AtomicU64,
    samples: AtomicU64,
    /// How many times the plugin asked the host for tempo or position (AU only).
    pub queries: AtomicU64,
}

impl SharedTime {
    pub fn set(&self, beats: f64, samples: i64) {
        self.beats.store(beats.to_bits(), Ordering::Relaxed);
        self.samples.store(samples as u64, Ordering::Relaxed);
    }
    pub fn beats(&self) -> f64 {
        f64::from_bits(self.beats.load(Ordering::Relaxed))
    }
    pub fn samples(&self) -> i64 {
        self.samples.load(Ordering::Relaxed) as i64
    }
}

pub struct Block {
    pub start_samples: i64,
    pub beats: f64,
    pub bar_start_beats: f64,
}

pub struct Sequencer {
    samples_per_beat: f64,
    pos: i64,
    muted: bool, // MK_NO_NOTES=1: send no notes, to check the sound comes from them
}

impl Sequencer {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            samples_per_beat: sample_rate * 60.0 / TEMPO,
            pos: 0,
            muted: std::env::var_os("MK_NO_NOTES").is_some(),
        }
    }

    /// Advances one block of `n` frames. Calls `note(on, pitch, offset)` for every
    /// note that starts or ends inside it. No allocation.
    pub fn block(&mut self, n: usize, mut note: impl FnMut(bool, u8, u32)) -> Block {
        let start = self.pos;
        let end = start + n as i64;
        let spb = self.samples_per_beat;
        let first = (start as f64 / spb).floor() as i64 - 1;
        let last = (end as f64 / spb).ceil() as i64;
        for k in first.max(0)..=last {
            if self.muted { break; }
            let pitch = PITCHES[(k % 4) as usize];
            let on = (k as f64 * spb).round() as i64;
            let off = ((k as f64 + 0.5) * spb).round() as i64;
            // Note-offs first, so a note never overlaps itself.
            if off >= start && off < end {
                note(false, pitch, (off - start) as u32);
            }
            if on >= start && on < end {
                note(true, pitch, (on - start) as u32);
            }
        }
        self.pos = end;
        let beats = start as f64 / spb;
        Block { start_samples: start, beats, bar_start_beats: (beats / 4.0).floor() * 4.0 }
    }
}
