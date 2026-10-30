//! The transport: the one beat clock every track runs on.
//!
//! The control side sets the tempo, the time signature, the loop and play or
//! stop through [`TransportShared`] (atomics, any thread). The audio thread's
//! [`Transport`] reads them once per block, moves the position, and hands each
//! device a [`ProcessContext`] that says where its piece of the block sits in
//! time. A block is split where the loop wraps, so time runs straight inside
//! one context: frame `n` of it is at beat `beat + n * beats_per_frame`.
//!
//! Positions are in beats (quarter notes), as VST3 and CLAP count them. A
//! tempo change takes effect at the next block, from the beat the clock has
//! reached, so nothing jumps.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// The slowest and fastest tempo, in BPM.
pub const MIN_TEMPO: f64 = 20.0;
pub const MAX_TEMPO: f64 = 999.0;
/// A boundary this close (in frames) after a frame still lands on that frame,
/// so float noise never pushes an exact boundary one frame late.
pub const EDGE_FRAMES: f64 = 1.0e-6;
/// Bit pattern meaning "no seek waiting". Never a beat the control side can store.
const NO_SEEK: u64 = u64::MAX;

/// Beats per bar and the note value of a beat, as in 4/4 or 6/8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSignature {
    pub numerator: u32,
    pub denominator: u32,
}

impl Default for TimeSignature {
    fn default() -> Self {
        Self { numerator: 4, denominator: 4 }
    }
}

impl TimeSignature {
    /// A bar's length in quarter notes: 4 in 4/4, 3 in 6/8.
    pub fn bar_beats(&self) -> f64 {
        f64::from(self.numerator) * 4.0 / f64::from(self.denominator)
    }

    fn pack(self) -> u32 {
        (self.numerator << 16) | self.denominator
    }

    fn unpack(v: u32) -> Self {
        Self { numerator: v >> 16, denominator: v & 0xFFFF }
    }
}

/// The loop, in beats: playback that reaches `end` carries on from `start`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoopRange {
    pub start: f64,
    pub end: f64,
}

impl Default for LoopRange {
    fn default() -> Self {
        Self { start: 0.0, end: 16.0 }
    }
}

/// Where one piece of an audio block sits in time. Devices get one with
/// every call; plugins get the same facts in their own process context.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessContext {
    pub sample_rate: u32,
    /// Frames in this piece.
    pub frames: usize,
    pub playing: bool,
    /// Playback started on this piece's first frame.
    pub started: bool,
    /// The position jumped on this piece's first frame: the loop wrapped or a seek landed.
    pub jumped: bool,
    /// BPM.
    pub tempo: f64,
    pub beats_per_frame: f64,
    /// The position at the first frame, in beats.
    pub beat: f64,
    /// The position at the first frame, in frames. After a jump it is the
    /// beat at the current tempo; from there it counts on.
    pub frame: u64,
    /// Frames since the output opened. Never jumps, and runs while stopped.
    pub continuous_frame: u64,
    pub time_signature: TimeSignature,
    /// The bar the first frame is in, from 0, and the beat that bar starts on.
    pub bar: i64,
    pub bar_start: f64,
    pub looping: bool,
    pub loop_range: LoopRange,
}

impl ProcessContext {
    /// A stopped clock at 120 BPM, for running a device on its own.
    pub fn stopped(sample_rate: u32, frames: usize) -> Self {
        let tempo = 120.0;
        let sig = TimeSignature::default();
        Self {
            sample_rate,
            frames,
            playing: false,
            started: false,
            jumped: false,
            tempo,
            beats_per_frame: beats_per_frame(tempo, sample_rate),
            beat: 0.0,
            frame: 0,
            continuous_frame: 0,
            time_signature: sig,
            bar: 0,
            bar_start: 0.0,
            looping: false,
            loop_range: LoopRange::default(),
        }
    }

    pub fn frames_per_beat(&self) -> f64 {
        1.0 / self.beats_per_frame
    }

    /// The beat at frame `n` of this piece.
    pub fn beat_at(&self, n: usize) -> f64 {
        self.beat + n as f64 * self.beats_per_frame
    }

    /// The first frame of this piece at or after `beat`, if it falls inside.
    pub fn frame_of(&self, beat: f64) -> Option<usize> {
        if !self.playing {
            return None;
        }
        let n = ((beat - self.beat) / self.beats_per_frame - EDGE_FRAMES).ceil().max(0.0);
        (n < self.frames as f64).then_some(n as usize)
    }
}

fn beats_per_frame(tempo: f64, rate: u32) -> f64 {
    tempo / 60.0 / f64::from(rate.max(1))
}

fn store_f64(a: &AtomicU64, v: f64) {
    a.store(v.to_bits(), Ordering::Relaxed);
}

fn load_f64(a: &AtomicU64) -> f64 {
    f64::from_bits(a.load(Ordering::Relaxed))
}

/// The transport's settings and position, shared by the control side and
/// the audio thread. Atomics only: every method is safe from any thread and
/// never blocks the audio thread.
#[derive(Debug)]
pub struct TransportShared {
    tempo: AtomicU64,
    signature: AtomicU32,
    playing: AtomicBool,
    /// Bumped by each play from stopped, so a stop and a play between two
    /// blocks still restart.
    starts: AtomicU32,
    /// Where play starts, in beats.
    start: AtomicU64,
    /// A beat to jump to, or NO_SEEK.
    seek: AtomicU64,
    looping: AtomicBool,
    /// Odd while the loop is being written (a seqlock: the audio thread keeps
    /// the last whole range rather than wait).
    loop_seq: AtomicU32,
    loop_start: AtomicU64,
    loop_end: AtomicU64,
    /// Written by the audio thread: the position at the end of its last block.
    beat: AtomicU64,
    frame: AtomicU64,
}

impl Default for TransportShared {
    fn default() -> Self {
        let lr = LoopRange::default();
        Self {
            tempo: AtomicU64::new(120.0f64.to_bits()),
            signature: AtomicU32::new(TimeSignature::default().pack()),
            playing: AtomicBool::new(false),
            starts: AtomicU32::new(0),
            start: AtomicU64::new(0.0f64.to_bits()),
            seek: AtomicU64::new(NO_SEEK),
            looping: AtomicBool::new(false),
            loop_seq: AtomicU32::new(0),
            loop_start: AtomicU64::new(lr.start.to_bits()),
            loop_end: AtomicU64::new(lr.end.to_bits()),
            beat: AtomicU64::new(0.0f64.to_bits()),
            frame: AtomicU64::new(0),
        }
    }
}

impl TransportShared {
    /// BPM, clamped to 20..999. A NaN is ignored. Takes effect at the next
    /// block, from the beat the clock has reached.
    pub fn set_tempo(&self, bpm: f64) {
        if !bpm.is_nan() {
            store_f64(&self.tempo, bpm.clamp(MIN_TEMPO, MAX_TEMPO));
        }
    }

    pub fn tempo(&self) -> f64 {
        load_f64(&self.tempo)
    }

    /// Sets the time signature. The numerator is 1..=32 and the denominator
    /// one of 1, 2, 4, 8, 16, 32; anything else is ignored.
    pub fn set_time_signature(&self, numerator: u32, denominator: u32) {
        if (1..=32).contains(&numerator) && [1, 2, 4, 8, 16, 32].contains(&denominator) {
            self.signature.store(TimeSignature { numerator, denominator }.pack(), Ordering::Relaxed);
        }
    }

    pub fn time_signature(&self) -> TimeSignature {
        TimeSignature::unpack(self.signature.load(Ordering::Relaxed))
    }

    /// Plays from the start position, or stops. Playing while already
    /// playing changes nothing; a stop then a play restarts, even before the
    /// audio thread has seen the stop.
    pub fn set_playing(&self, on: bool) {
        if on {
            if !self.playing.load(Ordering::Acquire) {
                self.starts.fetch_add(1, Ordering::AcqRel);
                self.playing.store(true, Ordering::Release);
            }
        } else {
            self.playing.store(false, Ordering::Release);
        }
    }

    pub fn play(&self) {
        self.set_playing(true);
    }

    pub fn stop(&self) {
        self.set_playing(false);
    }

    /// Whether playback is on, as last asked: the audio thread follows at its next block.
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    /// Moves the position to `beat` (clamped at 0) and makes it where play
    /// starts. While playing it jumps there at the next block.
    pub fn seek(&self, beat: f64) {
        let beat = if beat.is_finite() { beat.max(0.0) } else { 0.0 };
        store_f64(&self.start, beat);
        self.seek.store(beat.to_bits(), Ordering::Release);
    }

    /// Where play starts, in beats.
    pub fn start(&self) -> f64 {
        load_f64(&self.start)
    }

    /// Sets the loop. `end` must be after `start` and `start` at or after 0;
    /// anything else is ignored. Calls come from one control thread at a time.
    pub fn set_loop(&self, start: f64, end: f64) {
        if !(start.is_finite() && end.is_finite() && start >= 0.0 && end > start) {
            return;
        }
        let v = self.loop_seq.load(Ordering::Relaxed) | 1;
        self.loop_seq.store(v, Ordering::Release);
        std::sync::atomic::fence(Ordering::Release);
        store_f64(&self.loop_start, start);
        store_f64(&self.loop_end, end);
        self.loop_seq.store(v.wrapping_add(1), Ordering::Release);
    }

    pub fn loop_range(&self) -> LoopRange {
        LoopRange { start: load_f64(&self.loop_start), end: load_f64(&self.loop_end) }
    }

    /// With looping on, playback that reaches the loop's end carries on from its start, on the frame.
    pub fn set_looping(&self, on: bool) {
        self.looping.store(on, Ordering::Relaxed);
    }

    pub fn looping(&self) -> bool {
        self.looping.load(Ordering::Relaxed)
    }

    /// The position the audio thread reached at the end of its last block, in beats.
    pub fn position_beats(&self) -> f64 {
        load_f64(&self.beat)
    }

    /// The same position in frames.
    pub fn position_frames(&self) -> u64 {
        self.frame.load(Ordering::Relaxed)
    }

    /// Reads the loop without waiting: `None` while a write is under way.
    fn read_loop(&self) -> Option<(u32, LoopRange)> {
        let v1 = self.loop_seq.load(Ordering::Acquire);
        if v1 & 1 == 1 {
            return None;
        }
        let range = self.loop_range();
        std::sync::atomic::fence(Ordering::Acquire);
        let v2 = self.loop_seq.load(Ordering::Acquire);
        (v1 == v2).then_some((v1, range))
    }

    fn take_seek(&self) -> Option<f64> {
        match self.seek.swap(NO_SEEK, Ordering::AcqRel) {
            NO_SEEK => None,
            bits => Some(f64::from_bits(bits)),
        }
    }
}

/// The audio thread's half of the clock. Plain data: copy it to run it.
#[derive(Debug, Clone, Copy)]
pub struct Transport {
    rate: u32,
    playing: bool,
    starts: u32,
    /// The position is `anchor_beat + (frames since the anchor) * bpf`, worked
    /// out by multiplying rather than adding block after block, so it does
    /// not drift. Re-anchored at every tempo change and jump.
    anchor_beat: f64,
    anchor_frame: u64,
    since: u64,
    bpf: f64,
    continuous: u64,
    range: LoopRange,
}

impl Transport {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            rate: sample_rate.max(1),
            playing: false,
            starts: 0,
            anchor_beat: 0.0,
            anchor_frame: 0,
            since: 0,
            bpf: beats_per_frame(120.0, sample_rate),
            continuous: 0,
            range: LoopRange::default(),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn beat(&self) -> f64 {
        self.anchor_beat + self.since as f64 * self.bpf
    }

    fn frame(&self) -> u64 {
        self.anchor_frame + self.since
    }

    fn jump_to(&mut self, beat: f64) {
        self.anchor_beat = beat;
        self.anchor_frame = (beat / self.bpf).round() as u64;
        self.since = 0;
    }

    /// Runs one block of `frames` frames: reads the shared settings, moves the
    /// position, and calls `f(context, offset)` for each piece of the block in
    /// order (`offset` is the piece's first frame in the block). The block
    /// splits only where the loop wraps. Real-time safe.
    pub fn run(&mut self, shared: &TransportShared, frames: usize, mut f: impl FnMut(&ProcessContext, usize)) {
        let tempo = shared.tempo();
        let bpf = beats_per_frame(tempo, self.rate);
        if bpf != self.bpf {
            // A new tempo: carry on from the beat reached, at the new rate.
            let beat = self.beat();
            let frame = self.frame();
            self.bpf = bpf;
            self.anchor_beat = beat;
            self.anchor_frame = frame;
            self.since = 0;
        }
        if let Some((_, range)) = shared.read_loop() {
            self.range = range;
        }
        let sig = shared.time_signature();
        // A loop shorter than a frame could never be played: treat it as off.
        let looping = shared.looping() && (self.range.end - self.range.start) / bpf >= 1.0;

        let want = shared.is_playing();
        let starts = shared.starts.load(Ordering::Acquire);
        let seek = shared.take_seek();
        let mut started = false;
        let mut jumped = false;
        if want && (!self.playing || starts != self.starts) {
            self.playing = true;
            started = true;
            self.jump_to(seek.unwrap_or_else(|| shared.start()));
        } else if let Some(beat) = seek {
            self.jump_to(beat);
            jumped = self.playing;
        }
        if !want {
            self.playing = false;
        }
        self.starts = starts;

        let context = |t: &Self, frames: usize, started: bool, jumped: bool| {
            let beat = t.beat();
            let bar_beats = sig.bar_beats();
            let bar = (beat / bar_beats).floor();
            ProcessContext {
                sample_rate: t.rate,
                frames,
                playing: t.playing,
                started,
                jumped,
                tempo,
                beats_per_frame: t.bpf,
                beat,
                frame: t.frame(),
                continuous_frame: t.continuous,
                time_signature: sig,
                bar: bar as i64,
                bar_start: bar * bar_beats,
                looping,
                loop_range: t.range,
            }
        };

        if !self.playing {
            f(&context(self, frames, false, false), 0);
            self.continuous += frames as u64;
        } else {
            let mut offset = 0;
            let mut wrapped = false;
            while offset < frames {
                let mut len = frames - offset;
                let mut wrap = false;
                let beat = self.beat();
                if looping && beat < self.range.end {
                    // The first frame at or after the loop's end starts the loop again.
                    let floor = if wrapped { 1.0 } else { 0.0 };
                    let to_end = ((self.range.end - beat) / bpf - EDGE_FRAMES).ceil().max(floor);
                    if to_end < len as f64 {
                        len = to_end as usize;
                        wrap = true;
                    }
                }
                if len > 0 {
                    f(&context(self, len, started, jumped), offset);
                    started = false;
                    jumped = false;
                    self.since += len as u64;
                    self.continuous += len as u64;
                    offset += len;
                }
                wrapped = false;
                if wrap {
                    // Keep the part of a frame that went past the end, so the loop stays in phase.
                    let over = (self.beat() - self.range.end).max(0.0);
                    self.jump_to(self.range.start + over);
                    jumped = true;
                    wrapped = true;
                }
            }
        }
        shared.beat.store(self.beat().to_bits(), Ordering::Relaxed);
        shared.frame.store(self.frame(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// Runs `frames` frames in blocks of `block` and returns every piece.
    fn pieces(t: &mut Transport, s: &TransportShared, frames: usize, block: usize) -> Vec<(usize, ProcessContext)> {
        let mut out = Vec::new();
        let mut done = 0;
        while done < frames {
            let n = block.min(frames - done);
            t.run(s, n, |c, off| out.push((done + off, *c)));
            done += n;
        }
        out
    }

    #[test]
    fn stopped_the_clock_holds_still_and_counts_continuous_frames() {
        let s = TransportShared::default();
        let mut t = Transport::new(RATE);
        let p = pieces(&mut t, &s, 1_000, 256);
        assert_eq!(p.len(), 4, "one piece per block when nothing wraps");
        assert!(p.iter().all(|(_, c)| !c.playing && c.beat == 0.0));
        assert_eq!(p[3].1.continuous_frame, 768);
    }

    #[test]
    fn play_starts_on_the_first_frame_and_beats_follow_the_tempo() {
        let s = TransportShared::default();
        let mut t = Transport::new(RATE);
        s.play();
        let p = pieces(&mut t, &s, 48_000, 512);
        assert!(p[0].1.started && p[0].1.playing && p[0].1.beat == 0.0);
        assert!(!p[1].1.started);
        // 120 BPM: two beats a second.
        assert!((s.position_beats() - 2.0).abs() < 1e-9, "{}", s.position_beats());
        assert_eq!(s.position_frames(), 48_000);
        let (at, c) = p[10];
        assert!((c.beat - at as f64 / 24_000.0).abs() < 1e-12);
    }

    #[test]
    fn a_tempo_change_carries_on_from_the_beat_reached() {
        let s = TransportShared::default();
        let mut t = Transport::new(RATE);
        s.play();
        pieces(&mut t, &s, 24_000, 1_000); // one beat at 120
        s.set_tempo(60.0);
        let p = pieces(&mut t, &s, 48_000, 1_000); // one more beat at 60
        assert!((p[0].1.beat - 1.0).abs() < 1e-12, "no jump: {}", p[0].1.beat);
        assert_eq!(p[0].1.tempo, 60.0);
        assert!((s.position_beats() - 2.0).abs() < 1e-9, "{}", s.position_beats());
        assert_eq!(s.position_frames(), 72_000);
    }

    #[test]
    fn the_tempo_is_clamped_and_nan_ignored() {
        let s = TransportShared::default();
        s.set_tempo(5.0);
        assert_eq!(s.tempo(), MIN_TEMPO);
        s.set_tempo(5_000.0);
        assert_eq!(s.tempo(), MAX_TEMPO);
        s.set_tempo(f64::NAN);
        assert_eq!(s.tempo(), MAX_TEMPO);
    }

    #[test]
    fn the_loop_wraps_on_its_frame_and_splits_the_block() {
        let s = TransportShared::default();
        s.set_loop(0.0, 1.0); // one beat: 24,000 frames at 120
        s.set_looping(true);
        s.play();
        let mut t = Transport::new(RATE);
        // 512-frame blocks: 24,000 is 46 blocks and 448 frames.
        let p = pieces(&mut t, &s, 72_000, 512);
        let jumps: Vec<usize> = p.iter().filter(|(_, c)| c.jumped).map(|(at, _)| *at).collect();
        assert_eq!(jumps, vec![24_000, 48_000], "the loop starts again exactly on its end frame");
        for (at, c) in &p {
            assert!(c.beat >= 0.0 && c.beat + (c.frames as f64 - 1.0) * c.beats_per_frame < 1.0, "piece at {at} stays inside the loop");
        }
        let wrap = p.iter().position(|(at, _)| *at == 24_000).unwrap();
        assert_eq!(p[wrap - 1].1.frames, 448, "the block before the wrap is cut at it");
        assert!(p[wrap].1.beat.abs() < 1e-12);
    }

    #[test]
    fn a_loop_shorter_than_a_block_wraps_several_times_in_one() {
        let s = TransportShared::default();
        s.set_loop(2.0, 2.0625); // a 64th note: 1,500 frames
        s.set_looping(true);
        s.seek(2.0);
        s.play();
        let mut t = Transport::new(RATE);
        let p = pieces(&mut t, &s, 4_096, 4_096);
        let lens: Vec<usize> = p.iter().map(|(_, c)| c.frames).collect();
        assert_eq!(lens, vec![1_500, 1_500, 1_096]);
        assert!(p[1].1.jumped && (p[1].1.beat - 2.0).abs() < 1e-12);
    }

    #[test]
    fn a_position_past_the_loop_end_plays_on() {
        let s = TransportShared::default();
        s.set_loop(0.0, 1.0);
        s.set_looping(true);
        s.seek(3.0);
        s.play();
        let mut t = Transport::new(RATE);
        let p = pieces(&mut t, &s, 48_000, 4_096);
        assert!(p.iter().all(|(_, c)| !c.jumped), "the loop only catches playback that reaches its end from inside");
        assert!((s.position_beats() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn a_seek_while_playing_jumps_and_stop_then_play_restarts() {
        let s = TransportShared::default();
        let mut t = Transport::new(RATE);
        s.play();
        pieces(&mut t, &s, 4_800, 480);
        s.seek(8.0);
        let p = pieces(&mut t, &s, 480, 480);
        assert!(p[0].1.jumped && p[0].1.beat == 8.0 && p[0].1.frame == 192_000);
        assert_eq!(p[0].1.bar, 2, "beat 8 is the third bar of 4/4");

        // Stop and play again between two blocks: the clock restarts from the start position.
        s.stop();
        s.play();
        let p = pieces(&mut t, &s, 480, 480);
        assert!(p[0].1.started && p[0].1.beat == 8.0);
        // Playing again while playing changes nothing.
        s.play();
        let p = pieces(&mut t, &s, 480, 480);
        assert!(!p[0].1.started);
    }

    #[test]
    fn bars_follow_the_time_signature() {
        let s = TransportShared::default();
        s.set_time_signature(6, 8);
        s.set_time_signature(7, 3); // not a note value: ignored
        assert_eq!(s.time_signature(), TimeSignature { numerator: 6, denominator: 8 });
        s.seek(7.0);
        s.play();
        let mut t = Transport::new(RATE);
        let p = pieces(&mut t, &s, 10, 10);
        assert_eq!(p[0].1.bar, 2, "6/8 bars are three beats long");
        assert_eq!(p[0].1.bar_start, 6.0);
    }

    #[test]
    fn a_bad_loop_is_ignored() {
        let s = TransportShared::default();
        s.set_loop(4.0, 2.0);
        s.set_loop(-1.0, 2.0);
        s.set_loop(0.0, f64::INFINITY);
        assert_eq!(s.loop_range(), LoopRange::default());
    }

    #[test]
    fn frame_of_finds_a_beat_inside_a_piece() {
        let mut c = ProcessContext::stopped(RATE, 512);
        c.playing = true;
        c.beat = 1.0;
        assert_eq!(c.frame_of(1.0), Some(0));
        assert_eq!(c.frame_of(1.0 + 100.0 * c.beats_per_frame), Some(100));
        assert_eq!(c.frame_of(1.0 + 100.5 * c.beats_per_frame), Some(101));
        assert_eq!(c.frame_of(2.0), None);
    }
}
