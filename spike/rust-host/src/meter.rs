//! Output level and render-time stats, written by the audio thread, read by main.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[derive(Default)]
pub struct Meter {
    peak: AtomicU32,         // f32 bits, max since last take
    sumsq: AtomicU64,        // f64 bits
    frames: AtomicU64,
    blocks: AtomicU64,
    worst_load: AtomicU32,   // f32 bits: render time / block duration, max since last take
    total_frames: AtomicU64, // never reset
}

pub struct Reading {
    pub peak: f32,
    pub rms: f32,
    pub blocks: u64,
    pub worst_load: f32,
}

fn raise(a: &AtomicU32, v: f32) {
    let mut seen = a.load(Ordering::Relaxed);
    while v > f32::from_bits(seen) {
        match a.compare_exchange_weak(seen, v.to_bits(), Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(s) => seen = s,
        }
    }
}

impl Meter {
    /// Audio thread: record one rendered block. No allocation or locks.
    pub fn record(&self, left: &[f32], right: &[f32]) {
        let mut peak = 0.0f32;
        let mut sumsq = 0.0f64;
        for (l, r) in left.iter().zip(right) {
            peak = peak.max(l.abs()).max(r.abs());
            sumsq += (*l as f64) * (*l as f64) + (*r as f64) * (*r as f64);
        }
        raise(&self.peak, peak);
        let mut seen = self.sumsq.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(seen) + sumsq).to_bits();
            match self.sumsq.compare_exchange_weak(seen, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(s) => seen = s,
            }
        }
        self.frames.fetch_add(left.len() as u64 * 2, Ordering::Relaxed);
        self.blocks.fetch_add(1, Ordering::Relaxed);
        self.total_frames.fetch_add(left.len() as u64, Ordering::Relaxed);
    }

    /// Audio thread: record how long a device callback took, as a share of its budget.
    pub fn record_load(&self, load: f32) {
        raise(&self.worst_load, load);
    }

    pub fn take(&self) -> Reading {
        let peak = f32::from_bits(self.peak.swap(0, Ordering::Relaxed));
        let sumsq = f64::from_bits(self.sumsq.swap(0, Ordering::Relaxed));
        let frames = self.frames.swap(0, Ordering::Relaxed);
        let blocks = self.blocks.swap(0, Ordering::Relaxed);
        let worst_load = f32::from_bits(self.worst_load.swap(0, Ordering::Relaxed));
        let rms = if frames > 0 { (sumsq / frames as f64).sqrt() as f32 } else { 0.0 };
        Reading { peak, rms, blocks, worst_load }
    }

    pub fn total_frames(&self) -> u64 {
        self.total_frames.load(Ordering::Relaxed)
    }
}

pub fn db(x: f32) -> f32 {
    if x <= 0.0 { -120.0 } else { 20.0 * x.log10() }
}
