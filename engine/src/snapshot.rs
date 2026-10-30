//! Hands immutable snapshots of the document from the main thread to the
//! audio thread, and the old ones back.
//!
//! The main thread owns the document. After each edit it builds a new
//! snapshot and [`Publisher::publish`]es it. The audio thread calls
//! [`Reader::update`] at the top of each block: it switches to the newest
//! snapshot and pushes the ones it is done with onto a return queue, so
//! nothing is ever freed on the audio thread. The main thread frees them in
//! [`Publisher::collect`], which `publish` also calls.

use rtrb::{Consumer, Producer, PushError, RingBuffer};

/// Snapshots that can wait at once on their way in. Edits faster than the
/// audio thread takes them fail to publish until it catches up.
pub const QUEUE: usize = 64;

/// The main thread's end.
pub struct Publisher<T> {
    send: Producer<Box<T>>,
    returned: Consumer<Box<T>>,
}

/// The audio thread's end. Always has a current snapshot.
pub struct Reader<T> {
    current: Box<T>,
    incoming: Consumer<Box<T>>,
    garbage: Producer<Box<T>>,
    /// A snapshot that could not go back because the return queue was full.
    held: Option<Box<T>>,
}

/// A publisher and a reader that starts on `first`.
pub fn channel<T: Send>(first: Box<T>) -> (Publisher<T>, Reader<T>) {
    let (send, incoming) = RingBuffer::new(QUEUE);
    // Room for every snapshot in flight plus the reader's own.
    let (garbage, returned) = RingBuffer::new(QUEUE * 2 + 1);
    (Publisher { send, returned }, Reader { current: first, incoming, garbage, held: None })
}

impl<T> Publisher<T> {
    /// Frees the snapshots the audio thread has finished with. Main thread only.
    pub fn collect(&mut self) -> usize {
        let mut n = 0;
        while self.returned.pop().is_ok() {
            n += 1;
        }
        n
    }

    /// Sends a new snapshot. Gives it back when the audio thread has fallen
    /// behind and the queue is full.
    pub fn publish(&mut self, snapshot: Box<T>) -> Result<(), Box<T>> {
        self.collect();
        self.send.push(snapshot).map_err(|PushError::Full(s)| s)
    }
}

impl<T> Reader<T> {
    pub fn current(&self) -> &T {
        &self.current
    }

    /// Switches to the newest snapshot that came in, if any, and hands the
    /// old ones back. Returns whether it switched. Real-time safe: it moves
    /// boxes between queues and never allocates or frees.
    pub fn update(&mut self) -> bool {
        if let Some(old) = self.held.take() {
            if let Err(PushError::Full(old)) = self.garbage.push(old) {
                // Still no room: keep it, and leave new snapshots queued until there is.
                self.held = Some(old);
                return false;
            }
        }
        let mut switched = false;
        while let Ok(next) = self.incoming.pop() {
            let old = std::mem::replace(&mut self.current, next);
            switched = true;
            if let Err(PushError::Full(old)) = self.garbage.push(old) {
                self.held = Some(old);
                break;
            }
        }
        switched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reader_takes_the_newest_and_returns_the_rest() {
        let (mut p, mut r) = channel(Box::new(0));
        assert_eq!(*r.current(), 0);
        assert!(!r.update(), "nothing new");
        for v in 1..=3 {
            p.publish(Box::new(v)).unwrap();
        }
        assert!(r.update());
        assert_eq!(*r.current(), 3, "only the newest counts");
        assert_eq!(p.collect(), 3, "the first snapshot and the two skipped ones came back");
    }

    #[test]
    fn a_full_queue_gives_the_snapshot_back() {
        let (mut p, mut r) = channel(Box::new(0u32));
        for v in 0..QUEUE as u32 {
            p.publish(Box::new(v)).unwrap();
        }
        assert_eq!(p.publish(Box::new(99)).map_err(|b| *b), Err(99));
        r.update();
        assert!(p.publish(Box::new(100)).is_ok(), "room again once the audio thread has caught up");
    }

    #[test]
    fn a_full_return_queue_holds_back_without_freeing() {
        let (mut p, mut r) = channel(Box::new(0u32));
        // Fill the return queue, as if the main thread had stopped collecting.
        for v in 0..r.garbage.slots() {
            r.garbage.push(Box::new(1_000 + v as u32)).unwrap();
        }
        p.send.push(Box::new(1)).unwrap();
        p.send.push(Box::new(2)).unwrap();
        assert!(r.update(), "it switches to 1, though 0 can't go back yet");
        assert_eq!(*r.current(), 1);
        assert!(r.held.is_some(), "0 is held, not freed");
        assert!(!r.update(), "while one is held, 2 waits in the queue");
        assert_eq!(*r.current(), 1);
        p.collect();
        assert!(r.update());
        assert_eq!(*r.current(), 2);
        assert!(r.held.is_none());
    }
}
