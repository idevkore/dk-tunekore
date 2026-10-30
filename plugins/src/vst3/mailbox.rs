//! The latest value of each parameter set from outside the processor (the UI, the plugin's own
//! editor), waiting for the processor's next block.
//!
//! A queue would drop edits when full or while there is no processor to drain it; this keeps
//! one slot per parameter instead. A write stores the value and raises the slot's dirty bit; the
//! processor takes every dirty slot at the start of a block. Writers and the reader never lock
//! or allocate, so `performEdit` may come from any thread, the audio thread included. Values
//! wait across deactivation and reach the next processor.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// The id that marks an empty slot (VST3's `kNoParamId`).
const EMPTY: u32 = u32::MAX;

pub(crate) struct ParamMailbox {
    ids: Box<[AtomicU32]>,
    values: Box<[AtomicU64]>,
    /// One bit per slot.
    dirty: Box<[AtomicU64]>,
    mask: usize,
    /// Writes refused because every slot is taken by other parameters.
    refused: AtomicU32,
}

impl ParamMailbox {
    /// Room for `slots` parameters (rounded up to a power of two, at least 64). A plugin's
    /// parameters fit in half of it, so probing stays short.
    pub fn new(slots: usize) -> ParamMailbox {
        let n = slots.max(64).next_power_of_two();
        ParamMailbox {
            ids: (0..n).map(|_| AtomicU32::new(EMPTY)).collect(),
            values: (0..n).map(|_| AtomicU64::new(0)).collect(),
            dirty: (0..n / 64).map(|_| AtomicU64::new(0)).collect(),
            mask: n - 1,
            refused: AtomicU32::new(0),
        }
    }

    /// The slot for `id`, claiming a free one the first time. `None` when full.
    fn slot(&self, id: u32) -> Option<usize> {
        if id == EMPTY {
            return None;
        }
        let mut i = (id.wrapping_mul(0x9E37_79B1) as usize) & self.mask;
        for _ in 0..=self.mask {
            let cur = self.ids[i].load(Ordering::Acquire);
            if cur == id {
                return Some(i);
            }
            if cur == EMPTY {
                match self.ids[i].compare_exchange(EMPTY, id, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => return Some(i),
                    Err(now) if now == id => return Some(i),
                    Err(_) => {}
                }
            }
            i = (i + 1) & self.mask;
        }
        None
    }

    /// Sets `id` to `value` (normalized), replacing any value not yet taken. Any thread.
    pub fn post(&self, id: u32, value: f64) {
        match self.slot(id) {
            Some(i) => {
                self.values[i].store(value.to_bits(), Ordering::Release);
                self.dirty[i / 64].fetch_or(1 << (i % 64), Ordering::AcqRel);
            }
            None => {
                self.refused.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Takes every waiting value. The processor's side.
    pub fn drain(&self, mut f: impl FnMut(u32, f64)) {
        for (w, word) in self.dirty.iter().enumerate() {
            if word.load(Ordering::Relaxed) == 0 {
                continue;
            }
            let mut bits = word.swap(0, Ordering::AcqRel);
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let i = w * 64 + b;
                f(self.ids[i].load(Ordering::Acquire), f64::from_bits(self.values[i].load(Ordering::Acquire)));
            }
        }
    }

    /// Whether any value is waiting.
    pub fn pending(&self) -> bool {
        self.dirty.iter().any(|w| w.load(Ordering::Acquire) != 0)
    }

    /// Writes refused so far because the mailbox was full.
    pub fn refused(&self) -> u32 {
        self.refused.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn taken(m: &ParamMailbox) -> Vec<(u32, f64)> {
        let mut v = Vec::new();
        m.drain(|id, x| v.push((id, x)));
        v.sort_by_key(|p| p.0);
        v
    }

    #[test]
    fn the_latest_value_per_parameter_waits_until_taken() {
        let m = ParamMailbox::new(64);
        assert!(!m.pending());
        m.post(7, 0.1);
        m.post(7, 0.2);
        m.post(1_000_003, 0.9);
        assert!(m.pending());
        assert_eq!(taken(&m), vec![(7, 0.2), (1_000_003, 0.9)], "only the latest value of 7");
        assert!(!m.pending());
        assert!(taken(&m).is_empty(), "taken once");
        m.post(7, 0.3);
        assert_eq!(taken(&m), vec![(7, 0.3)], "the slot is reused");
    }

    #[test]
    fn a_full_mailbox_refuses_new_parameters_but_keeps_updating_old_ones() {
        let m = ParamMailbox::new(64);
        for id in 0..64 {
            m.post(id, 0.5);
        }
        m.post(1000, 0.5);
        assert_eq!(m.refused(), 1);
        m.post(3, 0.7);
        let all = taken(&m);
        assert_eq!(all.len(), 64);
        assert!(all.contains(&(3, 0.7)));
    }

    #[test]
    fn writers_on_many_threads_lose_nothing() {
        let m = std::sync::Arc::new(ParamMailbox::new(4096));
        let threads: Vec<_> = (0..4u32)
            .map(|t| {
                let m = m.clone();
                std::thread::spawn(move || {
                    for i in 0..500u32 {
                        m.post(t * 1000 + i, f64::from(i));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(taken(&m).len(), 2000);
    }
}
