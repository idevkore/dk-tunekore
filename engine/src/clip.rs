//! Audio clips on the Timeline (idevkore/dk-tunekore#36): the player an audio
//! track has in its source slot, and the main thread's handle that feeds it.
//!
//! A clip sits on the beat grid at `start` and plays a span of a file at the
//! file's own speed (clips follow the tempo once Warp lands, Phase 8). Where
//! it is in the file is the time the tempo map puts between its start and the
//! beat playing ([`frames_into`], the one place that turns beats into a clip's
//! frames), worked out from the beat alone, as the Sampler's steps are: so a
//! clip lands on the exact frame at its start, its end, a trim, its own loop
//! and the transport's loop wrap, and finds its place after a seek.
//!
//! A tempo change while playing keeps every clip's file running: today's map
//! holds one tempo and no history, so the player remembers where each change
//! it heard landed ([`FileClock`]) and counts time through them, as a map
//! with the Timeline's tempo lane will. A jump (play, a seek, the loop's
//! wrap) or an edit places the clips by the map again. A clip that hasn't
//! started yet starts on its beat at the new tempo.
//!
//! The main thread builds a [`ClipSet`] for a track, each clip's audio
//! already at the output's rate, and [`ClipFeed::publish`]es it. The
//! [`ClipPlayer`] takes the newest at the top of a piece and hands the old one
//! back on a queue, to be freed on the main thread. Nothing it does on the
//! audio thread allocates, frees, locks or logs.
//!
//! Clicks: each clip edge gets at least a short fade ([`DECLICK_SECONDS`]),
//! except where the next clip carries on with the very next frame of the
//! same file at the same gain with no fade of its own (a split). A looped
//! clip's seam is crossfaded into the frames after its content when the file
//! has them, and dipped otherwise. Wherever the sound jumps (play, stop, a
//! seek, the loop's wrap, an edit while playing) what was playing fades out
//! from its level at that moment while the new position fades in, so a jump
//! during another's fade never clicks either.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::device::{Buffers, Device, Events};
use crate::player::Track;
use crate::snapshot::Mailbox;
use crate::transport::{ProcessContext, TempoMap, EDGE_FRAMES};

/// The shortest fade at a clip's edge, the crossfade at a jump, and a looped clip's seam.
pub const DECLICK_SECONDS: f64 = 0.0015;
/// Clip sets on their way back to the main thread.
const RETURN_QUEUE: usize = 64;
/// Sets held on the audio thread while the return queue is full.
const SPILL: usize = 4;
/// Sounds fading out at once after jumps, and sets replaced by edits still in them.
const TAILS: usize = 4;
/// Tempo changes a player remembers between two jumps; one more places the clips again.
const CHANGES: usize = 32;
/// No neighbour.
const NONE: u32 = u32::MAX;

/// One clip, as the main thread describes it. `start` is in beats; the rest
/// is in seconds of the file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipSpec {
    /// Where the clip starts on the Timeline, in beats.
    pub start: f64,
    /// Where in the file the clip's content starts.
    pub offset: f64,
    /// How much of the file, from `offset`, the clip plays and loops over.
    pub content: f64,
    /// Where in the content the clip's first sample is (a looped clip trimmed at its start).
    pub phase: f64,
    /// How long the clip plays. Past the end of the content it loops.
    pub length: f64,
    pub fade_in: f64,
    pub fade_out: f64,
    /// Linear, 0..4.
    pub gain: f32,
}

/// How far into a clip that starts on `start` the sound is at `beat`, in
/// frames: the time the tempo map puts between the two beats, since a clip
/// plays its file at the file's own speed. Every beat-to-clip-frame
/// conversion goes through here.
pub fn frames_into(map: &TempoMap, start: f64, beat: f64) -> f64 {
    map.frames_between(start, beat)
}

/// How many frames a clip that starts on `start` plays before `cut` (beats),
/// counted as the player counts them from beat 0. Cut a clip there and put
/// the rest at `cut`: the second half starts on the frame after the first
/// half's last, and plays the file's next frame.
pub fn frames_before(map: &TempoMap, start: f64, cut: f64) -> u64 {
    let first = |b: f64| (frames_into(map, 0.0, b) - EDGE_FRAMES).ceil();
    (first(cut) - first(start)).max(0.0) as u64
}

/// Cuts a clip at `cut` (beats) into two that play on without a seam: the
/// first plays [`frames_before`] frames and the second carries on from the
/// file's next frame, its times snapped to whole frames at the map's rate.
/// The cut loses the first half's fade-out and the second's fade-in. `None`
/// when `cut` isn't inside the clip.
pub fn split(map: &TempoMap, c: &ClipSpec, cut: f64) -> Option<(ClipSpec, ClipSpec)> {
    let rate = f64::from(map.sample_rate());
    let frames = |s: f64| (s * rate).round();
    let (o, p, k, l) = (frames(c.offset), frames(c.phase), frames(c.content).max(1.0), frames(c.length));
    let n = frames_before(map, c.start, cut) as f64;
    if n < 1.0 || n >= l {
        return None;
    }
    let mut left = ClipSpec { offset: o / rate, phase: p / rate, content: k / rate, length: n / rate, fade_out: 0.0, ..*c };
    let mut right = ClipSpec { start: cut, offset: o / rate, content: k / rate, length: (l - n) / rate, fade_in: 0.0, ..*c };
    if p + l > k {
        // Looped: the second half carries on through the same loop.
        right.phase = ((p + n) % k) / rate;
    } else {
        // Not looped: each half keeps to the part of the file it plays.
        left.content = (p + n) / rate;
        right.offset = (o + p + n) / rate;
        right.content = (k - p - n).max(1.0) / rate;
        right.phase = 0.0;
    }
    left.fade_in = left.fade_in.min(left.length);
    right.fade_out = right.fade_out.min(right.length);
    Some((left, right))
}

/// How a looped clip's seam is smoothed.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Seam {
    /// Crossfaded into the frames after the content, which the file has.
    Cross(usize),
    /// Faded down and up again over this many frames on each side.
    Dip(usize),
}

/// A clip as the audio thread plays it: frames at the set's rate.
#[derive(Debug, Clone)]
struct Playing {
    start: f64,
    offset: usize,
    /// At least 1, and `offset + content` is inside the audio.
    content: usize,
    /// Less than `content`.
    phase: usize,
    length: usize,
    fade_in: usize,
    fade_out: usize,
    gain: f32,
    seam: Seam,
    /// One or two channels at the set's rate, all at least `offset + content` long.
    audio: Track,
    /// The clip that carries on from this one's last frame, and the one this carries on from.
    next: u32,
    prev: u32,
}

impl Playing {
    /// The file frame the clip plays at its frame `i` (any `i`, past its end too).
    fn source(&self, i: usize) -> usize {
        self.offset + (self.phase + i) % self.content
    }

    /// The clip's frame at the first frame of a piece that starts on `beat`
    /// (negative before it starts).
    fn index_at(&self, beat: f64, clock: &FileClock) -> f64 {
        (clock.frames(self.start, beat) + EDGE_FRAMES).floor()
    }
}

/// Every clip on one track, sorted by start, with its audio at `rate`.
/// Built on the main thread.
#[derive(Debug, Default)]
pub struct ClipSet {
    rate: u32,
    clips: Vec<Playing>,
}

/// Frames of the declick fade at `rate`.
fn declick_frames(rate: u32) -> usize {
    ((DECLICK_SECONDS * f64::from(rate)).round() as usize).max(1)
}

impl ClipSet {
    /// No clips, for a player at `rate`.
    pub fn empty(rate: u32) -> Self {
        Self { rate, clips: Vec::new() }
    }

    /// The clips with their audio, which must already be at `rate`. Times are
    /// rounded to the nearest frame and clamped to the audio; a clip with no
    /// audio, a bad start or no length is left out.
    pub fn new(rate: u32, clips: impl IntoIterator<Item = (ClipSpec, Track)>) -> Self {
        let frames_of = |s: f64| if s.is_finite() && s > 0.0 { (s * f64::from(rate)).round() as usize } else { 0 };
        let declick = declick_frames(rate);
        let mut list: Vec<Playing> = Vec::new();
        for (spec, audio) in clips {
            let frames = audio.iter().map(Vec::len).min().unwrap_or(0);
            if frames == 0 || audio.len() > 2 || !spec.start.is_finite() {
                continue;
            }
            let offset = frames_of(spec.offset).min(frames - 1);
            let content = frames_of(spec.content).clamp(1, frames - offset);
            let length = frames_of(spec.length);
            if length == 0 {
                continue;
            }
            // The seam: into what follows the content in the file, or a dip when nothing does.
            let after = frames - (offset + content);
            let seam = if after >= declick && content >= 2 * declick { Seam::Cross(declick) } else { Seam::Dip((declick / 2).min(content / 4).max(1)) };
            list.push(Playing {
                start: spec.start,
                offset,
                content,
                phase: frames_of(spec.phase) % content,
                length,
                fade_in: frames_of(spec.fade_in),
                fade_out: frames_of(spec.fade_out),
                gain: if spec.gain.is_finite() { spec.gain.clamp(0.0, 4.0) } else { 1.0 },
                seam,
                audio,
                next: NONE,
                prev: NONE,
            });
        }
        list.sort_by(|a, b| a.start.total_cmp(&b.start));
        // A clip that carries on with the next frame of the same file, at the
        // same gain and with no fade at the cut (a split), joins the one before:
        // when they also meet on the same frame, neither gets the declick there.
        for a in 0..list.len() {
            let x = &list[a];
            let after = x.source(x.length - 1) + 1;
            let found = (a + 1..list.len()).find(|&b| {
                let y = &list[b];
                y.prev == NONE && Arc::ptr_eq(&x.audio, &y.audio) && y.source(0) == after && x.gain == y.gain && x.fade_out == 0 && y.fade_in == 0
            });
            if let Some(b) = found {
                list[a].next = b as u32;
                list[b].prev = a as u32;
            }
        }
        Self { rate, clips: list }
    }

    /// The output rate the set's audio is at.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn len(&self) -> usize {
        self.clips.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }
}

/// Where clips are in their files: the time between a clip's start and the
/// playhead along the tempo map, through the tempo changes heard since the
/// last jump. Each change is the beat it landed on and the map from there.
#[derive(Debug, Clone, Copy)]
struct FileClock {
    changes: [(f64, TempoMap); CHANGES],
    len: usize,
}

impl FileClock {
    /// No history: clips placed by `map` alone.
    fn new(map: TempoMap) -> Self {
        Self { changes: [(f64::NEG_INFINITY, map); CHANGES], len: 1 }
    }

    fn map(&self) -> &TempoMap {
        &self.changes[self.len - 1].1
    }

    /// A new map from `beat` on. False when no room is left: place the clips again.
    fn change(&mut self, beat: f64, map: TempoMap) -> bool {
        if self.len == CHANGES {
            return false;
        }
        self.changes[self.len] = (beat, map);
        self.len += 1;
        true
    }

    /// Frames from `start` to `beat`, through the changes between them.
    fn frames(&self, start: f64, beat: f64) -> f64 {
        let (since, map) = &self.changes[self.len - 1];
        // Started after the last change (or not started yet): the map alone.
        if start >= *since {
            return frames_into(map, start, beat);
        }
        let mut total = 0.0;
        for k in 0..self.len {
            let (from, map) = &self.changes[k];
            let to = if k + 1 < self.len { self.changes[k + 1].0 } else { f64::INFINITY };
            let (lo, hi) = (start.max(*from), beat.min(to));
            if hi > lo {
                total += frames_into(map, lo, hi);
            }
        }
        total
    }
}

/// How a render is faded over a piece, `pos` frames into a fade `len` long.
#[derive(Debug, Clone, Copy)]
enum Ramp {
    One,
    In { pos: usize, len: usize },
    /// From gain `from` down to silence.
    Out { from: f32, pos: usize, len: usize },
}

impl Ramp {
    fn gain(self, n: usize) -> f32 {
        match self {
            Ramp::One => 1.0,
            Ramp::In { pos, len } => ((pos + n + 1) as f32 / len as f32).min(1.0),
            Ramp::Out { from, pos, len } => from * (1.0 - ((pos + n + 1) as f32 / len as f32).min(1.0)),
        }
    }
}

/// Adds frames `0..to` of every clip in `set` into `left` and `right`, for a
/// piece whose first frame is at `beat`. Real-time safe.
fn render(set: &ClipSet, beat: f64, clock: &FileClock, declick: usize, to: usize, left: &mut [f32], right: &mut [f32], ramp: Ramp) {
    let to = to.min(left.len()).min(right.len());
    if to == 0 || !beat.is_finite() {
        return;
    }
    let clips = &set.clips;
    for c in clips {
        let i0 = c.index_at(beat, clock);
        if i0 + to as f64 <= 0.0 {
            // Not started by the piece's end, and the ones after start later still.
            break;
        }
        if i0 >= c.length as f64 {
            continue;
        }
        let i0 = i0 as i64;
        let from = (-i0).max(0) as usize;
        let until = (c.length as i64 - i0).min(to as i64) as usize;
        if from >= until {
            continue;
        }
        // Joined to a neighbour on this very frame: no declick between them.
        let joined = |other: u32, expect: i64| other != NONE && clips[other as usize].index_at(beat, clock) as i64 == expect;
        let fade_in = if c.prev != NONE && joined(c.prev, i0 + clips[c.prev as usize].length as i64) { c.fade_in } else { c.fade_in.max(declick) };
        let fade_out = if joined(c.next, i0 - c.length as i64) { c.fade_out } else { c.fade_out.max(declick) };
        let (a, b) = (&c.audio[0], &c.audio[c.audio.len() - 1]);
        let mut i = (i0 + from as i64) as usize;
        // Where in the content it is, and whether it has come round the loop yet.
        let mut j = (c.phase + i) % c.content;
        let mut wrapped = c.phase + i >= c.content;
        for n in from..until {
            let mut g = c.gain;
            if i < fade_in {
                g *= (i + 1) as f32 / fade_in as f32;
            }
            let left_over = c.length - i;
            if left_over <= fade_out {
                g *= left_over as f32 / fade_out as f32;
            }
            let src = c.offset + j;
            let (mut l, mut r) = (a[src], b[src]);
            match c.seam {
                // Just past the seam: from what follows the content into its start.
                Seam::Cross(len) if wrapped && j < len => {
                    let w = (j + 1) as f32 / (len + 1) as f32;
                    let past = c.offset + c.content + j;
                    l = l * w + a[past] * (1.0 - w);
                    r = r * w + b[past] * (1.0 - w);
                }
                Seam::Dip(len) => {
                    if wrapped && j < len {
                        g *= (j + 1) as f32 / (len + 1) as f32;
                    }
                    // Before a seam the clip goes on past.
                    let to_seam = c.content - j;
                    if to_seam <= len && i + to_seam < c.length {
                        g *= to_seam as f32 / (len + 1) as f32;
                    }
                }
                _ => {}
            }
            g *= ramp.gain(n);
            left[n] += l * g;
            right[n] += r * g;
            i += 1;
            j += 1;
            if j == c.content {
                j = 0;
                wrapped = true;
            }
        }
    }
}

/// Which set a fading sound plays.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Which {
    Current,
    /// One an edit replaced, held until its tails are done.
    Retired(usize),
}

/// A sound fading out after a jump: where it was heading, from its level then.
#[derive(Debug, Clone, Copy)]
struct Tail {
    set: Which,
    beat: f64,
    clock: FileClock,
    from: f32,
    pos: usize,
}

/// A track's clip player: its source device.
pub struct ClipPlayer {
    rate: u32,
    declick: usize,
    current: Box<ClipSet>,
    /// Sets edits replaced while their sound still fades.
    retired: [Option<Box<ClipSet>>; TAILS],
    tails: [Option<Tail>; TAILS],
    /// How the current set's clips find their place.
    clock: FileClock,
    /// How far the current position's fade in has got, while it runs.
    fade_in: Option<usize>,
    /// The beat the last piece's next frame would be at, while playing.
    last: Option<f64>,
    mailbox: Arc<Mailbox<ClipSet>>,
    garbage: Producer<Box<ClipSet>>,
    /// Sets that didn't fit on the return queue, waiting for room.
    spill: [Option<Box<ClipSet>>; SPILL],
    /// The rate it was last prepared for, for the feed to build sets at.
    shared_rate: Arc<AtomicU32>,
}

/// The main thread's handle on a [`ClipPlayer`]: publishes new sets of
/// clips and frees the ones it hands back.
pub struct ClipFeed {
    mailbox: Arc<Mailbox<ClipSet>>,
    returned: Consumer<Box<ClipSet>>,
    rate: Arc<AtomicU32>,
}

/// A player for a track's source slot, and its feed. The player plays
/// nothing until it is prepared and a set at its rate arrives.
pub fn player() -> (ClipPlayer, ClipFeed) {
    let mailbox = Arc::new(Mailbox::new());
    let (garbage, returned) = RingBuffer::new(RETURN_QUEUE);
    let rate = Arc::new(AtomicU32::new(0));
    let player = ClipPlayer {
        rate: 0,
        declick: 1,
        current: Box::new(ClipSet::empty(0)),
        retired: Default::default(),
        tails: [None; TAILS],
        clock: FileClock::new(TempoMap::constant(120.0, Default::default(), 48_000)),
        fade_in: None,
        last: None,
        mailbox: mailbox.clone(),
        garbage,
        spill: Default::default(),
        shared_rate: rate.clone(),
    };
    (player, ClipFeed { mailbox, returned, rate })
}

impl ClipFeed {
    /// The rate the player was last prepared for, or 0 before it was. Build sets at this rate.
    pub fn rate(&self) -> u32 {
        self.rate.load(Ordering::Acquire)
    }

    /// Hands `set` to the player, replacing any it hasn't taken yet. A set at
    /// a rate other than the player's is handed back unplayed.
    pub fn publish(&mut self, set: ClipSet) {
        self.collect();
        drop(self.mailbox.put(Box::new(set)));
    }

    /// Frees the sets the player has finished with.
    pub fn collect(&mut self) {
        while self.returned.pop().is_ok() {}
    }

    /// Whether the player still exists: false once it was dropped (its
    /// track removed, its slot given to another device, or lost with the output).
    pub fn is_attached(&self) -> bool {
        Arc::strong_count(&self.mailbox) > 1
    }
}

impl ClipPlayer {
    /// Hands a set back to the main thread, or holds it until there is room.
    fn give_back(&mut self, set: Box<ClipSet>) {
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

    /// Takes the newest set, if one at this rate is waiting, and keeps the
    /// one it replaces while tails still play it. Returns the slot the
    /// replaced set went to when it swapped.
    fn take_newest(&mut self) -> Option<usize> {
        let next = self.mailbox.take()?;
        if next.rate != self.rate {
            self.give_back(next);
            return None;
        }
        let old = std::mem::replace(&mut self.current, next);
        // A free slot for it, or the one with the faintest tails, cut short.
        let slot = (0..TAILS).find(|&i| self.retired[i].is_none()).unwrap_or(0);
        if let Some(gone) = self.retired[slot].take() {
            for t in &mut self.tails {
                if t.is_some_and(|t| t.set == Which::Retired(slot)) {
                    *t = None;
                }
            }
            self.give_back(gone);
        }
        self.retired[slot] = Some(old);
        for t in self.tails.iter_mut().flatten() {
            if t.set == Which::Current {
                t.set = Which::Retired(slot);
            }
        }
        // Played by tails or by the fade out that follows; given back once nothing plays it.
        Some(slot)
    }

    /// Starts fading out what was heading for `beat`, from its level now.
    fn add_tail(&mut self, tail: Tail) {
        let place = match self.tails.iter().position(|t| t.is_none()) {
            Some(i) => i,
            // Full: the one furthest through its fade makes room.
            None => (0..TAILS).max_by_key(|&i| self.tails[i].map_or(0, |t| t.pos)).unwrap_or(0),
        };
        self.tails[place] = Some(tail);
    }

    /// Gives back the replaced sets no tail plays any more.
    fn free_retired(&mut self) {
        for i in 0..TAILS {
            if self.retired[i].is_some() && !self.tails.iter().flatten().any(|t| t.set == Which::Retired(i)) {
                if let Some(set) = self.retired[i].take() {
                    self.give_back(set);
                }
            }
        }
    }
}

impl Device for ClipPlayer {
    /// A new rate empties the player: its audio was at the old one. The feed
    /// sees the rate and sends a new set.
    fn prepare(&mut self, sample_rate: u32, _max_frames: usize) {
        let rate = sample_rate.max(1);
        if rate != self.rate || self.current.rate != rate {
            self.rate = rate;
            self.declick = declick_frames(rate);
            self.current = Box::new(ClipSet::empty(rate));
            self.spill = Default::default();
            // A waiting set at the new rate goes straight in; one at the old
            // rate is dropped here, on the main thread, and the feed sends again.
            if let Some(next) = self.mailbox.take().filter(|s| s.rate == rate) {
                self.current = next;
            }
        }
        self.retired = Default::default();
        self.tails = [None; TAILS];
        self.fade_in = None;
        self.last = None;
        self.shared_rate.store(rate, Ordering::Release);
    }

    fn process(&mut self, ctx: &ProcessContext, _events: &mut Events, audio: Buffers<'_>) {
        let n = audio.frames().min(ctx.frames);
        let len = self.declick;
        let swapped = if self.flush_spill() { self.take_newest() } else { None };
        // The level the current position plays at now: part way up its fade in, or full.
        let level = self.fade_in.map_or(1.0, |pos| (pos as f32 / len as f32).min(1.0));
        let was = self.last;
        let onward = was.is_some_and(|b| ctx.playing && !ctx.jumped && !ctx.started && (ctx.beat - b).abs() <= 0.5 * ctx.beats_per_frame);
        // A tempo change heard while playing on: the files keep running.
        let mut place = !onward || swapped.is_some();
        if !place && ctx.map != *self.clock.map() && !self.clock.change(ctx.beat, ctx.map) {
            place = true;
        }
        if let (Some(beat), true) = (was, place) {
            // What was playing (the replaced set, after an edit) fades out from where it was heading.
            let set = swapped.map_or(Which::Current, Which::Retired);
            self.add_tail(Tail { set, beat, clock: self.clock, from: level, pos: 0 });
        }
        if ctx.playing && (place || was.is_none()) {
            self.clock = FileClock::new(ctx.map);
            self.fade_in = Some(0);
        }

        if ctx.playing {
            let ramp = self.fade_in.map_or(Ramp::One, |pos| Ramp::In { pos, len });
            render(&self.current, ctx.beat, &self.clock, len, n, &mut *audio.left, &mut *audio.right, ramp);
            self.fade_in = self.fade_in.map(|p| p + n).filter(|&p| p < len);
        }
        for k in 0..TAILS {
            let Some(mut t) = self.tails[k] else { continue };
            let set = match t.set {
                Which::Current => Some(&*self.current),
                Which::Retired(i) => self.retired[i].as_deref(),
            };
            if let Some(set) = set {
                render(set, t.beat, &t.clock, len, (len - t.pos.min(len)).min(n), &mut *audio.left, &mut *audio.right, Ramp::Out { from: t.from, pos: t.pos, len });
            }
            t.pos += n;
            t.beat = t.clock.map().advance(t.beat, n as f64);
            self.tails[k] = (t.pos < len).then_some(t);
        }
        self.free_retired();
        self.last = ctx.playing.then(|| ctx.map.advance(ctx.beat, n as f64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Mixer;
    use crate::testing::realtime;
    use crate::transport::{Transport, TransportShared};

    const RATE: u32 = 48_000;
    /// Frames per beat at 120 BPM.
    const BEAT: usize = 24_000;

    fn dc(level: f32, frames: usize) -> Track {
        Arc::new(vec![vec![level; frames]])
    }

    /// Sample `i` is `i`: shows exactly which file frame plays where.
    fn ramp(frames: usize) -> Track {
        Arc::new(vec![(0..frames).map(|i| i as f32).collect()])
    }

    fn spec(start: f64, seconds: f64) -> ClipSpec {
        ClipSpec { start, offset: 0.0, content: seconds, phase: 0.0, length: seconds, fade_in: 0.0, fade_out: 0.0, gain: 1.0 }
    }

    fn secs(frames: usize) -> f64 {
        frames as f64 / f64::from(RATE)
    }

    struct Rig {
        player: ClipPlayer,
        feed: ClipFeed,
        shared: TransportShared,
        clock: Transport,
        events: Events,
        left: Vec<f32>,
        right: Vec<f32>,
        /// Everything played so far, left channel.
        out: Vec<f32>,
    }

    impl Rig {
        fn new(clips: Vec<(ClipSpec, Track)>) -> Self {
            let (mut player, mut feed) = player();
            player.prepare(RATE, 4096);
            assert_eq!(feed.rate(), RATE);
            feed.publish(ClipSet::new(RATE, clips));
            Self {
                player,
                feed,
                shared: TransportShared::default(),
                clock: Transport::new(RATE),
                events: Events::with_capacity(8),
                left: vec![0.0; 4096],
                right: vec![0.0; 4096],
                out: Vec::with_capacity(1 << 20),
            }
        }

        /// Runs one block of `frames`, as the graph does: each piece starts silent.
        fn block(&mut self, frames: usize) {
            let Rig { player, shared, clock, events, left, right, out, .. } = self;
            clock.run(shared, frames, |ctx, off| {
                let (l, r) = (&mut left[off..off + ctx.frames], &mut right[off..off + ctx.frames]);
                l.fill(0.0);
                r.fill(0.0);
                player.process(ctx, events, Buffers { left: l, right: r });
            });
            out.extend_from_slice(&left[..frames]);
        }

        fn run(&mut self, frames: usize, block: usize) {
            let mut done = 0;
            while done < frames {
                let n = block.min(frames - done);
                self.block(n);
                done += n;
            }
        }

        fn first_sound(&self) -> Option<usize> {
            self.out.iter().position(|&x| x != 0.0)
        }

        fn last_sound(&self) -> Option<usize> {
            self.out.iter().rposition(|&x| x != 0.0)
        }
    }

    #[test]
    fn a_clip_starts_and_ends_on_its_frames() {
        for block in [64, 512, 1000] {
            // A beat in, half a second (24,000 frames) long.
            let mut rig = Rig::new(vec![(spec(1.0, 0.5), dc(1.0, 48_000))]);
            rig.shared.play();
            rig.run(3 * BEAT, block);
            assert_eq!(rig.first_sound(), Some(BEAT), "block {block}");
            assert_eq!(rig.last_sound(), Some(BEAT + 24_000 - 1), "block {block}");
            // Between its declicks it plays at full level.
            assert!(rig.out[BEAT + 100..BEAT + 23_000].iter().all(|&x| x == 1.0));
        }
    }

    #[test]
    fn a_start_between_frames_lands_on_the_next_one() {
        // 1.00001 beats is 24,000.24 frames: the clip's first frame is 24,001.
        let mut rig = Rig::new(vec![(spec(1.00001, 0.1), ramp(48_000)), (spec(1.00001, 0.1), dc(1.0, 48_000))]);
        rig.shared.play();
        rig.run(2 * BEAT, 512);
        assert_eq!(rig.first_sound(), Some(BEAT + 1));
        assert_eq!(rig.out[BEAT + 1 + 200], 201.0, "frame 200 of the file (plus the other clip's 1), 200 frames in");
    }

    #[test]
    fn trims_read_the_right_part_of_the_file() {
        // Trimmed at the start: the content starts 1,000 frames into the file.
        let s = ClipSpec { offset: secs(1_000), content: secs(5_000), length: secs(5_000), ..spec(0.0, 0.0) };
        let mut rig = Rig::new(vec![(s, ramp(48_000))]);
        rig.shared.play();
        rig.run(8_000, 512);
        assert_eq!(rig.first_sound(), Some(0));
        assert_eq!(rig.out[500], 1_500.0);
        assert_eq!(rig.last_sound(), Some(4_999), "trimmed at the end too");
        assert_eq!(rig.out[4_900], 5_900.0);
        assert!(rig.out[4_990] < 5_990.0, "the end fades out");
    }

    #[test]
    fn a_looped_clip_wraps_on_the_frame() {
        // 1,000 frames of content from frame 2,000, played for 3,500 frames, starting 300 into it.
        let s = ClipSpec { offset: secs(2_000), content: secs(1_000), phase: secs(300), length: secs(3_500), ..spec(0.0, 0.0) };
        let mut rig = Rig::new(vec![(s, ramp(48_000))]);
        rig.shared.play();
        rig.run(4_000, 256);
        let x = declick_frames(RATE);
        assert_eq!(rig.out[100], 2_400.0);
        assert_eq!(rig.out[699], 2_999.0, "the content's last frame");
        // On the next frame the content's first, crossfaded in from what follows the content in the file.
        for j in 0..x {
            let w = (j + 1) as f32 / (x + 1) as f32;
            let want = (2_000 + j) as f32 * w + (3_000 + j) as f32 * (1.0 - w);
            assert!((rig.out[700 + j] - want).abs() < 0.01, "seam frame {j}: {} not {want}", rig.out[700 + j]);
        }
        assert_eq!(rig.out[700 + x], (2_000 + x) as f32, "past the crossfade, the content's start");
        assert_eq!(rig.out[1_700 + x], (2_000 + x) as f32, "and again on the next pass");
        assert_eq!(rig.out[2_800], 2_100.0);
        assert_eq!(rig.last_sound(), Some(3_499));
    }

    #[test]
    fn a_whole_file_loop_dips_at_its_seam_instead_of_cutting() {
        // A 1 kHz sine whose loop point isn't at a zero crossing.
        let sine: Track = Arc::new(vec![(0..1_030).map(|i| (i as f32 * std::f32::consts::TAU * 1_000.0 / RATE as f32).sin()).collect()]);
        let s = ClipSpec { content: secs(1_030), length: secs(4_000), ..spec(0.0, 0.0) };
        let mut rig = Rig::new(vec![(s, sine)]);
        rig.shared.play();
        rig.run(4_000, 512);
        let step = rig.out[200..3_900].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        // The sine itself moves up to 0.131 a frame; a cut at the seam would jump about 1.
        assert!(step < 0.2, "step {step}");
    }

    #[test]
    fn the_transport_loop_wraps_clips_on_the_frame() {
        // Loop two beats, and a clip from beat 0 for three seconds.
        let mut rig = Rig::new(vec![(spec(0.0, 3.0), ramp(4 * 48_000))]);
        rig.shared.set_loop(0.0, 2.0);
        rig.shared.set_looping(true);
        rig.shared.play();
        rig.run(3 * 2 * BEAT, 500);
        let fade = declick_frames(RATE);
        for pass in 0..3 {
            let at = pass * 2 * BEAT;
            assert_eq!(rig.out[at + fade + 10], (fade + 10) as f32, "pass {pass}: the clip's frame 0 is the loop's frame 0");
            assert_eq!(rig.out[at + 2 * BEAT - 1], (2 * BEAT - 1) as f32, "pass {pass}: full level up to the wrap");
        }
        // Across the wrap: the old position fades out while the new fades in, so no step bigger than a fade's.
        let at = 2 * BEAT;
        let step = rig.out[at - 1..at + fade + 1].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(step < (2 * BEAT) as f32 / fade as f32 + 2.0, "step {step}");
    }

    #[test]
    fn a_clip_at_the_loop_start_starts_on_the_wrap_frame() {
        let mut rig = Rig::new(vec![(spec(0.0, 0.25), dc(1.0, 48_000))]);
        rig.shared.set_loop(0.0, 1.0);
        rig.shared.set_looping(true);
        rig.shared.play();
        rig.run(2 * BEAT, 512);
        assert_eq!(rig.out[BEAT - 1], 0.0, "silent before the wrap: the clip ended");
        assert!(rig.out[BEAT] > 0.0, "and back on the wrap's frame");
        assert_eq!(rig.out[BEAT + 1_000], 1.0);
    }

    #[test]
    fn tempo_changes_move_clips_with_the_beat() {
        let mut rig = Rig::new(vec![(spec(4.0, 0.1), dc(1.0, 48_000))]);
        rig.shared.set_tempo(90.0);
        rig.shared.play();
        rig.run(150_000, 512);
        assert_eq!(rig.first_sound(), Some(128_000), "beat 4 at 90 BPM");

        // At 120 to beat 2 (48,000 frames), then 60: two more beats take 96,000 frames.
        let mut rig = Rig::new(vec![(spec(4.0, 0.1), dc(1.0, 48_000))]);
        rig.shared.play();
        rig.run(2 * BEAT, 480);
        rig.shared.set_tempo(60.0);
        rig.run(200_000, 480);
        assert_eq!(rig.first_sound(), Some(48_000 + 96_000), "a clip that hadn't started starts on its beat at the new tempo");
    }

    #[test]
    fn a_tempo_change_keeps_a_playing_clip_running_and_a_seek_places_it_again() {
        let mut rig = Rig::new(vec![(spec(0.0, 3.0), ramp(4 * 48_000))]);
        rig.shared.play();
        rig.run(BEAT, 480);
        rig.shared.set_tempo(60.0);
        rig.shared.set_tempo(90.0);
        rig.run(BEAT, 480);
        rig.shared.set_tempo(75.0);
        rig.run(BEAT, 480);
        let x = declick_frames(RATE);
        for k in x + 1..3 * BEAT {
            assert_eq!(rig.out[k], k as f32, "frame {k}: the file runs on through two tempo changes, no jump and no fade");
        }
        // A seek places it by the map again: beat 1 at 75 BPM is 0.8 s into the clip.
        rig.shared.seek(1.0);
        rig.run(2_000, 500);
        let at = 3 * BEAT;
        assert_eq!(rig.out[at + x + 100], (38_400 + x + 100) as f32);
        // And a clip that starts after a change still starts on its beat.
        let mut rig = Rig::new(vec![(spec(0.0, 3.0), dc(0.25, 4 * 48_000)), (spec(3.0, 0.1), dc(0.5, 48_000))]);
        rig.shared.play();
        rig.run(BEAT, 480);
        rig.shared.set_tempo(60.0);
        rig.run(200_000, 480);
        // The first clip plays 0.25 throughout; the second's first frame adds to it.
        let second = rig.out.iter().position(|&x| x > 0.2501).unwrap();
        assert_eq!(second, BEAT + 2 * 48_000, "beat 3 after a change at beat 1 to 60 BPM");
    }

    #[test]
    fn a_second_jump_during_a_crossfade_does_not_click() {
        let sine: Track = Arc::new(vec![(0..4 * 48_000).map(|i| (i as f32 * std::f32::consts::TAU * 1_000.0 / RATE as f32).sin()).collect()]);
        let mut rig = Rig::new(vec![(spec(0.0, 4.0), sine)]);
        rig.shared.play();
        rig.run(4_800, 24);
        rig.shared.seek(1.37);
        rig.run(24, 24);
        // 24 frames into the first crossfade, another.
        rig.shared.seek(2.71);
        rig.run(4_800, 24);
        let step = rig.out[100..].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(step < 0.2, "step {step}: the sine alone moves 0.131 a frame");
    }

    #[test]
    fn a_cut_with_a_gain_change_or_a_fade_isnt_joined_and_doesnt_step() {
        let audio = dc(1.0, 48_000);
        let a = ClipSpec { length: secs(10_000), ..spec(0.0, 1.0) };
        let b = ClipSpec { start: 10_000.0 / BEAT as f64, phase: secs(10_000), length: secs(20_000), ..spec(0.0, 1.0) };
        for (a, b, what) in [
            (a, ClipSpec { gain: 0.25, ..b }, "a quieter second half"),
            (ClipSpec { fade_out: 0.05, ..a }, b, "a fade out on the first half"),
            (a, ClipSpec { fade_in: 0.05, ..b }, "a fade in on the second half"),
        ] {
            let set = ClipSet::new(RATE, vec![(a, audio.clone()), (b, audio.clone())]);
            assert_eq!(set.clips[0].next, NONE, "{what}: not joined");
            let mut rig = Rig::new(vec![(a, audio.clone()), (b, audio.clone())]);
            rig.shared.play();
            rig.run(40_000, 512);
            let step = rig.out[200..30_000].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
            assert!(step < 0.05, "{what}: step {step}");
        }
    }

    #[test]
    fn splits_of_looped_and_unlooped_clips_play_through() {
        let map = TempoMap::constant(97.0, crate::transport::TimeSignature::default(), RATE);
        let audio = dc(1.0, 3 * 48_000);
        // Not looped (the content is just what plays), and looped twice over a second.
        for (c, what) in [(ClipSpec { offset: 0.1, ..spec(0.5, 1.2) }, "unlooped"), (ClipSpec { offset: 0.1, content: 1.0, phase: 0.3, length: 2.5, ..spec(0.5, 0.0) }, "looped")] {
            let (left, right) = split(&map, &c, 1.9).unwrap();
            let set = ClipSet::new(RATE, vec![(left, audio.clone()), (right, audio.clone())]);
            assert_eq!((set.clips[0].next, set.clips[1].prev), (1, 0), "{what}: joined");
            let mut rig = Rig::new(vec![(left, audio.clone()), (right, audio.clone())]);
            rig.shared.set_tempo(97.0);
            rig.shared.play();
            let end = (map.frame_of_beat(0.5) + (left.length + right.length) * f64::from(RATE)) as usize;
            rig.run(end + 1_000, 512);
            let from = (map.frame_of_beat(0.5) - EDGE_FRAMES).ceil() as usize + declick_frames(RATE) + 1;
            let cut = (map.frame_of_beat(1.9) - EDGE_FRAMES).ceil() as usize;
            if what == "unlooped" {
                assert!(rig.out[from..cut + 2_000].iter().all(|&x| x == 1.0), "{what}: a seam at the cut");
            } else {
                // A looped clip dips only at its own seams, never at the cut.
                assert!(rig.out[cut - 50..cut + 50].iter().all(|&x| x == 1.0), "{what}: a seam at the cut");
            }
        }
        assert!(split(&map, &spec(0.5, 1.0), 0.4).is_none(), "before it");
        assert!(split(&map, &spec(0.5, 1.0), 9.0).is_none(), "after it");
    }

    #[test]
    fn overlapping_clips_sum() {
        let mut rig = Rig::new(vec![(spec(0.0, 1.0), dc(0.25, 96_000)), (spec(0.5, 1.0), dc(0.5, 96_000))]);
        rig.shared.play();
        rig.run(4 * BEAT, 512);
        assert_eq!(rig.out[5_000], 0.25);
        assert_eq!(rig.out[BEAT / 2 * 2 + 5_000], 0.75, "both");
        assert_eq!(rig.out[48_000 + 5_000], 0.5, "the first ended");
    }

    #[test]
    fn a_split_plays_through_without_a_dip() {
        // One clip cut at frame 10,000 into two that carry on in the file.
        let a = ClipSpec { length: secs(10_000), ..spec(0.0, 1.0) };
        let b = ClipSpec { start: 10_000.0 / BEAT as f64, phase: secs(10_000), length: secs(20_000), ..spec(0.0, 1.0) };
        let audio = dc(1.0, 48_000);
        let set = ClipSet::new(RATE, vec![(a, audio.clone()), (b, audio.clone())]);
        assert_eq!((set.clips[0].next, set.clips[1].prev), (1, 0), "joined");
        let mut rig = Rig::new(vec![(a, audio.clone()), (b, audio)]);
        rig.shared.play();
        rig.run(40_000, 512);
        assert!(rig.out[200..29_000].iter().all(|&x| x == 1.0), "no fade at the cut");
        assert_eq!(rig.last_sound(), Some(29_999));
    }

    #[test]
    fn a_cut_where_frames_before_says_plays_through_at_any_tempo_and_rate() {
        // Clip starts and cuts between frames, at rates and tempos where beats aren't whole frames.
        for (rate, tempo, start, cut) in [(44_100u32, 97.0, 1.00001, 1.3), (48_000, 133.3, 0.37, 2.0), (96_000, 61.0, 0.0, 0.123)] {
            let map = TempoMap::constant(tempo, crate::transport::TimeSignature::default(), rate);
            let n = frames_before(&map, start, cut) as f64;
            let r = f64::from(rate);
            let audio = dc(1.0, rate as usize * 4);
            let a = ClipSpec { start, offset: 0.0, content: 4.0, phase: 0.0, length: n / r, fade_in: 0.0, fade_out: 0.0, gain: 1.0 };
            let b = ClipSpec { start: cut, phase: n / r, length: 1.0, ..a };
            let (mut player, mut feed) = player();
            player.prepare(rate, 4096);
            feed.publish(ClipSet::new(rate, vec![(a, audio.clone()), (b, audio)]));
            let shared = TransportShared::default();
            shared.set_tempo(tempo);
            shared.play();
            let mut clock = Transport::new(rate);
            let (mut l, mut rr) = (vec![0.0f32; 4096], vec![0.0f32; 4096]);
            let mut events = Events::with_capacity(4);
            let mut out = Vec::new();
            let frames = map.frame_of_beat(cut) as usize + rate as usize / 2;
            let mut done = 0;
            while done < frames {
                let len = 512.min(frames - done);
                clock.run(&shared, len, |ctx, off| {
                    let (x, y) = (&mut l[off..off + ctx.frames], &mut rr[off..off + ctx.frames]);
                    x.fill(0.0);
                    y.fill(0.0);
                    player.process(ctx, &mut events, Buffers { left: x, right: y });
                });
                out.extend_from_slice(&l[..len]);
                done += len;
            }
            let from = (map.frame_of_beat(start) - EDGE_FRAMES).ceil() as usize + declick_frames(rate) + 1;
            let at = (map.frame_of_beat(cut) - EDGE_FRAMES).ceil() as usize;
            assert!(out[from..at + 1_000].iter().all(|&x| x == 1.0), "{rate} Hz, {tempo} BPM: a seam at the cut");
        }
    }

    #[test]
    fn stopping_fades_out_instead_of_cutting() {
        let mut rig = Rig::new(vec![(spec(0.0, 2.0), dc(1.0, 96_000))]);
        rig.shared.play();
        rig.run(10_000, 500);
        rig.shared.stop();
        rig.run(1_000, 500);
        let fade = declick_frames(RATE);
        let tail = &rig.out[10_000..10_000 + fade + 10];
        assert!(tail[0] > 0.9 && tail[0] < 1.0, "{}", tail[0]);
        assert!(tail.windows(2).all(|w| w[1] <= w[0]), "falls");
        assert_eq!(tail[fade], 0.0, "silent after the fade");
    }

    #[test]
    fn an_edit_while_playing_crossfades_and_an_unchanged_set_sounds_the_same() {
        let clips = || vec![(spec(0.0, 2.0), ramp(96_000))];
        let mut rig = Rig::new(clips());
        rig.shared.play();
        rig.run(5_000, 500);
        rig.feed.publish(ClipSet::new(RATE, clips()));
        rig.run(5_000, 500);
        for (i, &x) in rig.out[200..10_000].iter().enumerate() {
            assert!((x - (i + 200) as f32).abs() < 0.01, "frame {}: {x}", i + 200);
        }
        // Removing the clip fades it out.
        rig.feed.publish(ClipSet::empty(RATE));
        rig.run(1_000, 500);
        assert!(rig.out[10_000] > 0.0 && rig.out[10_000 + declick_frames(RATE)] == 0.0);
    }

    #[test]
    fn a_seek_into_a_clip_fades_in() {
        let mut rig = Rig::new(vec![(spec(0.0, 2.0), dc(1.0, 96_000))]);
        rig.shared.seek(1.0);
        rig.shared.play();
        rig.run(500, 500);
        let fade = declick_frames(RATE);
        assert!(rig.out[0] > 0.0 && rig.out[0] < 0.1, "{}", rig.out[0]);
        assert_eq!(rig.out[fade], 1.0);
    }

    #[test]
    fn a_set_at_another_rate_is_handed_back_unplayed() {
        let mut rig = Rig::new(vec![]);
        rig.feed.publish(ClipSet::new(44_100, vec![(spec(0.0, 1.0), dc(1.0, 44_100))]));
        rig.shared.play();
        rig.run(2_000, 500);
        assert_eq!(rig.first_sound(), None);
        // Preparing at a new rate empties the player and tells the feed.
        rig.player.prepare(44_100, 4096);
        assert_eq!(rig.feed.rate(), 44_100);
        assert!(rig.feed.is_attached());
        drop(rig.player);
        assert!(!rig.feed.is_attached());
    }

    #[test]
    fn bad_times_are_clamped_or_left_out() {
        let audio = dc(1.0, 1_000);
        let set = ClipSet::new(
            RATE,
            vec![
                (ClipSpec { offset: 99.0, content: f64::NAN, ..spec(0.0, 1.0) }, audio.clone()),
                (spec(f64::INFINITY, 1.0), audio.clone()),
                (spec(0.0, 0.0), audio.clone()),
                (spec(0.0, 1.0), Arc::new(Vec::new())),
            ],
        );
        assert_eq!(set.len(), 1);
        let c = &set.clips[0];
        assert_eq!((c.offset, c.content), (999, 1), "inside the audio");
    }

    #[test]
    fn playing_clips_never_allocate_on_the_audio_thread() {
        let mut rig = Rig::new(vec![(spec(0.0, 1.0), ramp(96_000)), (spec(2.0, 0.5), dc(0.5, 48_000))]);
        rig.shared.set_loop(0.0, 4.0);
        rig.shared.set_looping(true);
        rig.shared.play();
        let mut total = 0;
        for i in 0..300 {
            match i % 6 {
                // The main thread edits between blocks, as the UI does.
                0 => rig.feed.publish(ClipSet::new(RATE, vec![(spec((i % 4) as f64, 0.5), ramp(96_000))])),
                1 => rig.shared.seek((i % 3) as f64),
                2 => rig.shared.set_tempo(100.0 + (i % 5) as f64 * 10.0),
                3 => rig.shared.stop(),
                4 => rig.shared.play(),
                _ => rig.feed.collect(),
            }
            let Rig { player, shared, clock, events, left, right, .. } = &mut rig;
            let (_, n) = realtime(|| {
                clock.run(shared, 333, |ctx, off| {
                    let (l, r) = (&mut left[off..off + ctx.frames], &mut right[off..off + ctx.frames]);
                    l.fill(0.0);
                    r.fill(0.0);
                    player.process(ctx, events, Buffers { left: l, right: r });
                })
            });
            total += n;
        }
        assert_eq!(total, 0, "the clip player allocated or freed {total} times");
    }

    #[test]
    fn a_clip_and_a_sampler_step_on_the_same_beat_start_on_the_same_frame() {
        use crate::sampler::{self, PadParams, SamplerShared};
        use crate::track::TrackMix;
        for (rate, tempo) in [(44_100u32, 97.0), (48_000, 120.0), (96_000, 133.3)] {
            // The first frame with sound, with only the Sampler's track heard or only the clip's.
            let first = |sampler_heard: bool| {
                let shared = Arc::new(TransportShared::default());
                shared.set_tempo(tempo);
                let pads = Arc::new(SamplerShared::with_clock(shared.clone()));
                pads.set_pad(0, &PadParams::default());
                pads.set_pattern(0, 1 << 4); // step 5 of 16: beat 1
                let mut mixer = Mixer::new();
                let s = mixer.add_track("Sampler").unwrap();
                let a = mixer.add_track("Audio").unwrap();
                let (device, mut link) = sampler::device(pads.clone(), rate);
                mixer.set_source(s, Some(Box::new(device))).unwrap();
                let (clips, mut feed) = player();
                mixer.set_source(a, Some(Box::new(clips))).unwrap();
                let mut graph = mixer.start(rate);
                link.send(0, Some(Arc::new(vec![vec![0.5; 2_000]]))).unwrap();
                feed.publish(ClipSet::new(rate, vec![(spec(1.0, 0.05), dc(0.5, rate as usize))]));
                mixer.set_mix(if sampler_heard { a } else { s }, TrackMix { mute: true, ..Default::default() }).unwrap();
                let mut clock = Transport::new(rate);
                shared.play();
                let mut out = Vec::new();
                for _ in 0..200 {
                    graph.run(&mut clock, &shared, 512);
                    out.extend_from_slice(graph.master().0);
                }
                // Past the muted track's 8 ms ramp down.
                out.iter().skip(1_000).position(|&x| x.abs() > 1e-9).map(|i| i + 1_000)
            };
            let beat = (60.0 / tempo * f64::from(rate) - EDGE_FRAMES).ceil() as usize;
            assert_eq!(first(true), Some(beat), "the Sampler at {rate} Hz, {tempo} BPM");
            assert_eq!(first(false), Some(beat), "the clip at {rate} Hz, {tempo} BPM");
        }
    }

    #[test]
    fn on_a_track_in_the_graph_with_edits_between_blocks() {
        let mut mixer = Mixer::new();
        let t = mixer.add_track("Audio 1").unwrap();
        let (player, mut feed) = player();
        let mut graph = mixer.start(RATE);
        mixer.set_source(t, Some(Box::new(player))).unwrap();
        assert_eq!(feed.rate(), RATE, "prepared as it went in");
        let shared = TransportShared::default();
        let mut clock = Transport::new(RATE);
        shared.play();
        let mut total = 0;
        for i in 0..100 {
            if i % 3 == 0 {
                feed.publish(ClipSet::new(RATE, vec![(spec(0.0, 10.0), dc(0.5, 10 * 48_000))]));
            }
            let (_, n) = realtime(|| graph.run(&mut clock, &shared, 512));
            total += n;
            mixer.collect();
        }
        assert_eq!(total, 0);
        let (l, _) = graph.master();
        assert_eq!(l[511], 0.5, "it plays through the track");
        drop(graph);
        assert!(mixer.stopped());
        assert!(feed.is_attached(), "home with the mixer");
    }
}
