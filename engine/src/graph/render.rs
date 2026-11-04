//! Offline renders of the graph, for project export (idevkore/dk-tunekore#44).
//!
//! A render runs the same graph the output plays, on a clock of its own and
//! as fast as its devices go, one block per [`Render::next`] on the caller's
//! thread. Where it gets the graph:
//!
//! - **An output is open.** The output callback lends the running graph
//!   ([`Loan`]): at the top of its next callback it hands the graph over and
//!   plays on without it (the test tone and the Analyzer's player still
//!   sound), and it takes the graph back at the first callback after the
//!   render is dropped. The devices stay prepared at the output's rate,
//!   plugins stay activated as they are (a plugin can't be copied, and its
//!   controller is the main thread's), and nothing waits for the main thread.
//!   Edits made meanwhile reach the render's graph as they would the output's.
//! - **No output is open.** The mixer starts a graph at the rate the devices
//!   last ran at, and `Engine::finish_render` takes the devices back after.
//!
//! Closing or reopening the output while a render holds the graph cancels the
//! render and waits for the graph to come back first (`Engine::close_output`),
//! so no device is lost and no plugin runs after the output has let go of it.
//!
//! A render starts by letting the graph settle on a stopped clock: whatever
//! still sounds from live play (a release, a reverb's tail) dies away there
//! and is thrown away. Then its own transport plays from the range's start
//! beat. At the range's end the transport stops, as a stop does live (the
//! Sampler's pads let go, clips fade out, plugins get their note-offs), and
//! the tail runs on until everything has been below [`TAIL_FLOOR`] for
//! [`TAIL_QUIET`] seconds, or for the longest tail asked for. The render ends
//! at the last frame above the floor, never before the range's end.
//!
//! The master goes through the same code as the output callback's
//! (`mix_into`, then the master level and the limiter), so for the same blocks
//! it is the same, sample for sample. The limiter's lookahead delays it, and
//! delay compensation (#43) will delay the graph's output as a whole
//! ([`Graph::latency`]); the render trims both, so every file starts on the
//! range's first frame and the stems line up with the master.
//!
//! Per block nothing is allocated, freed, locked, logged or written: the
//! buffers are made in [`Render::new`]. Writing the files is the caller's.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use super::{Graph, MAX_FRAMES};
use crate::limiter::Limiter;
use crate::snapshot::Mailbox;
use crate::track::TrackId;
use crate::transport::{TempoMap, Transport, TransportShared, EDGE_FRAMES};

/// Quieter than this (-100 dBFS) is silence, for the tail and the settle.
pub const TAIL_FLOOR: f32 = 1.0e-5;
/// The tail ends once everything has been quiet this long, in seconds: long
/// enough for a slow delay's next echo.
pub const TAIL_QUIET: f64 = 2.0;
/// A tail cut at its longest fades out over its last this many seconds.
pub const TAIL_FADE: f64 = 0.01;
/// The settle ends once the graph has been quiet this long, in seconds...
const SETTLE_QUIET: f64 = 1.0;
/// ...or after this long, whatever still sounds.
const SETTLE_MAX: f64 = 10.0;

// What the graph's loan is doing.
const IDLE: u8 = 0;
/// A render asked for the graph.
const WANTED: u8 = 1;
/// The callback is handing it over.
const LENDING: u8 = 2;
/// It is in `lent`, for the render to take.
const LENT: u8 = 3;
/// A render holds it (or a graph the mixer started for one).
const OUT: u8 = 4;

/// How the output callback lends its graph to a render and takes it back.
/// Lock-free: the callback only swaps pointers and moves the state on.
pub(crate) struct Loan {
    state: AtomicU8,
    /// Set to make the render stop at its next block.
    cancel: AtomicBool,
    lent: Mailbox<Graph>,
    back: Mailbox<Graph>,
}

impl Loan {
    pub(crate) fn new() -> Self {
        Self { state: AtomicU8::new(IDLE), cancel: AtomicBool::new(false), lent: Mailbox::new(), back: Mailbox::new() }
    }

    /// Audio thread, at the top of each callback: takes the graph back from a
    /// render that is done with it, and hands it over when one asks.
    /// Allocates and frees nothing.
    pub(crate) fn at_callback(&self, graph: &mut Option<Box<Graph>>) {
        if graph.is_none() {
            if let Some(g) = self.back.take() {
                *graph = Some(g);
            }
        }
        if graph.is_some() && self.state.compare_exchange(WANTED, LENDING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            if let Some(g) = graph.take() {
                if let Some(old) = self.lent.put(g) {
                    // Can't happen (there is one graph), and leaking beats freeing here.
                    std::mem::forget(old);
                }
            }
            self.state.store(LENT, Ordering::Release);
        }
    }

    /// Asks the callback for the graph. False when a render already has it.
    pub(crate) fn ask(&self) -> bool {
        self.cancel.store(false, Ordering::Relaxed);
        self.state.compare_exchange(IDLE, WANTED, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// The graph, once the callback has handed it over.
    pub(crate) fn take(&self) -> Option<Box<Graph>> {
        if self.state.load(Ordering::Acquire) != LENT {
            return None;
        }
        let g = self.lent.take()?;
        self.state.store(OUT, Ordering::Release);
        Some(g)
    }

    /// Stops asking. False when the callback is already handing the graph
    /// over: then [`take`](Self::take) gets it.
    pub(crate) fn give_up(&self) -> bool {
        self.state.compare_exchange(WANTED, IDLE, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// Marks a graph the mixer started for a render as out.
    pub(crate) fn hold(&self) -> bool {
        self.cancel.store(false, Ordering::Relaxed);
        self.state.compare_exchange(IDLE, OUT, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// Whether a render holds the graph.
    pub(crate) fn is_out(&self) -> bool {
        self.state.load(Ordering::Acquire) == OUT
    }

    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// A graph a render gave back that the callback never took (the output
    /// closed first): the caller drops it, which sends its devices home.
    pub(crate) fn unclaimed(&self) -> Option<Box<Graph>> {
        self.back.take().or_else(|| self.lent.take())
    }
}

/// Where a render's graph goes when it is done.
pub(crate) enum Home {
    /// Back to the output callback that lent it.
    Output,
    /// Dropped, which sends its devices home to the mixer.
    Mixer,
}

/// What to render.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderSpec {
    /// Where the render starts and ends, in beats; `end` is after `start`.
    /// The range is the frames from the first at or after `start` to the
    /// last before `end`, as the transport counts them.
    pub start: f64,
    pub end: f64,
    /// The longest tail after `end`, in seconds; 0 cuts the render at `end`.
    pub tail_seconds: f64,
    /// The tracks to render on their own, in order: each after its volume,
    /// pan, mute and solo, before the master level and the limiter.
    pub stems: Vec<TrackId>,
    /// Frames per block, 1 to [`MAX_FRAMES`].
    pub block: usize,
}

impl Default for RenderSpec {
    fn default() -> Self {
        Self { start: 0.0, end: 16.0, tail_seconds: 0.0, stems: Vec::new(), block: MAX_FRAMES }
    }
}

/// Frames in the range `start..end` (beats), as the transport counts them:
/// the first frame at or after a boundary belongs to what comes after it.
pub fn range_frames(map: &TempoMap, start: f64, end: f64) -> u64 {
    (map.frames_between(start, end) - EDGE_FRAMES).ceil().max(0.0) as u64
}

/// Where a render is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Letting what still sounded from live play die away, unrecorded.
    Settling,
    /// Playing the range.
    Playing,
    /// After the range's end: the tail, and the limiter's lookahead.
    Tail,
    Done,
}

/// How far a render has got, in frames of the files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub phase: Phase,
    /// Frames every file has had.
    pub frames: u64,
    /// Frames in the range.
    pub range: u64,
    /// The longest the files can get: the range and the longest tail.
    pub most: u64,
}

/// One block of one file: its frames from `first` (0 is the range's first
/// frame), left and right. Empty while settling.
#[derive(Debug, Clone, Copy)]
pub struct Part<'a> {
    pub first: u64,
    pub left: &'a [f32],
    pub right: &'a [f32],
}

/// Which frames of a block go in a file.
#[derive(Debug, Clone, Copy, Default)]
struct Window {
    first: u64,
    from: usize,
    to: usize,
}

/// The frames `from..to` of a block of `n` graph frames starting at graph
/// frame `g0` that land in a file `latency` frames behind, of at most `cap`.
fn window(g0: u64, n: usize, latency: u64, cap: u64) -> Window {
    let at = g0 as i64 - latency as i64; // the file frame of the block's first
    let from = (-at).clamp(0, n as i64) as usize;
    let to = (cap as i64 - at).clamp(from as i64, n as i64) as usize;
    Window { first: (at + from as i64) as u64, from, to }
}

fn loud(l: &[f32], r: &[f32]) -> bool {
    l.iter().chain(r).any(|x| x.abs() > TAIL_FLOOR)
}

/// The last index of `l`/`r` at or after `from` above the floor.
fn last_loud(l: &[f32], r: &[f32], from: usize) -> Option<usize> {
    (from..l.len().min(r.len())).rev().find(|&i| l[i].abs() > TAIL_FLOOR || r[i].abs() > TAIL_FLOOR)
}

/// An offline render of the graph. Made by `Engine::start_render`; run it
/// with [`next`](Self::next) and write [`master`](Self::master) and each
/// [`stem`](Self::stem) after every block. Dropping it gives the graph back.
pub struct Render {
    graph: Option<Box<Graph>>,
    home: Home,
    loan: Arc<Loan>,
    rate: u32,
    block: usize,
    phase: Phase,
    /// The settle's own stopped clock.
    settle: Transport,
    settle_clock: TransportShared,
    settle_quiet: usize,
    settled: usize,
    settle_need: usize,
    settle_most: usize,
    /// The render's own clock, which plays the range.
    transport: Transport,
    clock: TransportShared,
    limiter: Limiter,
    gain: f32,
    /// Interleaved stereo, for the master's level and limiter.
    buf: Vec<f32>,
    /// The limited master for the last block, left and right.
    out: [Vec<f32>; 2],
    /// The graph slot of each stem.
    slots: Vec<usize>,
    /// Graph frames run since the range's start.
    ran: u64,
    /// Frames in the range, the longest tail, and both.
    body: u64,
    tail: u64,
    cap: u64,
    tail_quiet: u64,
    tail_fade: u64,
    /// How far behind the transport the master and the stems come out.
    master_latency: u64,
    stem_latency: u64,
    master_win: Window,
    stem_win: Window,
    /// The last file frame in the tail above the floor.
    loudest_end: Option<u64>,
    length: Option<u64>,
}

impl Render {
    /// A render of `graph` (which goes to `home` when done) for `spec`,
    /// already checked, on `map`'s tempo and rate, with the master at `gain`
    /// and stems from `slots`. Off the audio thread: it allocates.
    pub(crate) fn new(mut graph: Box<Graph>, home: Home, loan: Arc<Loan>, spec: &RenderSpec, map: TempoMap, gain: f32, slots: Vec<usize>) -> Self {
        let rate = map.sample_rate();
        let seconds = |s: f64| (s.max(0.0) * f64::from(rate)).round() as u64;
        let body = range_frames(&map, spec.start, spec.end);
        let tail = if spec.tail_seconds.is_finite() { seconds(spec.tail_seconds) } else { 0 };
        graph.capture(&slots);
        let clock = TransportShared::default();
        clock.set_tempo(map.tempo_at(spec.start));
        let sig = map.signature_at(spec.start);
        clock.set_time_signature(sig.numerator, sig.denominator);
        clock.seek(spec.start);
        clock.play();
        let settle_clock = TransportShared::default();
        settle_clock.set_tempo(map.tempo_at(spec.start));
        settle_clock.set_time_signature(sig.numerator, sig.denominator);
        let limiter = Limiter::new(rate, 2);
        let stem_latency = graph.latency() as u64;
        let master_latency = stem_latency + limiter.latency() as u64;
        Self {
            graph: Some(graph),
            home,
            loan,
            rate,
            block: spec.block.clamp(1, MAX_FRAMES),
            phase: Phase::Settling,
            settle: Transport::new(rate),
            settle_clock,
            settle_quiet: 0,
            settled: 0,
            settle_need: seconds(SETTLE_QUIET) as usize,
            settle_most: seconds(SETTLE_MAX) as usize,
            transport: Transport::new(rate),
            clock,
            limiter,
            gain,
            buf: vec![0.0; MAX_FRAMES * 2],
            out: [vec![0.0; MAX_FRAMES], vec![0.0; MAX_FRAMES]],
            slots,
            ran: 0,
            body,
            tail,
            cap: body + tail,
            tail_quiet: seconds(TAIL_QUIET),
            tail_fade: seconds(TAIL_FADE).max(1),
            master_latency,
            stem_latency,
            master_win: Window::default(),
            stem_win: Window::default(),
            loudest_end: None,
            length: None,
        }
    }

    /// The rate the files are at: the rate the devices run at.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Frames in the range.
    pub fn range(&self) -> u64 {
        self.body
    }

    /// How many stems the render makes, in the order asked for.
    pub fn stems(&self) -> usize {
        self.slots.len()
    }

    /// Whether the engine cancelled the render (its output is closing).
    pub fn cancelled(&self) -> bool {
        self.loan.cancel.load(Ordering::Relaxed)
    }

    /// How long every file is, once [`next`](Self::next) has said it's done:
    /// the range, and the tail up to its last frame above the floor.
    pub fn length(&self) -> Option<u64> {
        self.length
    }

    pub fn progress(&self) -> Progress {
        let frames = if self.phase == Phase::Settling { 0 } else { self.ran.saturating_sub(self.master_latency).min(self.cap) };
        Progress { phase: if self.length.is_some() { Phase::Done } else { self.phase }, frames, range: self.body, most: self.cap }
    }

    /// The master for the last block: the frames of it that go in the file.
    pub fn master(&self) -> Part<'_> {
        let w = self.master_win;
        Part { first: w.first, left: &self.out[0][w.from..w.to], right: &self.out[1][w.from..w.to] }
    }

    /// Stem `i` (in the order asked for) for the last block.
    pub fn stem(&self, i: usize) -> Part<'_> {
        let w = self.stem_win;
        let (l, r) = match (&self.graph, self.slots.get(i)) {
            (Some(g), Some(&slot)) => g.stem(slot),
            _ => (&[][..], &[][..]),
        };
        let to = w.to.min(l.len());
        let from = w.from.min(to);
        Part { first: w.first, left: &l[from..to], right: &r[from..to] }
    }

    /// Runs the next block. False once the render is done or cancelled: then
    /// there is nothing new to write. Real-time safe: no allocation, locks,
    /// logging or I/O.
    pub fn next(&mut self) -> bool {
        self.master_win = Window::default();
        self.stem_win = Window::default();
        if self.length.is_some() || self.cancelled() {
            return false;
        }
        let Some(graph) = self.graph.as_deref_mut() else { return false };
        if self.phase == Phase::Settling {
            graph.run(&mut self.settle, &self.settle_clock, self.block);
            let (l, r) = graph.master();
            let sound = loud(l, r) || self.slots.iter().any(|&s| {
                let (l, r) = graph.stem(s);
                loud(l, r)
            });
            self.settle_quiet = if sound { 0 } else { self.settle_quiet + self.block };
            self.settled += self.block;
            if self.settle_quiet >= self.settle_need || self.settled >= self.settle_most {
                self.phase = Phase::Playing;
            }
            return true;
        }

        let g0 = self.ran;
        if g0 == self.body {
            // The range's end: stop, as a stop does live, and let it ring.
            self.clock.stop();
            self.phase = Phase::Tail;
        }
        let mut n = (self.block as u64).min(self.cap + self.master_latency - g0);
        if g0 < self.body {
            n = n.min(self.body - g0);
        }
        let n = n as usize;
        graph.run(&mut self.transport, &self.clock, n);
        let buf = &mut self.buf[..n * 2];
        buf.fill(0.0);
        crate::mix_into(buf, 2, graph.master());
        self.limiter.process(buf, self.gain);
        for (i, f) in buf.chunks_exact(2).enumerate() {
            self.out[0][i] = f[0];
            self.out[1][i] = f[1];
        }
        self.ran += n as u64;
        self.master_win = window(g0, n, self.master_latency, self.cap);
        self.stem_win = window(g0, n, self.stem_latency, self.cap);

        if self.tail > 0 {
            self.watch_tail();
            self.fade_tail();
        }
        let delivered = self.ran.saturating_sub(self.master_latency);
        let done = if self.tail == 0 {
            delivered >= self.body
        } else {
            let heard = self.loudest_end.map_or(self.body, |f| (f + 1).max(self.body));
            delivered >= self.cap || (delivered >= self.body && delivered.saturating_sub(heard) >= self.tail_quiet)
        };
        if done {
            self.length = Some(if self.tail == 0 { self.body } else { self.loudest_end.map_or(self.body, |f| (f + 1).max(self.body)).min(self.cap) });
            self.phase = Phase::Done;
        }
        true
    }

    /// Notes the last frame in the tail where any file is above the floor.
    fn watch_tail(&mut self) {
        let body = self.body;
        let mut last = self.loudest_end;
        let mut note = |w: Window, l: &[f32], r: &[f32]| {
            // The part's frames at or after the range's end.
            let from = body.saturating_sub(w.first).min(l.len() as u64) as usize;
            if let Some(i) = last_loud(l, r, from) {
                let f = w.first + i as u64;
                last = Some(last.map_or(f, |x: u64| x.max(f)));
            }
        };
        let m = self.master();
        note(self.master_win, m.left, m.right);
        for i in 0..self.slots.len() {
            let s = self.stem(i);
            note(self.stem_win, s.left, s.right);
        }
        self.loudest_end = last;
    }

    /// Fades every file out over the last [`TAIL_FADE`] before the longest
    /// tail, so a tail that never goes quiet (a delay fed back for ever)
    /// doesn't end on a click. A tail that goes quiet first ends before it.
    fn fade_tail(&mut self) {
        let (cap, fade) = (self.cap, self.tail_fade);
        let start = cap.saturating_sub(fade).max(self.body);
        let gain = |f: u64| (cap.saturating_sub(f)) as f32 / fade as f32;
        let apply = |w: Window, l: &mut [f32], r: &mut [f32]| {
            for i in 0..l.len().min(r.len()) {
                let f = w.first + i as u64;
                if f >= start {
                    let g = gain(f);
                    l[i] *= g;
                    r[i] *= g;
                }
            }
        };
        let w = self.master_win;
        if w.first + (w.to - w.from) as u64 > start {
            let [l, r] = &mut self.out;
            apply(w, &mut l[w.from..w.to], &mut r[w.from..w.to]);
        }
        let w = self.stem_win;
        if w.first + (w.to - w.from) as u64 > start {
            if let Some(g) = self.graph.as_deref_mut() {
                for &slot in &self.slots {
                    let (l, r) = g.stem_mut(slot);
                    let to = w.to.min(l.len());
                    let from = w.from.min(to);
                    apply(w, &mut l[from..to], &mut r[from..to]);
                }
            }
        }
    }
}

impl Drop for Render {
    /// Gives the graph back: to the output callback that lent it, or to the
    /// mixer (dropping it sends its devices home).
    fn drop(&mut self) {
        let Some(mut graph) = self.graph.take() else { return };
        graph.capture(&[]);
        match self.home {
            Home::Output => drop(self.loan.back.put(graph)),
            Home::Mixer => drop(graph),
        }
        self.loan.state.store(IDLE, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
