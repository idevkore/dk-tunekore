//! Hands immutable snapshots of the document from the main thread to the
//! audio thread: a latest-wins mailbox with one slot.
//!
//! The main thread owns the document. After each edit it builds a new
//! snapshot and [`Mailbox::put`]s it, getting back the one before if the
//! audio thread never took it, to free or merge on the main thread. The
//! audio thread [`Mailbox::take`]s at the top of a block: it always gets the
//! newest, however many edits came in between, and it owns the box from
//! then on, so it hands the old one back on a queue of its own to be freed
//! on the main thread. Nothing is allocated or freed on the audio thread,
//! and a burst of edits can never fill the mailbox.

use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

/// One snapshot waiting for the audio thread, or none.
pub struct Mailbox<T> {
    slot: AtomicPtr<T>,
    _owns: PhantomData<Box<T>>,
}

// The mailbox owns at most one `Box<T>` and hands it whole from one thread to another.
unsafe impl<T: Send> Send for Mailbox<T> {}
unsafe impl<T: Send> Sync for Mailbox<T> {}

impl<T> Default for Mailbox<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Mailbox<T> {
    pub fn new() -> Self {
        Self { slot: AtomicPtr::new(ptr::null_mut()), _owns: PhantomData }
    }

    /// Main thread: leaves `next` for the audio thread. Returns the snapshot it
    /// replaced, which the audio thread never took.
    pub fn put(&self, next: Box<T>) -> Option<Box<T>> {
        let old = self.slot.swap(Box::into_raw(next), Ordering::AcqRel);
        // SAFETY: every pointer in the slot came from `Box::into_raw`, and the
        // swap hands it to exactly one owner.
        (!old.is_null()).then(|| unsafe { Box::from_raw(old) })
    }

    /// Takes the waiting snapshot, if any. On the audio thread this neither
    /// allocates nor frees: the caller owns the box from here.
    pub fn take(&self) -> Option<Box<T>> {
        let p = self.slot.swap(ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: as in `put`.
        (!p.is_null()).then(|| unsafe { Box::from_raw(p) })
    }
}

impl<T> Drop for Mailbox<T> {
    fn drop(&mut self) {
        drop(self.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    #[test]
    fn the_newest_wins_and_the_skipped_one_comes_back() {
        let m = Mailbox::new();
        assert!(m.take().is_none());
        assert!(m.put(Box::new(1)).is_none());
        assert_eq!(m.put(Box::new(2)).map(|b| *b), Some(1), "1 was never taken: it comes back to be freed");
        for v in 3..=1_000 {
            drop(m.put(Box::new(v)));
        }
        assert_eq!(m.take().map(|b| *b), Some(1_000), "however many came in, the newest");
        assert!(m.take().is_none());
    }

    #[test]
    fn a_snapshot_left_in_the_mailbox_is_freed_with_it() {
        struct Counted(Arc<AtomicUsize>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let m = Mailbox::new();
        m.put(Box::new(Counted(dropped.clone())));
        drop(m);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn it_moves_between_threads() {
        let m = Arc::new(Mailbox::new());
        let reader = {
            let m = m.clone();
            std::thread::spawn(move || {
                let mut last = 0;
                while last < 10_000 {
                    if let Some(v) = m.take() {
                        assert!(*v > last, "never an older one after a newer");
                        last = *v;
                    }
                }
            })
        };
        for v in 1..=10_000 {
            drop(m.put(Box::new(v)));
        }
        reader.join().unwrap();
    }
}
