//! Audio clips on the Timeline (idevkore/dk-tunekore#36): the player an audio
//! track has in its source slot, and the main thread's handle that feeds it.
//!
//! A clip sits on the beat grid at `start` and plays a span of a file at the
//! file's own speed (clips follow the tempo once Warp lands, Phase 8). Where
//! it is in the file is worked out from the beat alone, as the Sampler's
//! steps are: so a clip lands on the exact frame at its start, its end, a
//! trim, its own loop and the transport's loop wrap, and finds its place
//! after a seek. Its length in beats follows the tempo; at a tempo change
//! while playing, the clips under the playhead move to where the new tempo
//! puts them, crossfaded.
//!
//! The main thread builds a [`ClipSet`] for a track, each clip's audio
//! already at the output's rate, and [`ClipFeed::publish`]es it. The
//! [`ClipPlayer`] takes the newest at the top of a piece and hands the old one
//! back on a queue, to be freed on the main thread. Nothing it does on the
//! audio thread allocates, frees, locks or logs.
//!
//! Clicks: each clip edge gets at least a short fade ([`DECLICK_SECONDS`]),
//! except where the next clip carries on with the very next sample of the
//! same file (a split). Wherever the sound would jump (play, stop, a seek,
//! the transport's loop wrap, a tempo change, an edit while playing) the
//! player crossfades from where it was heading to where it is now over the
//! same short time.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::device::{Buffers, Device, Events};
use crate::player::Track;
use crate::snapshot::Mailbox;
use crate::transport::{ProcessContext, EDGE_FRAMES};

/// The shortest fade at a clip's edge, and the crossfade at a jump.
pub const DECLICK_SECONDS: f64 = 0.0015;
/// Clip sets on their way back to the main thread.
const RETURN_QUEUE: usize = 64;
/// Sets held on the audio thread while the return queue is full.
const SPILL: usize = 4;
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
    /// One or two channels at the set's rate, all at least `offset + content` long.
    audio: Track,
    /// The clip that carries on from this one's last sample, and the one this carries on from.
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
    fn index_at(&self, beat: f64, beats_per_frame: f64) -> f64 {
        ((beat - self.start) / beats_per_frame + EDGE_FRAMES).floor()
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
            list.push(Playing {
                start: spec.start,
                offset,
                content,
                phase: frames_of(spec.phase) % content,
                length,
                fade_in: frames_of(spec.fade_in),
                fade_out: frames_of(spec.fade_out),
                gain: if spec.gain.is_finite() { spec.gain.clamp(0.0, 4.0) } else { 1.0 },
                audio,
                next: NONE,
                prev: NONE,
            });
        }
        list.sort_by(|a, b| a.start.total_cmp(&b.start));
        // A clip that starts where another's file left off (a split) joins it:
        // when they also meet on the same frame, neither gets the declick there.
        for a in 0..list.len() {
            let after = list[a].source(list[a].length);
            let found = (a + 1..list.len()).find(|&b| {
                let (x, y) = (&list[a], &list[b]);
                y.prev == NONE && Arc::ptr_eq(&x.audio, &y.audio) && y.source(0) == after
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

/// How a render is faded over a piece, `pos` frames into a fade `len` long.
#[derive(Debug, Clone, Copy)]
enum Ramp {
    One,
    In { pos: usize, len: usize },
    Out { pos: usize, len: usize },
}

impl Ramp {
    fn gain(self, n: usize) -> f32 {
        match self {
            Ramp::One => 1.0,
            Ramp::In { pos, len } => ((pos + n + 1) as f32 / len as f32).min(1.0),
            Ramp::Out { pos, len } => 1.0 - ((pos + n + 1) as f32 / len as f32).min(1.0),
        }
    }
}

/// Adds frames `0..to` of every clip in `set` into `left` and `right`, for a
/// piece whose first frame is at `beat`. Real-time safe.
fn render(set: &ClipSet, beat: f64, bpf: f64, declick: usize, to: usize, left: &mut [f32], right: &mut [f32], ramp: Ramp) {
    let to = to.min(left.len()).min(right.len());
    if to == 0 || !(bpf > 0.0) || !beat.is_finite() {
        return;
    }
    let clips = &set.clips;
    for c in clips {
        let i0 = c.index_at(beat, bpf);
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
        let joined = |other: u32, expect: i64| {
            other != NONE && {
                let o = &clips[other as usize];
                o.index_at(beat, bpf) as i64 == expect
            }
        };
        let fade_in = if c.prev != NONE && joined(c.prev, i0 + clips[c.prev as usize].length as i64) { c.fade_in } else { c.fade_in.max(declick) };
        let fade_out = if joined(c.next, i0 - c.length as i64) { c.fade_out } else { c.fade_out.max(declick) };
        let (a, b) = (&c.audio[0], &c.audio[c.audio.len() - 1]);
        let mut i = (i0 + from as i64) as usize;
        let mut src = c.source(i);
        let end = c.offset + c.content;
        for n in from..until {
            let mut g = c.gain;
            if i < fade_in {
                g *= (i + 1) as f32 / fade_in as f32;
            }
            let left_over = c.length - i;
            if left_over <= fade_out {
                g *= left_over as f32 / fade_out as f32;
            }
            g *= ramp.gain(n);
            left[n] += a[src] * g;
            right[n] += b[src] * g;
            i += 1;
            src += 1;
            if src == end {
                src = c.offset;
            }
        }
    }
}

/// Which set a crossfade fades out.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Old {
    /// Nothing was sounding (playback just began): only a fade in.
    Nothing,
    Current,
    /// The set an edit replaced.
    Prev,
}

/// A crossfade from where the sound was heading (`beat`, `bpf`) to where it is now.
#[derive(Debug, Clone, Copy)]
struct Fade {
    old: Old,
    beat: f64,
    bpf: f64,
    pos: usize,
}

/// A track's clip player: its source device.
pub struct ClipPlayer {
    rate: u32,
    declick: usize,
    current: Box<ClipSet>,
    prev: Option<Box<ClipSet>>,
    fade: Option<Fade>,
    /// Where the last piece left off: the beat its next frame would be at,
    /// and its beats per frame. `None` when it wasn't playing.
    last: Option<(f64, f64)>,
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
        prev: None,
        fade: None,
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
    /// track removed, or lost with the output).
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

    /// Takes the newest set, if one at this rate is waiting. True when it swapped.
    fn take_newest(&mut self) -> bool {
        let Some(next) = self.mailbox.take() else { return false };
        if next.rate != self.rate {
            self.give_back(next);
            return false;
        }
        let old = std::mem::replace(&mut self.current, next);
        if let Some(p) = self.prev.take() {
            self.give_back(p);
        }
        self.prev = Some(old);
        true
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
            self.prev = None;
            self.spill = Default::default();
            // A waiting set at the new rate goes straight in; one at the old
            // rate is dropped here, on the main thread, and the feed sends again.
            if let Some(next) = self.mailbox.take().filter(|s| s.rate == rate) {
                self.current = next;
            }
        }
        self.fade = None;
        self.last = None;
        self.shared_rate.store(rate, Ordering::Release);
    }

    fn process(&mut self, ctx: &ProcessContext, _events: &mut Events, audio: Buffers<'_>) {
        let n = audio.frames().min(ctx.frames);
        let swapped = self.flush_spill() && self.take_newest();
        let bpf = ctx.beats_per_frame;
        // Where the sound jumps, start a crossfade from where it was heading.
        match (self.last, ctx.playing) {
            (Some((beat, was_bpf)), playing) => {
                let jumped = !playing || ctx.jumped || ctx.started || bpf != was_bpf || (ctx.beat - beat).abs() > 0.5 * bpf;
                if jumped || swapped {
                    self.fade = Some(Fade { old: if swapped { Old::Prev } else { Old::Current }, beat, bpf: was_bpf, pos: 0 });
                }
            }
            (None, true) => self.fade = Some(Fade { old: Old::Nothing, beat: 0.0, bpf: 0.0, pos: 0 }),
            (None, false) => {}
        }
        if swapped && !matches!(self.fade, Some(Fade { old: Old::Prev, .. })) {
            if let Some(p) = self.prev.take() {
                self.give_back(p);
            }
        }

        let len = self.declick;
        if ctx.playing {
            let ramp = self.fade.map_or(Ramp::One, |f| Ramp::In { pos: f.pos, len });
            render(&self.current, ctx.beat, bpf, self.declick, n, &mut *audio.left, &mut *audio.right, ramp);
        }
        if let Some(mut f) = self.fade {
            let old = match f.old {
                Old::Nothing => None,
                Old::Current => Some(&*self.current),
                Old::Prev => self.prev.as_deref(),
            };
            if let Some(set) = old {
                render(set, f.beat, f.bpf, self.declick, (len - f.pos.min(len)).min(n), &mut *audio.left, &mut *audio.right, Ramp::Out { pos: f.pos, len });
            }
            f.pos += n;
            f.beat += n as f64 * f.bpf;
            self.fade = (f.pos < len).then_some(f);
        }
        if self.fade.is_none() {
            if let Some(p) = self.prev.take() {
                self.give_back(p);
            }
        }
        self.last = ctx.playing.then(|| (ctx.beat + n as f64 * bpf, bpf));
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
        assert_eq!(rig.out[100], 2_400.0);
        assert_eq!(rig.out[699], 2_999.0, "the content's last frame");
        assert_eq!(rig.out[700], 2_000.0, "and its first, on the next frame");
        assert_eq!(rig.out[1_700], 2_000.0);
        assert_eq!(rig.out[2_800], 2_100.0);
        assert_eq!(rig.last_sound(), Some(3_499));
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
        assert_eq!(rig.first_sound(), Some(48_000 + 96_000));
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
