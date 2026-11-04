//! MIDI clips on the Timeline (idevkore/dk-tunekore#38): the player that
//! plays an instrument track's clips as notes, and the main thread's handle
//! that feeds it.
//!
//! A MIDI clip lives on the beat grid, and so do its notes: everything is in
//! beats, so the notes follow the tempo. A clip starts at `start` on the
//! Timeline and plays `length` beats of its notes from `offset + phase`,
//! looping over the `content` beats from `offset` when `phase + length` runs
//! past them, as an audio clip loops over its file (`clip.rs`). A note plays
//! when the clip reaches its start, and stops at its end, at the end of the
//! clip's content (the loop's seam) or at the clip's end, whichever comes
//! first. A note that started before the playhead got there isn't played (no
//! chasing), as after a seek into it.
//!
//! The player sits in the track's notes slot, ahead of the instrument in the
//! source slot, and pushes its notes into the track's [`Events`] at the frame
//! each falls on: the first frame at or after its beat, as the Sampler's steps
//! and the audio clips land ([`ProcessContext::frame_of`]'s rule). Each piece
//! covers the beats from where the last one stopped, so a note is played once
//! however the blocks fall, and a tempo change just moves the frames.
//!
//! No stuck notes: the player keeps the beat each sounding note ends on, by
//! key, and sends a note-off for every sounding note when playback stops or
//! jumps (play, a seek, the transport's loop wrap) and at the start of the
//! piece where an edit lands, except for notes the new clips still hold
//! sounding there. A note played again while it sounds is let go first.
//!
//! The main thread builds a [`MidiClipSet`] for a track and
//! [`MidiFeed::publish`]es it; the player takes the newest at the top of a
//! piece and hands the old one back on a queue to be freed on the main
//! thread. Notes played from the note editor's keyboard come through the
//! feed's preview queue. Nothing the player does on the audio thread
//! allocates, frees, locks or logs.

use std::sync::Arc;

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::device::{Buffers, Device, Event, Events};
use crate::snapshot::Mailbox;
use crate::transport::{ProcessContext, EDGE_FRAMES};

/// MIDI keys.
pub const KEYS: usize = 128;
/// The shortest note and the shortest clip or loop, in beats.
pub const MIN_BEATS: f64 = 1.0 / 128.0;
/// The channel the notes go out on.
const CHANNEL: u8 = 0;
/// Sets on their way back to the main thread.
const RETURN_QUEUE: usize = 64;
/// Sets held on the audio thread while the return queue is full.
const SPILL: usize = 4;
/// Preview notes waiting for the next piece.
const PREVIEW_QUEUE: usize = 256;
/// Preview notes taken in one piece.
const PREVIEWS: usize = 64;
/// Note starts one piece can play; a denser piece drops the rest (never their ends).
const MAX_ONS: usize = 512;

/// One note of a clip, as the main thread describes it: `start` and `length`
/// in beats of the clip's own time (where `offset` counts from).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MidiNote {
    pub key: u8,
    pub start: f64,
    pub length: f64,
    /// 0..1.
    pub velocity: f32,
}

/// One MIDI clip, in beats. The same shape as an audio clip's
/// [`ClipSpec`](super::ClipSpec), with beats of the clip's notes in place of
/// seconds of a file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MidiClipSpec {
    /// Where the clip starts on the Timeline.
    pub start: f64,
    /// Where in the clip's own time its content (the loop) starts.
    pub offset: f64,
    /// How long the content is.
    pub content: f64,
    /// Where in the content the clip's first beat is.
    pub phase: f64,
    /// How long the clip plays. Past the end of the content it loops.
    pub length: f64,
    pub notes: Vec<MidiNote>,
}

impl MidiClipSpec {
    /// Where the clip ends on the Timeline.
    pub fn end(&self) -> f64 {
        self.start + self.length
    }

    /// Whether it plays its content more than once.
    pub fn is_looped(&self) -> bool {
        self.phase + self.length > self.content + 1e-9
    }
}

/// Cuts a clip at `cut` (a beat on the Timeline) into two that play on as
/// one: the second carries on where the first stops. A note that crosses the
/// cut in a clip that doesn't loop is cut in two, so it sounds on in the
/// second half; a looped clip's notes are shared by all its passes, so they
/// stay as they are, and a note that crosses the cut stops there. `None`
/// when `cut` isn't inside the clip.
pub fn split(c: &MidiClipSpec, cut: f64) -> Option<(MidiClipSpec, MidiClipSpec)> {
    let n = cut - c.start;
    if !(n >= MIN_BEATS && n <= c.length - MIN_BEATS) {
        return None;
    }
    let mut left = MidiClipSpec { length: n, ..c.clone() };
    let mut right = MidiClipSpec { start: cut, length: c.length - n, ..c.clone() };
    if c.is_looped() {
        right.phase = (c.phase + n) % c.content;
        return Some((left, right));
    }
    // Not looped: each half keeps the part of the content it plays, and the notes in it.
    let at = c.offset + c.phase + n;
    left.content = c.phase + n;
    right.offset = at;
    right.content = (c.content - c.phase - n).max(MIN_BEATS);
    right.phase = 0.0;
    left.notes = c.notes.iter().filter(|x| x.start < at).map(|x| MidiNote { length: x.length.min(at - x.start), ..*x }).collect();
    right.notes = c
        .notes
        .iter()
        .filter(|x| x.start + x.length > at + 1e-9)
        .map(|x| if x.start < at { MidiNote { start: at, length: x.start + x.length - at, ..*x } } else { *x })
        .collect();
    Some((left, right))
}

/// A note as the player plays it: from the content's start, its length
/// already cut at the content's end.
#[derive(Debug, Clone, Copy)]
struct Note {
    at: f64,
    len: f64,
    key: u8,
    velocity: f32,
}

#[derive(Debug, Clone)]
struct Playing {
    start: f64,
    end: f64,
    phase: f64,
    content: f64,
    /// Sorted by `at`, then key.
    notes: Vec<Note>,
}

impl Playing {
    /// The beat on the Timeline where pass `k` of the content starts.
    fn pass(&self, k: f64) -> f64 {
        self.start - self.phase + k * self.content
    }
}

/// Every MIDI clip on one track, sorted by start. Built on the main thread.
#[derive(Debug, Default)]
pub struct MidiClipSet {
    clips: Vec<Playing>,
}

impl MidiClipSet {
    pub fn empty() -> Self {
        Self::default()
    }

    /// The clips, made sane: a clip with a bad start or no length is left
    /// out, a note outside its content never plays and is left out, and the
    /// rest is clamped to range.
    pub fn new(clips: impl IntoIterator<Item = MidiClipSpec>) -> Self {
        let mut list: Vec<Playing> = Vec::new();
        for c in clips {
            if !(c.start.is_finite() && c.length.is_finite() && c.length > 0.0 && c.offset.is_finite()) {
                continue;
            }
            let content = if c.content.is_finite() { c.content.max(MIN_BEATS) } else { c.length.max(MIN_BEATS) };
            let phase = if c.phase.is_finite() { c.phase.rem_euclid(content) } else { 0.0 };
            let mut notes: Vec<Note> = c
                .notes
                .iter()
                .filter(|x| x.key < KEYS as u8 && x.start.is_finite() && x.length.is_finite())
                .filter_map(|x| {
                    let at = x.start - c.offset;
                    (at >= 0.0 && at < content).then(|| Note {
                        at,
                        len: x.length.max(MIN_BEATS).min(content - at),
                        key: x.key,
                        velocity: if x.velocity.is_finite() { x.velocity.clamp(1.0 / 127.0, 1.0) } else { 1.0 },
                    })
                })
                .collect();
            notes.sort_by(|a, b| a.at.total_cmp(&b.at).then(a.key.cmp(&b.key)));
            list.push(Playing { start: c.start, end: c.start + c.length, phase, content, notes });
        }
        list.sort_by(|a, b| a.start.total_cmp(&b.start));
        Self { clips: list }
    }

    pub fn len(&self) -> usize {
        self.clips.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }
}

/// A note played by hand, from the note editor's keyboard.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Preview {
    /// `velocity` 0..1.
    On { key: u8, velocity: f32 },
    Off { key: u8 },
    /// Lets go of every note played by hand.
    AllOff,
}

/// A note start found in a piece.
#[derive(Debug, Clone, Copy)]
struct On {
    beat: f64,
    off: f64,
    key: u8,
    velocity: f32,
}

/// An event on its way out. They go out by frame, ends before starts within
/// a frame (so notes that meet aren't joined legato), and otherwise in the
/// order they were made. The player never puts a note's start and its own
/// end on one frame, so that order is always the order things happened.
#[derive(Debug, Clone, Copy)]
struct Out {
    frame: u32,
    on: bool,
    seq: u32,
    key: u8,
    velocity: f32,
}

/// A track's MIDI clip player: its notes device.
///
/// It keeps every key's state as the instrument sees it: sounding or not,
/// and whose it is, a clip's (with the beat it ends on) or the keyboard's.
/// A key is never started twice or ended while silent, so no note sticks on
/// any instrument however its voices are kept.
pub struct MidiPlayer {
    current: Box<MidiClipSet>,
    mailbox: Arc<Mailbox<MidiClipSet>>,
    garbage: Producer<Box<MidiClipSet>>,
    spill: [Option<Box<MidiClipSet>>; SPILL],
    preview: Consumer<Preview>,
    /// Keys sounding, a bit each.
    on: u128,
    /// Those of them played by hand; the rest are a clip's.
    hand: u128,
    /// The beat each clip note sounding ends on, by key.
    end: [f64; KEYS],
    /// The frame of this piece each key last started on, or `u32::MAX`.
    started: [u32; KEYS],
    /// Keys whose end fell past the last piece: they end on this one's first frame.
    deferred: u128,
    /// While playing: where the last piece's notes stopped (the beat its
    /// next frame takes from) and the beat that next frame is at.
    last: Option<(f64, f64)>,
    /// Frames in the piece being made.
    frames: usize,
    ons: Vec<On>,
    out: Vec<Out>,
}

/// The main thread's handle on a [`MidiPlayer`]: publishes new sets of
/// clips, frees the ones it hands back, and plays notes by hand.
pub struct MidiFeed {
    mailbox: Arc<Mailbox<MidiClipSet>>,
    returned: Consumer<Box<MidiClipSet>>,
    preview: Producer<Preview>,
}

/// A player for a track's notes slot, and its feed. Off the audio thread: it allocates.
pub fn player() -> (MidiPlayer, MidiFeed) {
    let mailbox = Arc::new(Mailbox::new());
    let (garbage, returned) = RingBuffer::new(RETURN_QUEUE);
    let (preview_tx, preview) = RingBuffer::new(PREVIEW_QUEUE);
    let player = MidiPlayer {
        current: Box::new(MidiClipSet::empty()),
        mailbox: mailbox.clone(),
        garbage,
        spill: Default::default(),
        preview,
        on: 0,
        hand: 0,
        end: [0.0; KEYS],
        started: [u32::MAX; KEYS],
        deferred: 0,
        last: None,
        frames: 0,
        ons: Vec::with_capacity(MAX_ONS),
        out: Vec::with_capacity(2 * MAX_ONS + 4 * KEYS + 2 * PREVIEWS),
    };
    (player, MidiFeed { mailbox, returned, preview: preview_tx })
}

impl MidiFeed {
    /// Hands `set` to the player, replacing any it hasn't taken yet.
    pub fn publish(&mut self, set: MidiClipSet) {
        self.collect();
        drop(self.mailbox.put(Box::new(set)));
    }

    /// Frees the sets the player has finished with.
    pub fn collect(&mut self) {
        while self.returned.pop().is_ok() {}
    }

    /// Plays or lets go of a note by hand, at the player's next piece.
    /// False when the queue is full and it was dropped.
    pub fn preview(&mut self, p: Preview) -> bool {
        self.preview.push(p).is_ok()
    }

    /// Whether the player still exists: false once it was dropped (its
    /// track removed, or lost with the output).
    pub fn is_attached(&self) -> bool {
        Arc::strong_count(&self.mailbox) > 1
    }
}

/// The first frame of the piece at or after `beat`, kept inside its `n` frames.
fn frame_in(ctx: &ProcessContext, n: usize, beat: f64) -> u32 {
    let f = ((beat - ctx.beat) / ctx.beats_per_frame - EDGE_FRAMES).ceil().max(0.0);
    (f as usize).min(n.saturating_sub(1)) as u32
}

const fn bit(key: u8) -> u128 {
    1u128 << (key & 127)
}

impl MidiPlayer {
    fn give_back(&mut self, set: Box<MidiClipSet>) {
        if let Err(PushError::Full(set)) = self.garbage.push(set) {
            match self.spill.iter_mut().find(|s| s.is_none()) {
                Some(place) => *place = Some(set),
                // Can't happen with the feed collecting; leaking beats freeing here.
                None => std::mem::forget(set),
            }
        }
    }

    /// Sends back what is waiting. True when nothing is left waiting.
    fn flush_spill(&mut self) -> bool {
        let mut clear = true;
        for place in &mut self.spill {
            if let Some(set) = place.take() {
                if let Err(PushError::Full(set)) = self.garbage.push(set) {
                    *place = Some(set);
                    clear = false;
                }
            }
        }
        clear
    }

    /// Takes the newest set, if one is waiting. True when it swapped.
    fn take_newest(&mut self) -> bool {
        let Some(next) = self.mailbox.take() else { return false };
        let old = std::mem::replace(&mut self.current, next);
        self.give_back(old);
        true
    }

    /// Queues an event. The list was made with room for every event a piece
    /// can make, so this never grows it.
    fn emit(&mut self, frame: u32, on: bool, key: u8, velocity: f32) {
        if self.out.len() < self.out.capacity() {
            let seq = self.out.len() as u32;
            self.out.push(Out { frame, on, seq, key, velocity });
        }
    }

    /// Ends `key` at `frame`, or just after its start when that is later; on
    /// the next piece's first frame when that falls past this one.
    fn note_off(&mut self, key: u8, frame: u32) {
        let b = bit(key);
        if self.on & b == 0 {
            return;
        }
        self.on &= !b;
        self.hand &= !b;
        let started = self.started[usize::from(key)];
        let frame = if started != u32::MAX && frame <= started { started + 1 } else { frame };
        if frame as usize >= self.frames {
            self.deferred |= b;
        } else {
            self.emit(frame, false, key, 0.0);
        }
    }

    /// Starts `key` at `frame`, for a clip until `end` or by hand. A key
    /// sounding is ended first, or, started on this very frame already,
    /// carries on as the newer note's.
    fn note_on(&mut self, key: u8, frame: u32, velocity: f32, end: Option<f64>) {
        let (b, k) = (bit(key), usize::from(key));
        let set_owner = |p: &mut Self| match end {
            Some(e) => {
                p.hand &= !b;
                p.end[k] = e;
            }
            None => p.hand |= b,
        };
        if self.deferred & b != 0 && self.started[k] == frame {
            // Its end was put off to the next piece: this start on the same frame takes it over.
            self.deferred &= !b;
            self.on |= b;
            set_owner(self);
            return;
        }
        if self.on & b != 0 {
            if self.started[k] == frame {
                set_owner(self);
                return;
            }
            self.note_off(key, frame);
        }
        self.on |= b;
        self.started[k] = frame;
        set_owner(self);
        self.emit(frame, true, key, velocity);
    }

    /// Keys sounding, and ending on this piece's first frame.
    fn busy(&self) -> usize {
        (self.on | self.deferred).count_ones() as usize
    }

    /// A note-off at frame 0 for every sounding clip note.
    fn release_clips(&mut self) {
        let mut keys = self.on & !self.hand;
        while keys != 0 {
            let key = keys.trailing_zeros() as u8;
            keys &= keys - 1;
            self.note_off(key, 0);
        }
    }

    /// After an edit at `now`: notes the new clips hold sounding there carry
    /// on to their new ends; the rest stop at frame 0.
    fn keep_or_release(&mut self, now: f64) {
        let clips = self.on & !self.hand;
        let mut kept: u128 = 0;
        for c in &self.current.clips {
            if c.start >= now {
                break;
            }
            if c.end <= now {
                continue;
            }
            let k = ((now - c.pass(0.0)) / c.content).floor().max(0.0);
            let pass = c.pass(k);
            let upto = c.notes.partition_point(|x| pass + x.at < now);
            for x in &c.notes[..upto] {
                let end = pass + x.at + x.len;
                if end > now && clips & bit(x.key) != 0 {
                    self.end[usize::from(x.key)] = end.min(c.end);
                    kept |= bit(x.key);
                }
            }
        }
        let mut gone = clips & !kept;
        while gone != 0 {
            let key = gone.trailing_zeros() as u8;
            gone &= gone - 1;
            self.note_off(key, 0);
        }
    }

    /// Notes played by hand, at frame 0, while there is room for `room` events in all.
    fn previews(&mut self, room: usize) {
        for _ in 0..PREVIEWS {
            // An `AllOff` can end every key at once.
            if self.out.len() + KEYS + 2 > room {
                break;
            }
            let Ok(p) = self.preview.pop() else { break };
            match p {
                Preview::On { key, velocity } if usize::from(key) < KEYS => {
                    let velocity = if velocity.is_finite() { velocity.clamp(1.0 / 127.0, 1.0) } else { 1.0 };
                    self.note_on(key, 0, velocity, None);
                }
                Preview::Off { key } if usize::from(key) < KEYS && self.hand & bit(key) != 0 => self.note_off(key, 0),
                Preview::AllOff => {
                    let mut keys = self.hand;
                    while keys != 0 {
                        let key = keys.trailing_zeros() as u8;
                        keys &= keys - 1;
                        self.note_off(key, 0);
                    }
                }
                _ => {}
            }
        }
    }

    /// Every note start in `[lo, hi)`, at most `cap`, sorted by beat.
    fn gather(&mut self, lo: f64, hi: f64, cap: usize) {
        self.ons.clear();
        let cap = cap.min(self.ons.capacity());
        'clips: for c in &self.current.clips {
            if c.start >= hi {
                break;
            }
            let (a, b) = (lo.max(c.start), hi.min(c.end));
            if a >= b {
                continue;
            }
            let mut k = ((a - c.pass(0.0)) / c.content).floor().max(0.0);
            while c.pass(k) < b {
                let pass = c.pass(k);
                let from = c.notes.partition_point(|x| pass + x.at < a);
                for x in &c.notes[from..] {
                    let beat = pass + x.at;
                    if beat >= b {
                        break;
                    }
                    if self.ons.len() == cap {
                        break 'clips;
                    }
                    let off = (beat + x.len).min(pass + c.content).min(c.end);
                    self.ons.push(On { beat, off, key: x.key, velocity: x.velocity });
                }
                k += 1.0;
            }
        }
        self.ons.sort_unstable_by(|p, q| p.beat.total_cmp(&q.beat).then(p.key.cmp(&q.key)));
    }
}

impl Device for MidiPlayer {
    /// The output reopened: the next piece starts afresh, and lets go of any
    /// clip note still sounding on the instrument.
    fn prepare(&mut self, _sample_rate: u32, _max_frames: usize) {
        self.last = None;
    }

    fn process(&mut self, ctx: &ProcessContext, events: &mut Events, audio: Buffers<'_>) {
        let n = audio.frames().min(ctx.frames);
        if n == 0 {
            return;
        }
        self.out.clear();
        self.frames = n;
        self.started = [u32::MAX; KEYS];
        // Ends put off from the last piece.
        let mut late = std::mem::take(&mut self.deferred);
        while late != 0 {
            let key = late.trailing_zeros() as u8;
            late &= late - 1;
            self.emit(0, false, key, 0.0);
        }
        let swapped = self.flush_spill() && self.take_newest();
        let room = events.capacity().saturating_sub(events.len());
        let bpf = ctx.beats_per_frame;
        let onward = ctx.playing && !ctx.started && !ctx.jumped && self.last.is_some_and(|(_, next)| (ctx.beat - next).abs() <= 0.5 * bpf);
        // Where this piece's notes start from: on from the last piece, or its first frame.
        let lo = match self.last {
            Some((lo, _)) if onward => lo,
            _ => ctx.beat - EDGE_FRAMES * bpf,
        };
        // Stopped or jumped: every clip note sounding ends here. After an edit, those the new clips don't hold.
        if !onward {
            self.release_clips();
        } else if swapped {
            self.keep_or_release(lo);
        }
        self.previews(room);
        if ctx.playing {
            let hi = ctx.beat + (n as f64 - EDGE_FRAMES) * bpf;
            // Each start can bring one end with it, and each key sounding now ends once.
            let left = room.saturating_sub(self.out.len() + self.busy());
            self.gather(lo, hi, left / 2);
            for i in 0..self.ons.len() {
                let On { beat, off, key, velocity } = self.ons[i];
                let b = bit(key);
                // A clip note still sounding again: it ends where it was to, if that comes first.
                if self.on & b != 0 && self.hand & b == 0 {
                    let end = self.end[usize::from(key)];
                    if end < beat {
                        self.note_off(key, frame_in(ctx, n, end));
                    }
                }
                self.note_on(key, frame_in(ctx, n, beat), velocity, Some(off));
            }
            let mut clips = self.on & !self.hand;
            while clips != 0 {
                let key = clips.trailing_zeros() as u8;
                clips &= clips - 1;
                let end = self.end[usize::from(key)];
                if end < hi {
                    self.note_off(key, frame_in(ctx, n, end));
                }
            }
            self.last = Some((hi, ctx.map.advance(ctx.beat, n as f64)));
        } else {
            self.last = None;
        }

        self.out.sort_unstable_by_key(|o| (o.frame, o.on, o.seq));
        for o in &self.out {
            let (frame, key, channel) = (o.frame, o.key, CHANNEL);
            let e = if o.on { Event::NoteOn { frame, channel, key, velocity: o.velocity } } else { Event::NoteOff { frame, channel, key, velocity: 0.0 } };
            events.push(e);
        }
    }
}

#[cfg(test)]
mod tests;
