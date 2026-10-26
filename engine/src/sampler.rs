//! The Sampler: 16 pads that play samples, a preview slot for the Library,
//! and a step sequencer that triggers the pads in time.
//!
//! Samples reach the audio thread at the device's rate through a lock-free
//! queue, and old ones go back through a second queue, so the audio thread
//! never frees memory. Everything else (pad settings, the pattern, tempo,
//! triggers from the UI) is an atomic in [`SamplerShared`]. The sequencer
//! counts frames on the audio thread, so steps land on the sample.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::Track;

/// Pads a kit has.
pub const PADS: usize = 16;
/// The Library's preview plays from this slot, after the pads.
pub const PREVIEW: usize = PADS;
/// Pads plus the preview slot.
pub const SLOTS: usize = PADS + 1;
/// The longest pattern, in steps (sixteenth notes).
pub const MAX_STEPS: usize = 64;
/// Voices that can sound at once. The oldest is stolen when all are busy.
const VOICES: usize = 32;
/// Fade at the start of a voice, so a slice that starts mid-wave doesn't click.
const ATTACK_SECONDS: f64 = 0.0005;
/// Fade when a voice is cut, released or reaches its end point.
const RELEASE_SECONDS: f64 = 0.004;
/// The shortest release a pad can ask for, so a gate ending never clicks.
const MIN_RELEASE_SECONDS: f64 = 0.001;
/// The decay stage ends, and holds at the sustain level, once it is this close.
const DECAY_SNAP: f32 = 1.0e-4;
/// Filter cutoff range in Hz. The top is also clamped below Nyquist.
const CUTOFF_MIN: f32 = 20.0;
const CUTOFF_MAX: f32 = 20_000.0;
/// Filter modes in `PadParams::filter`.
pub const FILTER_OFF: u8 = 0;
pub const FILTER_LOW_PASS: u8 = 1;
pub const FILTER_HIGH_PASS: u8 = 2;
pub const FILTER_BAND_PASS: u8 = 3;
const NO_STEP: u32 = u32::MAX;

const GATE: u32 = 1;
const REVERSE: u32 = 2;
const MUTE: u32 = 4;
/// The choke group lives in bits 8..12 of the flags word.
const CHOKE_SHIFT: u32 = 8;
const CHOKE_MASK: u32 = 0xF;
/// Choke groups a pad can join, 1..=8. 0 is none.
const CHOKE_GROUPS: u8 = 8;

/// A step's velocity is stored as 0..=255; this is full velocity.
const VEL_MAX: u8 = 255;

/// The metronome's click: a decaying sine on each beat.
const CLICK_SECONDS: f64 = 0.030;
const CLICK_FADE_IN_SECONDS: f64 = 0.001;
const CLICK_DECAY_SECONDS: f64 = 0.005;
const CLICK_DOWNBEAT_HZ: f32 = 1_600.0;
const CLICK_BEAT_HZ: f32 = 1_000.0;
/// Peak of about -12 dBFS once the fade-in and decay are counted.
const CLICK_GAIN: f32 = 0.3;
/// Steps (sixteenths) per beat.
const STEPS_PER_BEAT: usize = 4;

/// How one pad plays its sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PadParams {
    /// Linear gain, 0..4.
    pub gain: f32,
    /// -1 (left) to 1 (right).
    pub pan: f32,
    /// Semitones, -24..24. Changes speed and pitch together.
    pub pitch: f32,
    /// Where playback starts and ends, as fractions of the sample, 0..1.
    pub start: f32,
    pub end: f32,
    /// Play only while held (a pad) or for one step (the sequencer), instead of to the end.
    pub gate: bool,
    pub reverse: bool,
    /// The sequencer skips a muted pad. Playing it by hand still sounds.
    pub mute: bool,
    /// Choke group, 0 (none) or 1..=8. Starting a voice on a pad releases the
    /// voices of the other pads in its group (closed hi-hat cutting open).
    pub choke: u8,
    /// Amp envelope. `attack` is the time to full level, 0..2 s; 0 keeps the
    /// 0.5 ms anti-click ramp. `decay` is the time, 0..5 s, for the level to fall
    /// 60 dB of the way to `sustain` (0..1); 0 means no decay, and `sustain`
    /// then has no effect. `release` is the fade-out time, 0..5 s, after a
    /// gate or pad release (at least 1 ms). Cuts, chokes and retriggers always
    /// use the 4 ms fade.
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
    /// Filter mode: 0 off, 1 low-pass, 2 high-pass, 3 band-pass.
    pub filter: u8,
    /// Filter cutoff (or centre) in Hz, 20..20,000.
    pub cutoff: f32,
    /// Filter resonance, 0..1.
    pub resonance: f32,
}

impl Default for PadParams {
    fn default() -> Self {
        Self {
            gain: 1.0,
            pan: 0.0,
            pitch: 0.0,
            start: 0.0,
            end: 1.0,
            gate: false,
            reverse: false,
            mute: false,
            choke: 0,
            attack: 0.0,
            decay: 0.0,
            sustain: 1.0,
            release: RELEASE_SECONDS as f32,
            filter: FILTER_OFF,
            cutoff: CUTOFF_MAX,
            resonance: 0.0,
        }
    }
}

/// `v` clamped to `lo..=hi`, or `default` when it is not a number.
fn sane(v: f32, lo: f32, hi: f32, default: f32) -> f32 {
    if v.is_nan() { default } else { v.clamp(lo, hi) }
}

#[derive(Debug)]
struct PadCtl {
    gain: AtomicU32,  // f32 bits
    pan: AtomicU32,   // f32 bits
    pitch: AtomicU32, // f32 bits
    start: AtomicU32, // f32 bits
    end: AtomicU32,   // f32 bits
    flags: AtomicU32,
    pattern: AtomicU64,
    /// Bumped by the UI for each hit; the audio thread plays one per change.
    hits: AtomicU32,
    /// Whether the UI is still holding the pad (for gate mode).
    held: AtomicBool,
    /// Bumped to stop whatever the slot is playing.
    cuts: AtomicU32,
    /// Bumped by the audio thread each time the slot starts a voice, for the UI.
    played: AtomicU32,
    // Sound shaping, kept after the fields above.
    attack: AtomicU32,    // f32 bits
    decay: AtomicU32,     // f32 bits
    sustain: AtomicU32,   // f32 bits
    release: AtomicU32,   // f32 bits
    filter: AtomicU8,
    cutoff: AtomicU32,    // f32 bits
    resonance: AtomicU32, // f32 bits
    /// Velocity of the next hit, 0..1 as f32 bits. Written before `hits` is bumped.
    hit_vel: AtomicU32,
}

impl Default for PadCtl {
    fn default() -> Self {
        let p = PadParams::default();
        Self {
            gain: AtomicU32::new(p.gain.to_bits()),
            pan: AtomicU32::new(p.pan.to_bits()),
            pitch: AtomicU32::new(p.pitch.to_bits()),
            start: AtomicU32::new(p.start.to_bits()),
            end: AtomicU32::new(p.end.to_bits()),
            flags: AtomicU32::new(0),
            pattern: AtomicU64::new(0),
            hits: AtomicU32::new(0),
            held: AtomicBool::new(false),
            cuts: AtomicU32::new(0),
            played: AtomicU32::new(0),
            attack: AtomicU32::new(p.attack.to_bits()),
            decay: AtomicU32::new(p.decay.to_bits()),
            sustain: AtomicU32::new(p.sustain.to_bits()),
            release: AtomicU32::new(p.release.to_bits()),
            filter: AtomicU8::new(p.filter),
            cutoff: AtomicU32::new(p.cutoff.to_bits()),
            resonance: AtomicU32::new(p.resonance.to_bits()),
            hit_vel: AtomicU32::new(1.0f32.to_bits()),
        }
    }
}

impl PadCtl {
    fn params(&self) -> PadParams {
        let f = |a: &AtomicU32| f32::from_bits(a.load(Ordering::Relaxed));
        let flags = self.flags.load(Ordering::Relaxed);
        PadParams {
            gain: f(&self.gain),
            pan: f(&self.pan),
            pitch: f(&self.pitch),
            start: f(&self.start),
            end: f(&self.end),
            gate: flags & GATE != 0,
            reverse: flags & REVERSE != 0,
            mute: flags & MUTE != 0,
            choke: ((flags >> CHOKE_SHIFT) & CHOKE_MASK) as u8,
            attack: f(&self.attack),
            decay: f(&self.decay),
            sustain: f(&self.sustain),
            release: f(&self.release),
            filter: self.filter.load(Ordering::Relaxed),
            cutoff: f(&self.cutoff),
            resonance: f(&self.resonance),
        }
    }
}

/// State shared by the control side and the audio thread. Atomics only, so
/// every method is safe from any thread.
#[derive(Debug)]
pub struct SamplerShared {
    pads: [PadCtl; SLOTS],
    tempo: AtomicU32, // BPM, f32 bits
    swing: AtomicU32, // 0..1, f32 bits
    steps: AtomicU32,
    playing: AtomicBool,
    /// The step that played last, or NO_STEP when stopped. Written by the audio thread.
    step: AtomicU32,
    /// Velocity of each pad's steps, 0..=255 for 0..1. Starts at full.
    velocities: [[AtomicU8; MAX_STEPS]; PADS],
    metronome: AtomicBool,
    recording: AtomicBool,
    /// Bumped by the audio thread for each hit it writes into a pattern.
    recorded: AtomicU32,
}

impl Default for SamplerShared {
    fn default() -> Self {
        Self {
            pads: Default::default(),
            tempo: AtomicU32::new(120.0f32.to_bits()),
            swing: AtomicU32::new(0.0f32.to_bits()),
            steps: AtomicU32::new(16),
            playing: AtomicBool::new(false),
            step: AtomicU32::new(NO_STEP),
            velocities: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU8::new(VEL_MAX))),
            metronome: AtomicBool::new(false),
            recording: AtomicBool::new(false),
            recorded: AtomicU32::new(0),
        }
    }
}

impl SamplerShared {
    /// Sets how a pad (or the preview slot) plays. Ignores a bad index.
    pub fn set_pad(&self, slot: usize, p: &PadParams) {
        let Some(c) = self.pads.get(slot) else { return };
        let start = p.start.clamp(0.0, 1.0);
        let end = p.end.clamp(start, 1.0);
        c.gain.store(p.gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
        c.pan.store(p.pan.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
        c.pitch.store(p.pitch.clamp(-24.0, 24.0).to_bits(), Ordering::Relaxed);
        c.start.store(start.to_bits(), Ordering::Relaxed);
        c.end.store(end.to_bits(), Ordering::Relaxed);
        let flags = (p.gate as u32 * GATE) | (p.reverse as u32 * REVERSE) | (p.mute as u32 * MUTE)
            | (u32::from(p.choke.min(CHOKE_GROUPS)) << CHOKE_SHIFT);
        c.flags.store(flags, Ordering::Relaxed);
        let d = PadParams::default();
        c.attack.store(sane(p.attack, 0.0, 2.0, d.attack).to_bits(), Ordering::Relaxed);
        c.decay.store(sane(p.decay, 0.0, 5.0, d.decay).to_bits(), Ordering::Relaxed);
        c.sustain.store(sane(p.sustain, 0.0, 1.0, d.sustain).to_bits(), Ordering::Relaxed);
        c.release.store(sane(p.release, 0.0, 5.0, d.release).to_bits(), Ordering::Relaxed);
        c.filter.store(p.filter.min(FILTER_BAND_PASS), Ordering::Relaxed);
        c.cutoff.store(sane(p.cutoff, CUTOFF_MIN, CUTOFF_MAX, d.cutoff).to_bits(), Ordering::Relaxed);
        c.resonance.store(sane(p.resonance, 0.0, 1.0, d.resonance).to_bits(), Ordering::Relaxed);
    }

    pub fn pad(&self, slot: usize) -> PadParams {
        self.pads.get(slot).map(PadCtl::params).unwrap_or_default()
    }

    /// A pad's steps as bits: bit `n` set means the pad plays on step `n`.
    pub fn set_pattern(&self, pad: usize, bits: u64) {
        if pad < PADS {
            self.pads[pad].pattern.store(bits, Ordering::Relaxed);
        }
    }

    pub fn pattern(&self, pad: usize) -> u64 {
        if pad < PADS { self.pads[pad].pattern.load(Ordering::Relaxed) } else { 0 }
    }

    /// How hard a pad plays on a step, 0..1 (clamped). Ignores a bad index.
    pub fn set_velocity(&self, pad: usize, step: usize, v: f32) {
        if let Some(a) = self.velocities.get(pad).and_then(|p| p.get(step)) {
            let v = if v.is_nan() { 1.0 } else { v.clamp(0.0, 1.0) };
            a.store((v * VEL_MAX as f32).round() as u8, Ordering::Relaxed);
        }
    }

    /// A step's velocity, 0..1. Every step starts at 1, and a bad index reads 1.
    pub fn velocity(&self, pad: usize, step: usize) -> f32 {
        self.velocities
            .get(pad)
            .and_then(|p| p.get(step))
            .map_or(1.0, |a| a.load(Ordering::Relaxed) as f32 / VEL_MAX as f32)
    }

    /// Turns the metronome on or off. While the pattern plays it clicks on every beat.
    pub fn set_metronome(&self, on: bool) {
        self.metronome.store(on, Ordering::Relaxed);
    }

    pub fn metronome(&self) -> bool {
        self.metronome.load(Ordering::Relaxed)
    }

    /// While recording and playing, hits on pads are written into the pattern.
    pub fn set_recording(&self, on: bool) {
        self.recording.store(on, Ordering::Relaxed);
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }

    /// How many hits have been written into the pattern. Re-read the patterns when it changes.
    pub fn recorded(&self) -> u32 {
        self.recorded.load(Ordering::Acquire)
    }

    /// Plays a slot now, as if its pad were pressed. Pair with [`release`](Self::release).
    pub fn trigger(&self, slot: usize) {
        self.trigger_velocity(slot, 1.0);
    }

    /// Like [`trigger`](Self::trigger), but the hit plays at `v` (0..1, clamped)
    /// of full level. A hit recorded into the pattern keeps this velocity.
    pub fn trigger_velocity(&self, slot: usize, v: f32) {
        if let Some(c) = self.pads.get(slot) {
            c.held.store(true, Ordering::Relaxed);
            c.hit_vel.store(sane(v, 0.0, 1.0, 1.0).to_bits(), Ordering::Relaxed);
            c.hits.fetch_add(1, Ordering::Release);
        }
    }

    /// Lets go of a pad. Only gate-mode pads stop.
    pub fn release(&self, slot: usize) {
        if let Some(c) = self.pads.get(slot) {
            c.held.store(false, Ordering::Release);
        }
    }

    /// Stops whatever a slot is playing, with a short fade.
    pub fn cut(&self, slot: usize) {
        if let Some(c) = self.pads.get(slot) {
            c.cuts.fetch_add(1, Ordering::Release);
        }
    }

    /// How many times a slot has started playing. The UI flashes a pad when it changes.
    pub fn played(&self, slot: usize) -> u32 {
        self.pads.get(slot).map_or(0, |c| c.played.load(Ordering::Relaxed))
    }

    pub fn set_tempo(&self, bpm: f32) {
        self.tempo.store(bpm.clamp(20.0, 300.0).to_bits(), Ordering::Relaxed);
    }

    pub fn tempo(&self) -> f32 {
        f32::from_bits(self.tempo.load(Ordering::Relaxed))
    }

    /// 0 is straight; 1 delays every other sixteenth to a triplet feel.
    pub fn set_swing(&self, swing: f32) {
        self.swing.store(swing.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn swing(&self) -> f32 {
        f32::from_bits(self.swing.load(Ordering::Relaxed))
    }

    /// Pattern length in sixteenths, 1..=64.
    pub fn set_steps(&self, steps: usize) {
        self.steps.store(steps.clamp(1, MAX_STEPS) as u32, Ordering::Relaxed);
    }

    pub fn steps(&self) -> usize {
        self.steps.load(Ordering::Relaxed) as usize
    }

    /// Starts the pattern from step 1, or stops it.
    pub fn set_playing(&self, on: bool) {
        self.playing.store(on, Ordering::Release);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    /// The step playing now, or `None` when stopped.
    pub fn current_step(&self) -> Option<usize> {
        match self.step.load(Ordering::Relaxed) {
            NO_STEP => None,
            s => Some(s as usize),
        }
    }

    /// Length of step `step` in frames at `rate`, with swing.
    fn step_frames(&self, step: usize, rate: f64) -> f64 {
        let sixteenth = rate * 60.0 / self.tempo() as f64 / 4.0;
        let push = sixteenth * self.swing() as f64 / 3.0;
        if step.is_multiple_of(2) { sixteenth + push } else { sixteenth - push }
    }

    /// Frames that `loops` passes of the pattern last at `rate`.
    pub fn pattern_frames(&self, rate: u32, loops: usize) -> usize {
        let one: f64 = (0..self.steps()).map(|s| self.step_frames(s, rate as f64)).sum();
        (one * loops as f64).round() as usize
    }
}

/// A sample for one slot, at the device's rate (`None` empties the slot).
/// The same box comes back holding the slot's old sample.
#[derive(Debug, Default)]
pub struct SampleSlot {
    pub slot: usize,
    pub data: Option<Track>,
}

#[derive(Debug, Clone, Copy)]
enum Hold {
    /// Plays to its end point.
    Free,
    /// Until the pad is let go.
    Held,
    /// For this many more frames (a gate-mode step).
    Frames(f64),
}

/// Where a voice is in its amp envelope (before it is released).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    Attack,
    Decay,
    Sustain,
}

/// A topology-preserving-transform (zero-delay) state-variable filter for one
/// voice: two integrators per channel, and the coefficients.
///
/// Adapted from soundCraft (github.com/storytold/soundcraft,
/// crates/dsp/src/plugins/synth.rs), MIT OR Apache-2.0, Copyright (c) 2026
/// ArtCraft Team and the SoundCraft contributors. The coefficients, the
/// integrator update, the denormal flush and the resonance-to-`k` mapping
/// follow its `Svf`; ours adds high-pass and band-pass outputs, a stereo state
/// and per-block coefficients.
#[derive(Debug, Clone, Copy, Default)]
struct VoiceFilter {
    mode: u8,
    k: f32,
    a1: f32,
    a2: f32,
    a3: f32,
    ic1: [f32; 2],
    ic2: [f32; 2],
}

impl VoiceFilter {
    /// Sets the mode and coefficients: one `tan` per call. Plain math.
    fn tune(&mut self, mode: u8, cutoff: f32, resonance: f32, rate: f64) {
        self.mode = mode;
        if mode == FILTER_OFF {
            return;
        }
        let top = CUTOFF_MAX.min(rate as f32 * 0.45).max(CUTOFF_MIN);
        let fc = sane(cutoff, CUTOFF_MIN, top, CUTOFF_MAX);
        let g = (std::f32::consts::PI * fc / rate as f32).tan();
        // Resonance 0 is k = 2 (Q 0.5); 1 is k = 0.05 (Q 20).
        self.k = 2.0 - 1.95 * sane(resonance, 0.0, 1.0, 0.0);
        self.a1 = 1.0 / (1.0 + g * (g + self.k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    fn reset(&mut self) {
        self.ic1 = [0.0; 2];
        self.ic2 = [0.0; 2];
    }

    #[inline]
    fn flush(x: f32) -> f32 {
        if x.is_finite() && x.abs() > 1.0e-25 { x } else { 0.0 }
    }

    #[inline]
    fn process(&mut self, ch: usize, x: f32) -> f32 {
        let v3 = x - self.ic2[ch];
        let v1 = self.a1 * self.ic1[ch] + self.a2 * v3;
        let v2 = self.ic2[ch] + self.a2 * self.ic1[ch] + self.a3 * v3;
        self.ic1[ch] = Self::flush(2.0 * v1 - self.ic1[ch]);
        self.ic2[ch] = Self::flush(2.0 * v2 - self.ic2[ch]);
        let y = match self.mode {
            FILTER_LOW_PASS => v2,
            FILTER_HIGH_PASS => x - self.k * v1 - v2,
            _ => self.k * v1, // band-pass, unity gain at the centre
        };
        if y.is_finite() { y } else { 0.0 }
    }
}

#[derive(Debug, Clone, Copy)]
struct Voice {
    active: bool,
    slot: usize,
    pos: f64,
    inc: f64,
    lo: f64,
    hi: f64,
    fade: f64,
    left: f32,
    right: f32,
    env: f32,
    releasing: bool,
    hold: Hold,
    age: u64,
    // Amp envelope and filter, kept after the fields above.
    stage: Stage,
    /// Attack: level added per frame.
    atk_inc: f32,
    /// Decay: factor per frame on the way to `sustain`; 0 when the pad has no decay.
    dec_coef: f32,
    sustain: f32,
    /// Release: level taken off per frame when timed from full level.
    rel_full: f32,
    /// Release: level taken off per frame now that the voice is releasing.
    rel_step: f32,
    /// Released by a cut, choke or retrigger, so on the 4 ms fade.
    fast: bool,
    filter: VoiceFilter,
}

impl Voice {
    /// Starts the release. `fast` is the step of a cut (4 ms from full level,
    /// whatever the pad's release is); `None` is the pad's own release, timed
    /// from the level the voice is at, so it lasts `release` seconds.
    fn release(&mut self, fast: Option<f32>) {
        let step = fast.unwrap_or(self.env.max(0.0) * self.rel_full);
        self.rel_step = if self.releasing { self.rel_step.max(step) } else { step };
        self.releasing = true;
        self.fast |= fast.is_some();
    }
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            active: false,
            slot: 0,
            pos: 0.0,
            inc: 1.0,
            lo: 0.0,
            hi: 0.0,
            fade: 1.0,
            left: 0.0,
            right: 0.0,
            env: 0.0,
            releasing: false,
            hold: Hold::Free,
            age: 0,
            stage: Stage::Sustain,
            atk_inc: 1.0,
            dec_coef: 0.0,
            sustain: 1.0,
            rel_full: 1.0,
            rel_step: 0.0,
            fast: false,
            filter: VoiceFilter::default(),
        }
    }
}

/// The audio-thread half. Lives in the stream callback.
pub struct Sampler {
    samples: [Option<Track>; SLOTS],
    incoming: Option<rtrb::Consumer<Box<SampleSlot>>>,
    garbage: Option<rtrb::Producer<Box<SampleSlot>>>,
    /// A box waiting to go back because the return queue was full.
    held_back: Option<Box<SampleSlot>>,
    voices: [Voice; VOICES],
    hits: [u32; SLOTS],
    cuts: [u32; SLOTS],
    rate: f64,
    release: f32,
    edge: f64,
    clock: u64,
    was_playing: bool,
    next_step: usize,
    /// Frames left in the step that fired last. At a block boundary this says
    /// where the sequencer is, so a recorded hit can be quantized.
    countdown: f64,
    /// A hit recorded onto `skip_step` was already played by hand: the
    /// sequencer leaves these pads (bit per pad) out of that step, once.
    skip_step: usize,
    skip_pads: u32,
    /// The metronome click: frames left (0 = silent), phase in cycles, and so on.
    click_left: u32,
    click_phase: f32,
    click_inc: f32,
    click_env: f32,
    click_fade: f32,
    click_len: u32,
    click_decay: f32,
    click_fade_step: f32,
}

/// The control side's ends of the sampler's two queues.
pub struct SamplerLink {
    send: rtrb::Producer<Box<SampleSlot>>,
    returned: rtrb::Consumer<Box<SampleSlot>>,
}

impl SamplerLink {
    /// Frees samples the audio thread has finished with. Control thread only.
    pub fn collect(&mut self) {
        while self.returned.pop().is_ok() {}
    }

    /// Hands a slot's new sample (at the device's rate) to the audio thread.
    pub fn send(&mut self, slot: usize, data: Option<Track>) -> Result<(), String> {
        self.collect();
        self.send
            .push(Box::new(SampleSlot { slot, data }))
            .map_err(|_| "the audio thread is not taking new samples".to_string())
    }
}

/// A sampler for a stream at `sample_rate`, and the control side's link to it.
pub fn sampler(sample_rate: u32) -> (Sampler, SamplerLink) {
    let (send, incoming) = rtrb::RingBuffer::new(32);
    let (garbage, returned) = rtrb::RingBuffer::new(64);
    let mut s = Sampler::offline(sample_rate);
    s.incoming = Some(incoming);
    s.garbage = Some(garbage);
    (s, SamplerLink { send, returned })
}

impl Sampler {
    /// A sampler with no queues, for rendering to a file. Load it with
    /// [`set_sample`](Self::set_sample).
    pub fn offline(sample_rate: u32) -> Self {
        let rate = sample_rate.max(1) as f64;
        Self {
            samples: Default::default(),
            incoming: None,
            garbage: None,
            held_back: None,
            voices: [Voice::default(); VOICES],
            hits: [0; SLOTS],
            cuts: [0; SLOTS],
            rate,
            release: (1.0 / (rate * RELEASE_SECONDS)).min(1.0) as f32,
            edge: rate * RELEASE_SECONDS,
            clock: 0,
            was_playing: false,
            next_step: 0,
            countdown: 0.0,
            skip_step: 0,
            skip_pads: 0,
            click_left: 0,
            click_phase: 0.0,
            click_inc: 0.0,
            click_env: 0.0,
            click_fade: 0.0,
            click_len: (rate * CLICK_SECONDS) as u32,
            click_decay: (-1.0 / (rate * CLICK_DECAY_SECONDS)).exp() as f32,
            click_fade_step: (1.0 / (rate * CLICK_FADE_IN_SECONDS)).min(1.0) as f32,
        }
    }

    /// Puts a sample (at this sampler's rate) in a slot directly. Not for the audio thread.
    pub fn set_sample(&mut self, slot: usize, data: Option<Track>) {
        if slot < SLOTS {
            self.samples[slot] = data;
        }
    }

    /// Takes in new samples and the UI's hits and cuts, then adds the pads
    /// and the sequencer into `out` (interleaved, `channels` wide).
    /// Real-time safe: no allocation, locks, logging or I/O.
    pub fn render(&mut self, shared: &SamplerShared, out: &mut [f32], channels: usize) {
        self.take_samples();

        let playing = shared.playing.load(Ordering::Acquire);
        if playing && !self.was_playing {
            self.next_step = 0;
            self.countdown = 0.0;
        } else if !playing && self.was_playing {
            shared.step.store(NO_STEP, Ordering::Relaxed);
            self.click_left = 0;
            self.skip_pads = 0;
        }
        self.was_playing = playing;
        let steps = shared.steps().max(1);
        if self.next_step >= steps {
            self.next_step = 0;
        }
        let recording = playing && shared.recording.load(Ordering::Relaxed);

        for slot in 0..SLOTS {
            let c = &shared.pads[slot];
            let cuts = c.cuts.load(Ordering::Acquire);
            if cuts != self.cuts[slot] {
                self.cuts[slot] = cuts;
                self.release_slot(slot);
            }
            let hits = c.hits.load(Ordering::Acquire);
            if hits != self.hits[slot] {
                self.hits[slot] = hits;
                let vel = sane(f32::from_bits(c.hit_vel.load(Ordering::Relaxed)), 0.0, 1.0, 1.0);
                if recording && slot < PADS {
                    self.record_hit(shared, slot, steps, vel);
                }
                let hold = if c.held.load(Ordering::Acquire) { Hold::Held } else { Hold::Frames(self.rate * 0.05) };
                self.start(shared, slot, hold, vel);
            }
        }
        self.tune_filters(shared);

        let channels = channels.max(1);
        let frames = out.len() / channels;
        for frame in 0..frames {
            if playing {
                if self.countdown <= 0.0 {
                    self.fire_step(shared, steps);
                }
                self.countdown -= 1.0;
            }
            let (left, right) = self.mix_frame(shared);
            let click = self.click_frame();
            let o = &mut out[frame * channels..(frame + 1) * channels];
            if channels == 1 {
                o[0] += 0.5 * (left + right) + click;
            } else {
                o[0] += left + click;
                o[1] += right + click;
            }
        }
    }

    fn take_samples(&mut self) {
        if let Some(b) = self.held_back.take() {
            self.give_back(b);
        }
        loop {
            let Some(Ok(mut b)) = self.incoming.as_mut().map(rtrb::Consumer::pop) else { return };
            if b.slot >= SLOTS {
                self.give_back(b);
                continue;
            }
            // Voices on this slot point into the old sample: stop them now.
            for v in self.voices.iter_mut().filter(|v| v.active && v.slot == b.slot) {
                v.active = false;
            }
            std::mem::swap(&mut self.samples[b.slot], &mut b.data);
            self.give_back(b);
        }
    }

    fn give_back(&mut self, b: Box<SampleSlot>) {
        let Some(garbage) = self.garbage.as_mut() else { return };
        if let Err(rtrb::PushError::Full(b)) = garbage.push(b) {
            self.held_back = Some(b); // try again next block rather than free it here
        }
    }

    fn fire_step(&mut self, shared: &SamplerShared, steps: usize) {
        let step = self.next_step;
        let length = shared.step_frames(step, self.rate);
        // Pads whose hit was recorded onto this step a moment ago already sounded.
        let skip = if step == self.skip_step { self.skip_pads } else { 0 };
        self.skip_pads = 0;
        for pad in 0..PADS {
            let c = &shared.pads[pad];
            let on = c.pattern.load(Ordering::Relaxed) & (1u64 << step) != 0;
            if on && c.flags.load(Ordering::Relaxed) & MUTE == 0 && skip & (1 << pad) == 0 {
                let vel = shared.velocities[pad][step].load(Ordering::Relaxed) as f32 / VEL_MAX as f32;
                self.start(shared, pad, Hold::Frames(length), vel);
            }
        }
        if step.is_multiple_of(STEPS_PER_BEAT) && shared.metronome.load(Ordering::Relaxed) {
            self.start_click(step == 0);
        }
        shared.step.store(step as u32, Ordering::Relaxed);
        self.countdown += length;
        self.next_step = (step + 1) % steps;
    }

    /// Writes a hand-played hit on `pad` into the nearest step. Where the
    /// sequencer is comes from `countdown`, the frames left in the step that
    /// fired last: in its first half the hit goes on that step, in its second
    /// half on the next one, which the sequencer then skips for this pad once.
    fn record_hit(&mut self, shared: &SamplerShared, pad: usize, steps: usize, vel: f32) {
        let current = (self.next_step + steps - 1) % steps;
        let length = shared.step_frames(current, self.rate);
        let step = if self.countdown > length * 0.5 {
            current
        } else {
            self.skip_step = self.next_step;
            self.skip_pads |= 1 << pad;
            self.next_step
        };
        shared.pads[pad].pattern.fetch_or(1u64 << step, Ordering::Relaxed);
        shared.set_velocity(pad, step, vel);
        shared.recorded.fetch_add(1, Ordering::Release);
    }

    fn start_click(&mut self, downbeat: bool) {
        let hz = if downbeat { CLICK_DOWNBEAT_HZ } else { CLICK_BEAT_HZ };
        self.click_inc = hz / self.rate as f32;
        self.click_phase = 0.0;
        self.click_env = 1.0;
        self.click_fade = 0.0;
        self.click_left = self.click_len;
    }

    /// One frame of the metronome click, or 0 when none is sounding.
    fn click_frame(&mut self) -> f32 {
        if self.click_left == 0 {
            return 0.0;
        }
        self.click_left -= 1;
        self.click_phase += self.click_inc;
        if self.click_phase >= 1.0 {
            self.click_phase -= 1.0;
        }
        self.click_env *= self.click_decay;
        self.click_fade = (self.click_fade + self.click_fade_step).min(1.0);
        CLICK_GAIN * self.click_env * self.click_fade * (self.click_phase * std::f32::consts::TAU).sin()
    }

    /// Releases the voices of every other pad in choke group `group`.
    fn choke(&mut self, shared: &SamplerShared, slot: usize, group: u8) {
        let fast = self.release;
        for v in self.voices.iter_mut().filter(|v| v.active && !v.fast && v.slot != slot && v.slot < PADS) {
            let flags = shared.pads[v.slot].flags.load(Ordering::Relaxed);
            if ((flags >> CHOKE_SHIFT) & CHOKE_MASK) as u8 == group {
                v.release(Some(fast));
            }
        }
    }

    /// Retunes every sounding voice's filter from its pad, once per block, so
    /// turning the cutoff while a pad rings needs no per-sample trig.
    fn tune_filters(&mut self, shared: &SamplerShared) {
        for v in self.voices.iter_mut().filter(|v| v.active) {
            let c = &shared.pads[v.slot];
            let mode = c.filter.load(Ordering::Relaxed);
            if v.filter.mode == FILTER_OFF && mode != FILTER_OFF {
                v.filter.reset(); // switched on while ringing: start from silence
            }
            let f = |a: &AtomicU32| f32::from_bits(a.load(Ordering::Relaxed));
            v.filter.tune(mode, f(&c.cutoff), f(&c.resonance), self.rate);
        }
    }

    /// Starts a voice on `slot`, cutting what the slot was already playing.
    /// `vel` (0..1) scales its gain.
    fn start(&mut self, shared: &SamplerShared, slot: usize, hold: Hold, vel: f32) {
        self.release_slot(slot);
        let Some(sample) = self.samples[slot].as_ref() else { return };
        let len = sample.first().map_or(0, Vec::len);
        if len < 2 {
            return;
        }
        let p = shared.pads[slot].params();
        let lo = (p.start as f64 * len as f64).floor();
        let hi = (p.end as f64 * len as f64).min(len as f64 - 1.0);
        if hi - lo < 1.0 {
            return;
        }
        let speed = 2f64.powf(p.pitch as f64 / 12.0);
        // Only gate-mode pads stop early; everything else plays to its end point.
        let hold = if p.gate && slot != PREVIEW { hold } else { Hold::Free };
        if slot < PADS && p.choke > 0 {
            self.choke(shared, slot, p.choke);
        }
        let i = match self.voices.iter().position(|v| !v.active) {
            Some(i) => i,
            None => (0..VOICES).min_by_key(|&i| self.voices[i].age).unwrap_or(0),
        };
        self.clock += 1;
        let rate = self.rate;
        let attack_secs = ATTACK_SECONDS.max(f64::from(p.attack));
        let release_secs = f64::from(p.release).max(MIN_RELEASE_SECONDS);
        // The default (0.004 as an f32) is the 4 ms fade exactly, as before.
        let release_secs = if (release_secs - RELEASE_SECONDS).abs() < 1e-6 { RELEASE_SECONDS } else { release_secs };
        let dec_coef = if p.decay > 0.0 {
            // Exponential, `decay` seconds to fall 60 dB of the way to sustain.
            (0.001f64.powf(1.0 / (rate * f64::from(p.decay))) as f32).max(1.0e-20)
        } else {
            0.0
        };
        let mut filter = VoiceFilter::default();
        filter.tune(p.filter, p.cutoff, p.resonance, rate);
        self.voices[i] = Voice {
            active: true,
            slot,
            pos: if p.reverse { hi } else { lo },
            inc: if p.reverse { -speed } else { speed },
            lo,
            hi,
            // A very short sample fades over its last quarter instead.
            fade: self.edge.min((hi - lo) / speed / 4.0).max(1.0),
            left: vel * p.gain * (1.0 - p.pan).min(1.0),
            right: vel * p.gain * (1.0 + p.pan).min(1.0),
            env: 0.0,
            releasing: false,
            hold,
            age: self.clock,
            stage: Stage::Attack,
            atk_inc: (1.0 / (rate * attack_secs)).min(1.0) as f32,
            dec_coef,
            sustain: p.sustain,
            rel_full: (1.0 / (rate * release_secs)).min(1.0) as f32,
            rel_step: 0.0,
            fast: false,
            filter,
        };
        shared.pads[slot].played.fetch_add(1, Ordering::Relaxed);
    }

    fn release_slot(&mut self, slot: usize) {
        let fast = self.release;
        for v in self.voices.iter_mut().filter(|v| v.active && v.slot == slot) {
            v.release(Some(fast));
        }
    }

    fn mix_frame(&mut self, shared: &SamplerShared) -> (f32, f32) {
        let mut left = 0.0f32;
        let mut right = 0.0f32;
        for v in self.voices.iter_mut().filter(|v| v.active) {
            let Some(sample) = self.samples[v.slot].as_ref() else {
                v.active = false;
                continue;
            };
            match v.hold {
                Hold::Held if !shared.pads[v.slot].held.load(Ordering::Relaxed) => {
                    if !v.releasing {
                        v.release(None);
                    }
                }
                Hold::Frames(n) if n <= 0.0 => {
                    if !v.releasing {
                        v.release(None);
                    }
                }
                Hold::Frames(n) => v.hold = Hold::Frames(n - 1.0),
                _ => {}
            }
            if v.releasing {
                v.env -= v.rel_step;
                if v.env <= 0.0 {
                    v.active = false;
                    continue;
                }
            } else {
                match v.stage {
                    Stage::Attack => {
                        v.env = (v.env + v.atk_inc).min(1.0);
                        if v.env >= 1.0 {
                            v.stage = if v.dec_coef > 0.0 { Stage::Decay } else { Stage::Sustain };
                        }
                    }
                    Stage::Decay => {
                        v.env = v.sustain + (v.env - v.sustain) * v.dec_coef;
                        if (v.env - v.sustain).abs() < DECAY_SNAP {
                            v.env = v.sustain;
                            v.stage = Stage::Sustain;
                            if v.sustain <= 0.0 {
                                v.active = false; // decayed to silence: the voice is done
                                continue;
                            }
                        }
                    }
                    Stage::Sustain => {}
                }
            }
            // Fade over the last few milliseconds before the end point.
            let to_edge = if v.inc > 0.0 { (v.hi - v.pos) / v.inc } else { (v.pos - v.lo) / -v.inc };
            let edge = (to_edge / v.fade).clamp(0.0, 1.0) as f32;
            let g = v.env * edge;

            let i = v.pos as usize;
            let frac = (v.pos - i as f64) as f32;
            let j = (i + 1).min(sample[0].len() - 1);
            let read = |c: &[f32]| c[i] + (c[j] - c[i]) * frac;
            let mut l = read(&sample[0]);
            let mut r = if sample.len() > 1 { read(&sample[1]) } else { l };
            if v.filter.mode != FILTER_OFF {
                l = v.filter.process(0, l);
                r = v.filter.process(1, r);
            }
            left += g * v.left * l;
            right += g * v.right * r;

            v.pos += v.inc;
            if v.pos >= v.hi || v.pos <= v.lo && v.inc < 0.0 {
                v.active = false;
            }
        }
        (left, right)
    }
}

/// Renders `loops` passes of the pattern with `samples` (at `rate`) to
/// stereo, offline and sample-exact. `shared` holds the pads and pattern;
/// its transport state is not touched.
pub fn render_pattern(samples: &[Option<Track>], shared: &SamplerShared, rate: u32, loops: usize) -> Vec<Vec<f32>> {
    let frames = shared.pattern_frames(rate, loops);
    let mut sampler = Sampler::offline(rate);
    for (slot, s) in samples.iter().enumerate().take(PADS) {
        sampler.set_sample(slot, s.clone());
    }
    // A private copy of the transport, so the live sampler isn't started.
    let local = SamplerShared::default();
    for pad in 0..PADS {
        local.set_pad(pad, &shared.pad(pad));
        local.set_pattern(pad, shared.pattern(pad));
        for step in 0..MAX_STEPS {
            local.set_velocity(pad, step, shared.velocity(pad, step));
        }
    }
    local.set_tempo(shared.tempo());
    local.set_swing(shared.swing());
    local.set_steps(shared.steps());
    local.set_playing(true);

    let mut out = vec![Vec::with_capacity(frames), Vec::with_capacity(frames)];
    let mut block = vec![0.0f32; 1024 * 2];
    let mut done = 0;
    while done < frames {
        let n = (frames - done).min(1024);
        let buf = &mut block[..n * 2];
        buf.fill(0.0);
        sampler.render(&local, buf, 2);
        for f in buf.chunks(2) {
            out[0].push(f[0]);
            out[1].push(f[1]);
        }
        done += n;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const RATE: u32 = 48_000;

    /// A mono click: full scale for 10 frames, then silence.
    fn click(frames: usize) -> Track {
        Arc::new(vec![(0..frames).map(|i| if i < 10 { 1.0 } else { 0.0 }).collect()])
    }

    fn dc(frames: usize, v: f32) -> Track {
        Arc::new(vec![vec![v; frames], vec![-v; frames]])
    }

    fn run(s: &mut Sampler, shared: &SamplerShared, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        s.render(shared, &mut out, 2);
        out
    }

    fn onsets(left: &[f32]) -> Vec<usize> {
        let mut v = Vec::new();
        let mut was = false;
        for (i, &x) in left.iter().enumerate() {
            let on = x.abs() > 0.01;
            if on && !was {
                v.push(i);
            }
            was = on;
        }
        v
    }

    #[test]
    fn steps_land_on_the_sample() {
        let shared = SamplerShared::default();
        shared.set_tempo(120.0); // a sixteenth is 6,000 frames at 48 kHz
        shared.set_pattern(0, 0b0001_0001); // steps 1 and 5
        shared.set_steps(8);
        let out = render_pattern(&[Some(click(100))], &shared, RATE, 2);
        assert_eq!(out[0].len(), 8 * 6_000 * 2);
        assert_eq!(onsets(&out[0]), vec![0, 24_000, 48_000, 72_000]);
    }

    #[test]
    fn swing_pushes_the_off_beats() {
        let shared = SamplerShared::default();
        shared.set_tempo(120.0);
        shared.set_swing(1.0); // a triplet feel: 8,000 then 4,000 frames
        shared.set_steps(4);
        shared.set_pattern(0, 0b1111);
        let out = render_pattern(&[Some(click(100))], &shared, RATE, 1);
        assert_eq!(onsets(&out[0]), vec![0, 8_000, 12_000, 20_000]);
    }

    #[test]
    fn muted_pads_skip_the_sequence() {
        let shared = SamplerShared::default();
        shared.set_steps(4);
        shared.set_pattern(0, 0b1111);
        shared.set_pad(0, &PadParams { mute: true, ..Default::default() });
        let out = render_pattern(&[Some(click(100))], &shared, RATE, 1);
        assert!(out[0].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn gain_pan_and_reverse() {
        let shared = SamplerShared::default();
        shared.set_steps(1);
        shared.set_pattern(0, 1);
        shared.set_pad(0, &PadParams { gain: 0.5, pan: 1.0, ..Default::default() });
        let out = render_pattern(&[Some(dc(10_000, 0.4))], &shared, RATE, 1);
        let mid = 5_000;
        assert_eq!(out[0][mid], 0.0, "hard right leaves nothing on the left");
        assert!((out[1][mid] + 0.2).abs() < 1e-6, "{}", out[1][mid]);

        // A ramp played backwards starts near its top.
        let ramp: Track = Arc::new(vec![(0..10_000).map(|i| i as f32 / 10_000.0).collect()]);
        shared.set_pad(0, &PadParams { reverse: true, ..Default::default() });
        let out = render_pattern(&[Some(ramp)], &shared, RATE, 1);
        assert!(out[0][100] > 0.95, "{}", out[0][100]);
    }

    #[test]
    fn pitch_up_an_octave_plays_twice_as_fast() {
        let shared = SamplerShared::default();
        shared.set_steps(1);
        shared.set_tempo(60.0); // one step = 12,000 frames
        shared.set_pattern(0, 1);
        shared.set_pad(0, &PadParams { pitch: 12.0, ..Default::default() });
        let out = render_pattern(&[Some(dc(8_000, 0.5))], &shared, RATE, 1);
        let last = out[0].iter().rposition(|&x| x != 0.0).unwrap();
        assert!((3_990..4_000).contains(&last), "ended at {last}");
    }

    #[test]
    fn gate_mode_stops_after_one_step() {
        let shared = SamplerShared::default();
        shared.set_steps(4);
        shared.set_tempo(120.0);
        shared.set_pattern(0, 1);
        shared.set_pad(0, &PadParams { gate: true, ..Default::default() });
        let out = render_pattern(&[Some(dc(24_000, 0.5))], &shared, RATE, 1);
        assert!(out[0][5_000] > 0.49);
        let tail = (RELEASE_SECONDS * RATE as f64) as usize + 2;
        assert!(out[0][6_000 + tail..].iter().all(|&x| x == 0.0), "silent after the step and its release");
    }

    #[test]
    fn hits_from_the_ui_and_cuts() {
        let shared = SamplerShared::default();
        let (mut s, mut link) = sampler(RATE);
        link.send(3, Some(dc(48_000, 0.5))).unwrap();
        run(&mut s, &shared, 16);
        assert_eq!(link.returned.slots(), 1, "the empty box came back");

        shared.trigger(3);
        let out = run(&mut s, &shared, 1_000);
        assert!(out[1998] > 0.49, "playing");
        assert_eq!(shared.played(3), 1);

        shared.cut(3);
        let out = run(&mut s, &shared, 1_000);
        assert_eq!(out[1998], 0.0, "cut");
    }

    #[test]
    fn a_gate_pad_plays_while_held() {
        let shared = SamplerShared::default();
        let (mut s, mut link) = sampler(RATE);
        shared.set_pad(0, &PadParams { gate: true, ..Default::default() });
        link.send(0, Some(dc(48_000, 0.5))).unwrap();
        shared.trigger(0);
        assert!(run(&mut s, &shared, 2_000)[3998] > 0.49);
        assert!(run(&mut s, &shared, 2_000)[3998] > 0.49, "still held");
        shared.release(0);
        assert_eq!(run(&mut s, &shared, 1_000)[1998], 0.0, "let go");
    }

    #[test]
    fn retriggering_does_not_click() {
        let shared = SamplerShared::default();
        let (mut s, mut link) = sampler(RATE);
        link.send(0, Some(dc(48_000, 0.5))).unwrap();
        shared.trigger(0);
        run(&mut s, &shared, 1_000);
        shared.trigger(0);
        let out = run(&mut s, &shared, 1_000);
        let max_jump = out.chunks(2).collect::<Vec<_>>().windows(2).map(|w| (w[1][0] - w[0][0]).abs()).fold(0.0, f32::max);
        assert!(max_jump < 0.05, "jump {max_jump}");
    }

    #[test]
    fn replacing_a_sample_returns_the_old_one() {
        let shared = SamplerShared::default();
        let (mut s, mut link) = sampler(RATE);
        let first = dc(100, 0.1);
        link.send(0, Some(first.clone())).unwrap();
        run(&mut s, &shared, 8);
        link.send(0, Some(dc(100, 0.2))).unwrap();
        run(&mut s, &shared, 8);
        link.collect();
        assert_eq!(Arc::strong_count(&first), 1, "the audio thread let go of it");
    }

    #[test]
    fn stopping_clears_the_step() {
        let shared = SamplerShared::default();
        let mut s = Sampler::offline(RATE);
        shared.set_playing(true);
        run(&mut s, &shared, 10);
        assert_eq!(shared.current_step(), Some(0));
        shared.set_playing(false);
        run(&mut s, &shared, 10);
        assert_eq!(shared.current_step(), None);
    }

    #[test]
    fn velocity_scales_a_step() {
        let shared = SamplerShared::default();
        assert_eq!(shared.velocity(0, 0), 1.0);
        assert_eq!(shared.velocity(PADS, 0), 1.0, "a bad index reads full");
        shared.set_velocity(0, 1, 0.5);
        shared.set_velocity(0, 2, 7.0);
        shared.set_velocity(0, 3, -1.0);
        shared.set_velocity(PADS, 0, 0.1); // ignored
        assert!((shared.velocity(0, 1) - 0.5).abs() < 0.003);
        assert_eq!(shared.velocity(0, 2), 1.0, "clamped");
        assert_eq!(shared.velocity(0, 3), 0.0, "clamped");

        shared.set_velocity(0, 3, 1.0);
        shared.set_steps(4);
        shared.set_tempo(120.0);
        shared.set_pattern(0, 0b11); // steps 0 and 1, 6,000 frames each
        let out = render_pattern(&[Some(dc(20_000, 0.4))], &shared, RATE, 1);
        assert!((out[0][3_000] - 0.4).abs() < 1e-3, "step 0 at full: {}", out[0][3_000]);
        assert!((out[0][9_000] - 0.2).abs() < 2e-3, "step 1 at half: {}", out[0][9_000]);

        // A hit from the UI plays at full.
        let live = SamplerShared::default();
        live.set_velocity(0, 0, 0.1);
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(dc(20_000, 0.4)));
        live.trigger(0);
        assert!((run(&mut s, &live, 1_000)[1_998] - 0.4).abs() < 1e-3);
    }

    #[test]
    fn choke_releases_the_other_pads_in_the_group() {
        let shared = SamplerShared::default();
        let (mut s, mut link) = sampler(RATE);
        for (pad, level, choke) in [(0, 0.01, 1), (1, 0.02, 1), (2, 0.04, 2), (3, 0.08, 0), (4, 0.16, 0)] {
            shared.set_pad(pad, &PadParams { choke, ..Default::default() });
            link.send(pad, Some(dc(48_000, level))).unwrap();
        }
        assert_eq!(shared.pad(1).choke, 1);
        for pad in [0, 2, 3] {
            shared.trigger(pad);
        }
        let out = run(&mut s, &shared, 1_000);
        assert!((out[1998] - 0.13).abs() < 1e-4, "{}", out[1998]);

        shared.trigger(1); // chokes pad 0 only
        let out = run(&mut s, &shared, 1_000);
        assert!((out[1998] - 0.14).abs() < 1e-4, "pad 0 gone, 2 and 3 still sound: {}", out[1998]);
        let drop = out.chunks(2).map(|f| f[0]).fold(f32::MAX, f32::min);
        assert!(drop > 0.12, "released with a fade, not a hole: {drop}");

        shared.trigger(4); // group 0 chokes nothing
        let out = run(&mut s, &shared, 1_000);
        assert!((out[1998] - 0.30).abs() < 1e-4, "{}", out[1998]);
    }

    #[test]
    fn choke_from_the_sequencer() {
        let shared = SamplerShared::default();
        shared.set_steps(2);
        shared.set_tempo(120.0);
        shared.set_pattern(0, 0b01);
        shared.set_pattern(1, 0b10);
        let samples = [Some(dc(20_000, 0.1)), Some(dc(20_000, 0.2))];
        let at = |out: &[Vec<f32>], i: usize| out[0][i];

        let out = render_pattern(&samples, &shared, RATE, 1);
        assert!((at(&out, 9_000) - 0.3).abs() < 1e-4, "both ring without a group");

        let grouped = PadParams { choke: 3, ..Default::default() };
        shared.set_pad(0, &grouped);
        shared.set_pad(1, &grouped);
        let out = render_pattern(&samples, &shared, RATE, 1);
        assert!((at(&out, 3_000) - 0.1).abs() < 1e-4);
        assert!((at(&out, 9_000) - 0.2).abs() < 1e-4, "pad 0 choked: {}", at(&out, 9_000));
    }

    /// Frames where a click begins: a nonzero sample after a stretch of silence.
    fn click_starts(left: &[f32]) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut last = None::<usize>;
        for (i, &x) in left.iter().enumerate() {
            if x != 0.0 {
                if last.is_none_or(|l| i - l > 200) {
                    starts.push(i);
                }
                last = Some(i);
            }
        }
        starts
    }

    fn crossings(x: &[f32]) -> usize {
        x.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count()
    }

    #[test]
    fn the_metronome_clicks_on_every_beat() {
        let shared = SamplerShared::default();
        shared.set_tempo(120.0); // a sixteenth is 6,000 frames, a beat 24,000
        shared.set_steps(16);
        shared.set_metronome(true);
        assert!(shared.metronome());
        shared.set_playing(true);
        let mut s = Sampler::offline(RATE);
        let mut left = Vec::new();
        for _ in 0..4 {
            left.extend(run(&mut s, &shared, 24_000).chunks(2).map(|f| f[0]));
        }
        assert_eq!(click_starts(&left), vec![0, 24_000, 48_000, 72_000], "on the step's frame, no other");
        assert_ne!(left[0], 0.0, "sounding on the very first frame");

        let peak = left.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((0.2..=0.26).contains(&peak), "about -12 dBFS: {peak}");
        let jump = left[..1_500].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(jump < 0.06, "no click from the attack: {jump}");
        assert!(left[1_450..24_000].iter().all(|&x| x == 0.0), "about 30 ms long");

        let down = crossings(&left[..1_440]);
        let beat = crossings(&left[24_000..25_440]);
        assert!(down > beat * 3 / 2, "higher on step 0: {down} vs {beat} crossings");
    }

    #[test]
    fn the_metronome_marks_the_top_of_every_pass() {
        let shared = SamplerShared::default();
        shared.set_tempo(120.0);
        shared.set_steps(8); // two beats a pass
        shared.set_metronome(true);
        shared.set_playing(true);
        let mut s = Sampler::offline(RATE);
        let left: Vec<f32> = run(&mut s, &shared, 8 * 6_000 * 2).chunks(2).map(|f| f[0]).collect();
        assert_eq!(click_starts(&left), vec![0, 24_000, 48_000, 72_000]);
        assert!(crossings(&left[48_000..49_440]) > crossings(&left[24_000..25_440]) * 3 / 2);
    }

    #[test]
    fn the_metronome_is_silent_when_off_or_stopped() {
        let mut s = Sampler::offline(RATE);
        let shared = SamplerShared::default();
        assert!(!shared.metronome(), "off by default");
        shared.set_playing(true);
        assert!(run(&mut s, &shared, 30_000).iter().all(|&x| x == 0.0), "off");

        let shared = SamplerShared::default();
        let mut s = Sampler::offline(RATE);
        shared.set_metronome(true);
        assert!(run(&mut s, &shared, 30_000).iter().all(|&x| x == 0.0), "stopped");

        // An export stays clean even when the live metronome is on.
        shared.set_steps(4);
        let out = render_pattern(&[], &shared, RATE, 1);
        assert!(out[0].iter().all(|&x| x == 0.0), "render_pattern leaves it off");
    }

    /// A sampler playing at 120 BPM, 16 steps (6,000 frames each), pad 0 loaded.
    fn recording_rig() -> (Sampler, SamplerShared) {
        let shared = SamplerShared::default();
        shared.set_tempo(120.0);
        shared.set_steps(16);
        shared.set_playing(true);
        shared.set_recording(true);
        assert!(shared.is_recording());
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(dc(40_000, 0.1)));
        (s, shared)
    }

    #[test]
    fn a_hit_just_after_a_step_lands_on_that_step() {
        let (mut s, shared) = recording_rig();
        run(&mut s, &shared, 12_500); // steps 0, 1, 2 have fired; 500 frames into step 2
        assert_eq!(shared.current_step(), Some(2));
        shared.trigger(0);
        let out = run(&mut s, &shared, 100);
        assert!(out[0] != 0.0 || out[198] > 0.0, "the hit still sounds at once");
        assert_eq!(shared.pattern(0), 1 << 2);
        assert_eq!(shared.recorded(), 1);
        assert_eq!(shared.velocity(0, 2), 1.0);
        assert_eq!(shared.played(0), 1);
    }

    #[test]
    fn a_hit_just_before_a_step_lands_on_the_next_and_does_not_double() {
        let (mut s, shared) = recording_rig();
        run(&mut s, &shared, 12_000 + 4_500); // 4,500 frames into step 2
        shared.trigger(0);
        run(&mut s, &shared, 100);
        assert_eq!(shared.pattern(0), 1 << 3, "the next step");
        assert_eq!(shared.recorded(), 1);
        assert_eq!(shared.played(0), 1);

        run(&mut s, &shared, 2_000); // step 3 fires at 18,000
        assert_eq!(shared.current_step(), Some(3));
        assert_eq!(shared.played(0), 1, "the sequencer does not play it again this pass");

        run(&mut s, &shared, 16 * 6_000 - 100); // on to 114,500: step 3 of the next pass fires at 114,000
        assert_eq!(shared.current_step(), Some(3));
        assert_eq!(shared.played(0), 2, "on the next loop it plays");
    }

    #[test]
    fn recording_only_when_recording_and_playing() {
        let (mut s, shared) = recording_rig();
        shared.set_recording(false);
        run(&mut s, &shared, 12_500);
        shared.trigger(0);
        run(&mut s, &shared, 100);
        assert_eq!(shared.pattern(0), 0);
        assert_eq!(shared.recorded(), 0);
        assert_eq!(shared.played(0), 1, "still sounds");

        // Recording while stopped changes nothing either.
        let shared = SamplerShared::default();
        shared.set_recording(true);
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(dc(1_000, 0.1)));
        shared.trigger(0);
        run(&mut s, &shared, 100);
        assert_eq!(shared.pattern(0), 0);
        assert_eq!(shared.recorded(), 0);
        assert_eq!(shared.played(0), 1);
    }

    #[test]
    fn the_preview_slot_is_not_recorded() {
        let (mut s, shared) = recording_rig();
        s.set_sample(PREVIEW, Some(dc(1_000, 0.1)));
        run(&mut s, &shared, 12_500);
        shared.trigger(PREVIEW);
        run(&mut s, &shared, 100);
        assert_eq!(shared.recorded(), 0);
        assert!((0..PADS).all(|p| shared.pattern(p) == 0));
    }

    #[test]
    fn a_hit_in_the_last_step_wraps_to_step_zero() {
        let (mut s, shared) = recording_rig();
        shared.set_steps(4);
        run(&mut s, &shared, 3 * 6_000 + 4_500); // late in step 3
        shared.trigger(0);
        run(&mut s, &shared, 100);
        assert_eq!(shared.pattern(0), 1, "wrapped to step 0");
    }

    // ---- Sound shaping: amp envelope, filter, hit velocity ----

    /// Plays `sample` on pad 0 with `p`, hit by hand, and returns the left channel.
    fn play(p: &PadParams, sample: Track, frames: usize) -> Vec<f32> {
        let shared = SamplerShared::default();
        shared.set_pad(0, p);
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(sample));
        shared.trigger(0);
        run(&mut s, &shared, frames).chunks(2).map(|f| f[0]).collect()
    }

    fn sine(hz: f32, frames: usize) -> Track {
        Arc::new(vec![(0..frames).map(|i| (std::f32::consts::TAU * hz * i as f32 / RATE as f32).sin()).collect()])
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// RMS of a long sine through a pad's filter, once the filter has settled.
    fn through(p: &PadParams, hz: f32) -> f32 {
        let out = play(p, sine(hz, 48_000), 40_000);
        rms(&out[8_000..])
    }

    #[test]
    fn default_shaping_is_the_old_sound() {
        let d = PadParams::default();
        assert_eq!((d.attack, d.decay, d.sustain, d.filter, d.cutoff, d.resonance), (0.0, 0.0, 1.0, 0, 20_000.0, 0.0));
        let shared = SamplerShared::default();
        assert_eq!(shared.pad(0), d, "set and read back");

        // The old fade-in: 0.5 ms (24 frames) up from zero.
        let out = play(&d, dc(10_000, 0.5), 200);
        for (i, &x) in out.iter().enumerate().take(30) {
            let want = 0.5 * ((i + 1) as f32 / 24.0).min(1.0);
            assert!((x - want).abs() < 1e-5, "frame {i}: {x} vs {want}");
        }
        // Settings that cannot matter leave every sample alone, bit for bit.
        let plain = play(&d, sine(440.0, 5_000), 5_000);
        let same = PadParams { sustain: 0.2, filter: 0, cutoff: 100.0, resonance: 1.0, attack: 0.0001, ..d };
        assert_eq!(plain, play(&same, sine(440.0, 5_000), 5_000), "no decay, filter off, attack under the minimum");
    }

    #[test]
    fn the_default_release_is_the_old_four_ms_fade() {
        let shared = SamplerShared::default();
        shared.set_pad(0, &PadParams { gate: true, ..Default::default() });
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(dc(48_000, 0.5)));
        shared.trigger(0);
        run(&mut s, &shared, 1_000);
        shared.release(0);
        let out: Vec<f32> = run(&mut s, &shared, 400).chunks(2).map(|f| f[0]).collect();
        // 4 ms is 192 frames, a straight line down from 0.5.
        for (i, &x) in out.iter().enumerate().take(190) {
            let want = 0.5 * (1.0 - (i + 1) as f32 / 192.0);
            assert!((x - want).abs() < 1e-4, "frame {i}: {x} vs {want}");
        }
        assert_eq!(out[196], 0.0);
    }

    #[test]
    fn attack_reaches_full_level_on_time() {
        let p = PadParams { attack: 0.1, ..Default::default() }; // 4,800 frames
        let out = play(&p, dc(48_000, 0.5), 6_000);
        assert!((out[2_399] - 0.25).abs() < 0.002, "halfway: {}", out[2_399]);
        assert!(out[4_700] < 0.49, "not there yet: {}", out[4_700]);
        assert!((out[4_799] - 0.5).abs() < 1e-4, "there: {}", out[4_799]);
        assert_eq!(out[5_500], 0.5);
        assert!(out.windows(2).all(|w| w[1] >= w[0]), "rises straight up");
    }

    #[test]
    fn decay_falls_to_the_sustain_level() {
        // 0.2 s to fall 60 dB of the way from full to 0.4.
        let p = PadParams { decay: 0.2, sustain: 0.4, ..Default::default() };
        let out = play(&p, dc(96_000, 0.5), 48_000);
        let at = |sec: f32| out[(sec * RATE as f32) as usize] / 0.5;
        assert!(at(0.001) > 0.97, "starts near full: {}", at(0.001));
        assert!((at(0.1) - (0.4 + 0.6 * 0.001f32.powf(0.5))).abs() < 2e-3, "halfway through: {}", at(0.1));
        assert!((at(0.2) - (0.4 + 0.6 * 0.001)).abs() < 2e-3, "-60 dB of the way at 0.2 s: {}", at(0.2));
        assert!((at(0.6) - 0.4).abs() < 1e-6, "holding: {}", at(0.6));
        assert!((at(0.99) - 0.4).abs() < 1e-6);
    }

    #[test]
    fn a_one_shot_that_decays_to_nothing_frees_its_voice() {
        let shared = SamplerShared::default();
        shared.set_pad(0, &PadParams { decay: 0.05, sustain: 0.0, ..Default::default() });
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(dc(480_000, 0.5)));
        shared.trigger(0);
        run(&mut s, &shared, 20_000);
        assert!(s.voices.iter().all(|v| !v.active));
    }

    #[test]
    fn release_takes_its_time_on_a_gate_pad() {
        let shared = SamplerShared::default();
        shared.set_pad(0, &PadParams { gate: true, release: 0.1, ..Default::default() });
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(dc(96_000, 0.5)));
        shared.trigger(0);
        run(&mut s, &shared, 2_000);
        shared.release(0);
        let out: Vec<f32> = run(&mut s, &shared, 6_000).chunks(2).map(|f| f[0]).collect();
        assert!((out[2_399] - 0.25).abs() < 0.002, "halfway down at 50 ms: {}", out[2_399]);
        assert!(out[4_700] > 0.0, "still fading");
        assert_eq!(out[4_820], 0.0, "silent after 100 ms");
    }

    #[test]
    fn release_lasts_the_same_from_the_sustain_level() {
        let p = PadParams { gate: true, decay: 0.01, sustain: 0.4, release: 0.1, ..Default::default() };
        let shared = SamplerShared::default();
        shared.set_pad(0, &p);
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(dc(96_000, 0.5)));
        shared.trigger(0);
        run(&mut s, &shared, 20_000);
        shared.release(0);
        let out: Vec<f32> = run(&mut s, &shared, 6_000).chunks(2).map(|f| f[0]).collect();
        assert!((out[0] - 0.2).abs() < 0.01, "starts at the sustain level: {}", out[0]);
        assert!((out[2_399] - 0.1).abs() < 0.005, "{}", out[2_399]);
        assert_eq!(out[4_820], 0.0, "still 100 ms, not 40");
    }

    #[test]
    fn a_sequencer_gate_step_ends_with_the_release() {
        let shared = SamplerShared::default();
        shared.set_steps(4);
        shared.set_tempo(120.0); // 6,000 frames a step
        shared.set_pattern(0, 1);
        shared.set_pad(0, &PadParams { gate: true, release: 0.05, ..Default::default() });
        let out = render_pattern(&[Some(dc(48_000, 0.5))], &shared, RATE, 1);
        assert!(out[0][5_999] > 0.49);
        assert!((out[0][6_000 + 1_200] - 0.25).abs() < 0.01, "half way down 25 ms after the step: {}", out[0][7_200]);
        assert_eq!(out[0][6_000 + 2_420], 0.0);
    }

    #[test]
    fn cuts_stay_tight_whatever_the_release() {
        let shared = SamplerShared::default();
        shared.set_pad(0, &PadParams { gate: true, release: 3.0, ..Default::default() });
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(dc(480_000, 0.5)));
        shared.trigger(0);
        run(&mut s, &shared, 1_000);
        shared.cut(0);
        let out: Vec<f32> = run(&mut s, &shared, 400).chunks(2).map(|f| f[0]).collect();
        assert_eq!(out[200], 0.0, "gone after the 4 ms fade");

        // A retrigger while a long release rings is cut tight too.
        let shared = SamplerShared::default();
        shared.set_pad(0, &PadParams { gate: true, release: 3.0, ..Default::default() });
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(dc(480_000, 0.5)));
        shared.trigger(0);
        run(&mut s, &shared, 1_000);
        shared.release(0);
        run(&mut s, &shared, 1_000); // ringing out slowly
        shared.trigger(0);
        run(&mut s, &shared, 400);
        assert_eq!(s.voices.iter().filter(|v| v.active).count(), 1, "the old voice faded in 4 ms");

        // So is a choke.
        let shared = SamplerShared::default();
        for pad in 0..2 {
            shared.set_pad(pad, &PadParams { gate: true, release: 3.0, choke: 1, ..Default::default() });
        }
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(dc(480_000, 0.5)));
        s.set_sample(1, Some(dc(480_000, 0.25)));
        shared.trigger(0);
        run(&mut s, &shared, 1_000);
        shared.trigger(1);
        let out: Vec<f32> = run(&mut s, &shared, 400).chunks(2).map(|f| f[0]).collect();
        assert!((out[399] - 0.25).abs() < 1e-4, "pad 0 choked in 4 ms: {}", out[399]);
    }

    fn lp(cutoff: f32, resonance: f32) -> PadParams {
        PadParams { filter: FILTER_LOW_PASS, cutoff, resonance, ..Default::default() }
    }

    #[test]
    fn a_low_pass_keeps_the_lows_and_cuts_the_highs() {
        let input = rms(&sine(200.0, 48_000)[0][8_000..40_000]);
        let low = through(&lp(1_000.0, 0.0), 200.0);
        let high = through(&lp(1_000.0, 0.0), 8_000.0);
        assert!((low / input - 1.0).abs() < 0.05, "200 Hz passes: {}", low / input);
        assert!(high / input < 0.05, "8 kHz is down 26 dB or more: {}", high / input);
    }

    #[test]
    fn a_high_pass_keeps_the_highs_and_cuts_the_lows() {
        let p = PadParams { filter: FILTER_HIGH_PASS, cutoff: 2_000.0, ..Default::default() };
        let input = rms(&sine(200.0, 48_000)[0][8_000..40_000]);
        let high = through(&p, 10_000.0);
        let low = through(&p, 200.0);
        assert!((high / input - 1.0).abs() < 0.05, "10 kHz passes: {}", high / input);
        assert!(low / input < 0.05, "200 Hz is down 26 dB or more: {}", low / input);
    }

    #[test]
    fn a_band_pass_keeps_the_middle() {
        let p = PadParams { filter: FILTER_BAND_PASS, cutoff: 1_000.0, resonance: 0.8, ..Default::default() };
        let input = rms(&sine(1_000.0, 48_000)[0][8_000..40_000]);
        assert!((through(&p, 1_000.0) / input - 1.0).abs() < 0.05, "unity at the centre");
        assert!(through(&p, 100.0) / input < 0.2);
        assert!(through(&p, 10_000.0) / input < 0.2);
    }

    #[test]
    fn resonance_boosts_near_the_cutoff() {
        let flat = through(&lp(1_000.0, 0.0), 1_000.0);
        let peaky = through(&lp(1_000.0, 0.9), 1_000.0);
        assert!(peaky > flat * 5.0, "{peaky} vs {flat}");
        // Far from the cutoff it does not.
        let a = through(&lp(4_000.0, 0.0), 100.0);
        let b = through(&lp(4_000.0, 0.9), 100.0);
        assert!((a - b).abs() / a < 0.05, "{a} vs {b}");
    }

    #[test]
    fn the_filter_follows_the_knob_while_a_pad_rings() {
        let shared = SamplerShared::default();
        shared.set_pad(0, &lp(20_000.0, 0.0));
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(sine(4_000.0, 96_000)));
        shared.trigger(0);
        let first: Vec<f32> = run(&mut s, &shared, 4_800).chunks(2).map(|f| f[0]).collect();
        shared.set_pad(0, &lp(200.0, 0.0));
        let second: Vec<f32> = run(&mut s, &shared, 4_800).chunks(2).map(|f| f[0]).collect();
        assert!(rms(&first[1_000..]) > 0.65, "open: {}", rms(&first[1_000..]));
        assert!(rms(&second[2_400..]) < 0.01, "closed: {}", rms(&second[2_400..]));
        assert!(second.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn the_cutoff_is_kept_below_nyquist() {
        // At a low rate, 20 kHz is above Nyquist; the filter must stay finite and stable.
        let shared = SamplerShared::default();
        shared.set_pad(0, &lp(20_000.0, 1.0));
        let mut s = Sampler::offline(22_050);
        s.set_sample(0, Some(sine(3_000.0, 20_000)));
        shared.trigger(0);
        let out = run(&mut s, &shared, 20_000);
        assert!(out.iter().all(|x| x.is_finite() && x.abs() < 40.0));
        shared.set_pad(0, &PadParams { filter: FILTER_LOW_PASS, cutoff: f32::NAN, resonance: f32::NAN, ..Default::default() });
        assert_eq!(shared.pad(0).cutoff, 20_000.0, "a bad number is the default");
    }

    #[test]
    fn a_new_voice_starts_the_filter_from_silence() {
        let shared = SamplerShared::default();
        shared.set_pad(0, &lp(500.0, 0.9));
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(dc(48_000, 0.5)));
        shared.trigger(0);
        let a = run(&mut s, &shared, 2_000);
        shared.cut(0);
        run(&mut s, &shared, 500);
        shared.trigger(0);
        let b = run(&mut s, &shared, 2_000);
        assert_eq!(a, b, "the same hit sounds the same twice");
    }

    #[test]
    fn trigger_velocity_scales_the_hit() {
        let shared = SamplerShared::default();
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(dc(20_000, 0.4)));
        shared.trigger_velocity(0, 0.5);
        assert!((run(&mut s, &shared, 1_000)[1_998] - 0.2).abs() < 1e-6, "half level");
        shared.trigger(0);
        assert!((run(&mut s, &shared, 1_000)[1_998] - 0.4).abs() < 1e-6, "trigger is full velocity");
        shared.trigger_velocity(0, 7.0);
        assert!((run(&mut s, &shared, 1_000)[1_998] - 0.4).abs() < 1e-6, "clamped to full");
        shared.trigger_velocity(0, 0.0);
        assert_eq!(run(&mut s, &shared, 1_000)[1_998], 0.0, "zero is silent");
        shared.trigger_velocity(PADS + 5, 0.5); // a bad slot is ignored
    }

    #[test]
    fn a_recorded_hit_keeps_its_velocity() {
        let (mut s, shared) = recording_rig();
        run(&mut s, &shared, 12_500);
        shared.trigger_velocity(0, 0.5);
        let out = run(&mut s, &shared, 100);
        assert_eq!(shared.pattern(0), 1 << 2);
        assert!((shared.velocity(0, 2) - 0.5).abs() < 0.003, "{}", shared.velocity(0, 2));
        assert!((out[198] - 0.05).abs() < 1e-3, "and it sounded at half: {}", out[198]);

        // The next pass plays the step at that velocity.
        run(&mut s, &shared, 16 * 6_000 - 100);
        let out = run(&mut s, &shared, 3_000);
        assert!((out[2_000] - 0.05).abs() < 1e-3, "{}", out[2_000]);
    }

    #[test]
    fn render_pattern_carries_the_shaping() {
        let shared = SamplerShared::default();
        shared.set_steps(1);
        shared.set_tempo(60.0);
        shared.set_pattern(0, 1);
        let p = PadParams { attack: 0.1, decay: 0.2, sustain: 0.5, release: 0.3, filter: FILTER_HIGH_PASS, cutoff: 3_000.0, resonance: 0.5, ..Default::default() };
        shared.set_pad(0, &p);
        assert_eq!(shared.pad(0), p);
        shared.set_pad(0, &PadParams { attack: 9.0, decay: -1.0, sustain: 3.0, release: 99.0, filter: 9, cutoff: 1.0, resonance: 5.0, ..p });
        let c = shared.pad(0);
        assert_eq!((c.attack, c.decay, c.sustain, c.release, c.filter, c.cutoff, c.resonance), (2.0, 0.0, 1.0, 5.0, 3, 20.0, 1.0));
        shared.set_pad(0, &p);

        let out = render_pattern(&[Some(dc(12_000, 0.5))], &shared, RATE, 1);
        assert!(out[0][2_399].abs() < 0.2, "the filter took the DC out: {}", out[0][2_399]);
        let plain = SamplerShared::default();
        plain.set_steps(1);
        plain.set_tempo(60.0);
        plain.set_pattern(0, 1);
        let same = render_pattern(&[Some(dc(12_000, 0.5))], &plain, RATE, 1);
        assert_ne!(out, same, "and it differs from the defaults");
    }
}
