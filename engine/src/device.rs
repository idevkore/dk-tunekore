//! Devices: what sits on a track. The Sampler is one; plugins (#37) and the
//! Timeline's clip players (#36) are others, each in its own file.
//!
//! A device is built and prepared on the main thread, then moved to the
//! audio thread, where only [`Device::process`] runs. It must be real-time
//! safe there: no allocation, locks, logging or file I/O. Anything it needs
//! (buffers, voices) is allocated in [`Device::prepare`].

use crate::transport::ProcessContext;

/// One piece of a track's audio, in stereo. `left` and `right` are the same length.
pub struct Buffers<'a> {
    pub left: &'a mut [f32],
    pub right: &'a mut [f32],
}

impl Buffers<'_> {
    pub fn frames(&self) -> usize {
        self.left.len().min(self.right.len())
    }
}

/// A note or controller message for a device, at a frame of the piece it
/// arrives with (0 = its first frame).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    NoteOn { frame: u32, channel: u8, key: u8, velocity: f32 },
    NoteOff { frame: u32, channel: u8, key: u8, velocity: f32 },
    /// A MIDI controller, `value` 0..1.
    Control { frame: u32, channel: u8, controller: u8, value: f32 },
}

impl Event {
    pub fn frame(&self) -> u32 {
        match *self {
            Event::NoteOn { frame, .. } | Event::NoteOff { frame, .. } | Event::Control { frame, .. } => frame,
        }
    }
}

/// The events moving along one track for one piece of a block, in frame
/// order. The list is allocated once; [`push`](Self::push) never grows it.
#[derive(Debug)]
pub struct Events {
    list: Vec<Event>,
}

impl Events {
    /// Off the audio thread.
    pub fn with_capacity(n: usize) -> Self {
        Self { list: Vec::with_capacity(n.max(1)) }
    }

    /// Adds an event. Returns false, dropping it, when the list is full.
    pub fn push(&mut self, e: Event) -> bool {
        if self.list.len() < self.list.capacity() {
            self.list.push(e);
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        self.list.clear();
    }

    pub fn as_slice(&self) -> &[Event] {
        &self.list
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.list.capacity()
    }
}

/// Something on a track that makes, plays or changes sound.
///
/// A track runs its source slot and then its chain, in order, over the same
/// [`Buffers`] and [`Events`]. The buffers start silent for each piece: a
/// source (an instrument, a clip player) adds its sound into them, and each
/// device after it changes them in place. Events go the same way: a clip
/// player or the keyboard pushes notes, and an instrument after it reads them.
///
/// Object safe, so a track holds `Box<dyn Device>`.
pub trait Device: Send {
    /// Main thread: get ready to run at `sample_rate`, in pieces of at most
    /// `max_frames`. Called before the device first runs and again whenever
    /// the output reopens, possibly at another rate. It may allocate.
    fn prepare(&mut self, sample_rate: u32, max_frames: usize) {
        let _ = (sample_rate, max_frames);
    }

    /// Audio thread: one piece of a block. `ctx.frames` is the buffers' length.
    fn process(&mut self, ctx: &ProcessContext, events: &mut Events, audio: Buffers<'_>);

    /// The delay the device adds, in frames. The graph will line paths up
    /// with it once delay compensation lands (Phase 6).
    fn latency(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_never_grow_past_their_capacity() {
        let mut e = Events::with_capacity(2);
        let on = Event::NoteOn { frame: 3, channel: 0, key: 60, velocity: 1.0 };
        assert!(e.push(on));
        assert!(e.push(on));
        assert!(!e.push(on), "full: dropped, not reallocated");
        assert_eq!(e.capacity(), 2);
        assert_eq!(e.as_slice()[0].frame(), 3);
        e.clear();
        assert!(e.is_empty());
    }
}
