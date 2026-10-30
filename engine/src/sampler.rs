//! The Sampler: 16 pads that play samples, a preview slot for the Library,
//! and a step sequencer that triggers the pads in time.
//!
//! Samples reach the audio thread at the device's rate through a lock-free
//! queue, and old ones go back through a second queue, so the audio thread
//! never frees memory. Everything else (pad settings, the pattern, tempo,
//! triggers from the UI) is an atomic in [`SamplerShared`].
//!
//! The sequencer has no clock of its own: it follows the transport
//! ([`crate::transport`]). Its pattern is laid out on the beat grid from beat
//! 0, a step every sixteenth (with swing), and it counts the beats each frame
//! moves on, so steps land on the sample even across tempo changes, and a
//! loop wrap or a seek finds its place again. Tempo and play or stop are the
//! transport's: [`SamplerShared::set_tempo`] and
//! [`SamplerShared::set_playing`] set the clock it runs on.
//!
//! The sequencer holds [`BANKS`] patterns. Each step of each pad has a
//! velocity, a chance (probability) and a ratchet count. In song mode a chain
//! of banks plays one after the other.
//!
//! Switching banks, or song mode on or off, lands at the end of the pattern
//! (the next wrap) by default, so the pattern playing finishes first; with
//! [`SamplerShared::set_switch_now`] a bank switch or song mode turned off
//! lands on the next step instead. A song always starts at the wrap, so its
//! first entry plays a whole pass. The audio thread makes that call at each
//! step, from atomics only.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use crate::device::{Buffers, Device, Events};
use crate::transport::{ProcessContext, Transport, TransportShared, EDGE_FRAMES};
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
/// How far the filter envelope and velocity can move the cutoff, in octaves either way.
const FENV_OCTAVES: f32 = 8.0;
/// While a filter envelope moves the cutoff, the filter is retuned this often, in frames.
const FENV_TICK: u32 = 16;
/// Velocity to envelope time at full amount: a hit at velocity 0 is this many times shorter.
const VEL_TIME_SPAN: f64 = 4.0;
const NO_STEP: u32 = u32::MAX;
/// The song position when no song is playing.
const NO_POS: u32 = u32::MAX;
/// The bank playing when the sequencer is stopped.
const NO_BANK: u32 = u32::MAX;

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

/// Pattern banks the sequencer holds (A to D).
pub const BANKS: usize = 4;
/// The longest song, in chain entries.
pub const CHAIN_MAX: usize = 64;
/// The most hits one step can be split into.
const RATCHET_MAX: u8 = 4;
/// The seed of an offline sampler's dice, so an export comes out the same every time.
const RNG_SEED: u64 = 0x7475_6E65_4B6F_7265;

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
    /// Filter envelope: moves the cutoff by `fenv_amount` octaves (-8..8, 0 is
    /// off) times the envelope's level on every hit. Its attack (0..2 s), decay
    /// (0..5 s, 0 holds at full), sustain (0..1) and release (0..5 s) work like
    /// the amp envelope's. Does nothing while the filter is off.
    pub fenv_amount: f32,
    pub fenv_attack: f32,
    pub fenv_decay: f32,
    pub fenv_sustain: f32,
    pub fenv_release: f32,
    /// Velocity to cutoff, in octaves, -8..8. A hit at velocity `v` moves the
    /// cutoff by `vel_cutoff * (v - 1)` octaves: with a positive amount a soft
    /// hit is darker, and a full hit sounds as set.
    pub vel_cutoff: f32,
    /// Velocity to envelope time, -1..1. A hit at velocity `v` scales the amp
    /// envelope's attack and decay by `4^(vel_time * (v - 1))`: positive makes
    /// soft hits shorter (a quarter at velocity 0), negative makes them longer.
    pub vel_time: f32,
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
            fenv_amount: 0.0,
            fenv_attack: 0.0,
            fenv_decay: 0.3,
            fenv_sustain: 0.0,
            fenv_release: 0.1,
            vel_cutoff: 0.0,
            vel_time: 0.0,
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
    // Filter envelope and velocity, kept after the fields above. All f32 bits.
    fenv_amount: AtomicU32,
    fenv_attack: AtomicU32,
    fenv_decay: AtomicU32,
    fenv_sustain: AtomicU32,
    fenv_release: AtomicU32,
    vel_cutoff: AtomicU32,
    vel_time: AtomicU32,
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
            fenv_amount: AtomicU32::new(p.fenv_amount.to_bits()),
            fenv_attack: AtomicU32::new(p.fenv_attack.to_bits()),
            fenv_decay: AtomicU32::new(p.fenv_decay.to_bits()),
            fenv_sustain: AtomicU32::new(p.fenv_sustain.to_bits()),
            fenv_release: AtomicU32::new(p.fenv_release.to_bits()),
            vel_cutoff: AtomicU32::new(p.vel_cutoff.to_bits()),
            vel_time: AtomicU32::new(p.vel_time.to_bits()),
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
            fenv_amount: f(&self.fenv_amount),
            fenv_attack: f(&self.fenv_attack),
            fenv_decay: f(&self.fenv_decay),
            fenv_sustain: f(&self.fenv_sustain),
            fenv_release: f(&self.fenv_release),
            vel_cutoff: f(&self.vel_cutoff),
            vel_time: f(&self.vel_time),
        }
    }
}

/// One pattern bank: each pad's steps, and for every step its velocity, its
/// chance (0..=255 for 0..1) and how many hits it is split into (1..=4).
#[derive(Debug)]
struct Bank {
    patterns: [AtomicU64; PADS],
    velocities: [[AtomicU8; MAX_STEPS]; PADS],
    chances: [[AtomicU8; MAX_STEPS]; PADS],
    ratchets: [[AtomicU8; MAX_STEPS]; PADS],
}

impl Default for Bank {
    /// Empty: no steps on, every step at full velocity, always playing, one hit.
    fn default() -> Self {
        let grid = |v: u8| -> [[AtomicU8; MAX_STEPS]; PADS] {
            std::array::from_fn(|_| std::array::from_fn(|_| AtomicU8::new(v)))
        };
        Self {
            patterns: Default::default(),
            velocities: grid(VEL_MAX),
            chances: grid(VEL_MAX),
            ratchets: grid(1),
        }
    }
}

impl Bank {
    fn copy_from(&self, from: &Bank) {
        let r = Ordering::Relaxed;
        for pad in 0..PADS {
            self.patterns[pad].store(from.patterns[pad].load(r), r);
            for step in 0..MAX_STEPS {
                self.velocities[pad][step].store(from.velocities[pad][step].load(r), r);
                self.chances[pad][step].store(from.chances[pad][step].load(r), r);
                self.ratchets[pad][step].store(from.ratchets[pad][step].load(r), r);
            }
        }
    }
}

/// A 0..1 value as 0..=255. NaN reads as 1, the default for velocity and chance.
fn unit_to_u8(v: f32) -> u8 {
    let v = if v.is_nan() { 1.0 } else { v.clamp(0.0, 1.0) };
    (v * VEL_MAX as f32).round() as u8
}

/// One step's cell in a bank's grid, or `None` for a bad pad or step.
fn cell(grid: &[[AtomicU8; MAX_STEPS]; PADS], pad: usize, step: usize) -> Option<&AtomicU8> {
    grid.get(pad).and_then(|p| p.get(step))
}

// Adapted from soundCraft (github.com/storytold/soundcraft, crates/midi/src/rng.rs),
// MIT OR Apache-2.0, Copyright (c) 2026 ArtCraft Team and the SoundCraft contributors.
/// Small deterministic PRNG (xorshift64*) for step chance, so an offline render
/// is the same every time. Plain math on one word: safe on the audio thread.
#[derive(Debug, Clone)]
struct XorShift(u64);

impl XorShift {
    /// Creates a generator. A zero seed is replaced by a fixed non-zero constant.
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    /// Next raw 64-bit value.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// True with probability `chance / 255`: never at 0, always at 255.
    fn roll(&mut self, chance: u8) -> bool {
        // 32 random bits scaled to 0..=254, so `< 255` always passes and `< 0` never does.
        let r = ((self.next_u64() >> 32) * 255) >> 32;
        r < u64::from(chance)
    }
}

/// State shared by the control side and the audio thread. Atomics only, so
/// every method is safe from any thread.
#[derive(Debug)]
pub struct SamplerShared {
    pads: [PadCtl; SLOTS],
    /// The transport the Sampler runs on: the engine's, or its own for an
    /// offline render. Tempo and play or stop live there.
    clock: Arc<TransportShared>,
    swing: AtomicU32, // 0..1, f32 bits
    steps: AtomicU32,
    /// The step that played last, or NO_STEP when stopped. Written by the audio thread.
    step: AtomicU32,
    metronome: AtomicBool,
    recording: AtomicBool,
    /// Bumped by the audio thread for each hit it writes into a pattern.
    recorded: AtomicU32,
    /// The pattern banks: steps, velocity, chance and ratchet of every pad.
    banks: [Bank; BANKS],
    /// The bank being edited and that recording writes to. Outside song mode
    /// it plays from the next switch (see `switch_now`).
    bank: AtomicU32,
    /// The song: bank numbers, `chain_len` of them in use.
    chain: [AtomicU8; CHAIN_MAX],
    chain_len: AtomicU32,
    song_mode: AtomicBool,
    song_loop: AtomicBool,
    /// The chain entry playing, or NO_POS. Written by the audio thread.
    chain_pos: AtomicU32,
    /// Switches land on the next step instead of the next wrap.
    switch_now: AtomicBool,
    /// The bank the sequencer read for its last step, or NO_BANK. Written by the audio thread.
    play_bank: AtomicU32,
}

impl Default for SamplerShared {
    fn default() -> Self {
        Self {
            pads: Default::default(),
            clock: Arc::default(),
            swing: AtomicU32::new(0.0f32.to_bits()),
            steps: AtomicU32::new(16),
            step: AtomicU32::new(NO_STEP),
            metronome: AtomicBool::new(false),
            recording: AtomicBool::new(false),
            recorded: AtomicU32::new(0),
            banks: Default::default(),
            bank: AtomicU32::new(0),
            chain: std::array::from_fn(|_| AtomicU8::new(0)),
            chain_len: AtomicU32::new(0),
            song_mode: AtomicBool::new(false),
            song_loop: AtomicBool::new(false),
            chain_pos: AtomicU32::new(NO_POS),
            switch_now: AtomicBool::new(false),
            play_bank: AtomicU32::new(NO_BANK),
        }
    }
}

impl SamplerShared {
    /// A Sampler that runs on `clock`, the engine's transport.
    pub fn with_clock(clock: Arc<TransportShared>) -> Self {
        Self { clock, ..Default::default() }
    }

    /// The transport this Sampler runs on.
    pub fn clock(&self) -> &Arc<TransportShared> {
        &self.clock
    }

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
        let octaves = FENV_OCTAVES;
        c.fenv_amount.store(sane(p.fenv_amount, -octaves, octaves, d.fenv_amount).to_bits(), Ordering::Relaxed);
        c.fenv_attack.store(sane(p.fenv_attack, 0.0, 2.0, d.fenv_attack).to_bits(), Ordering::Relaxed);
        c.fenv_decay.store(sane(p.fenv_decay, 0.0, 5.0, d.fenv_decay).to_bits(), Ordering::Relaxed);
        c.fenv_sustain.store(sane(p.fenv_sustain, 0.0, 1.0, d.fenv_sustain).to_bits(), Ordering::Relaxed);
        c.fenv_release.store(sane(p.fenv_release, 0.0, 5.0, d.fenv_release).to_bits(), Ordering::Relaxed);
        c.vel_cutoff.store(sane(p.vel_cutoff, -octaves, octaves, d.vel_cutoff).to_bits(), Ordering::Relaxed);
        c.vel_time.store(sane(p.vel_time, -1.0, 1.0, d.vel_time).to_bits(), Ordering::Relaxed);
    }

    pub fn pad(&self, slot: usize) -> PadParams {
        self.pads.get(slot).map(PadCtl::params).unwrap_or_default()
    }

    /// A pad's steps as bits in the current bank: bit `n` set means the pad
    /// plays on step `n`.
    pub fn set_pattern(&self, pad: usize, bits: u64) {
        self.set_bank_pattern(self.bank(), pad, bits);
    }

    /// A pad's steps in the current bank.
    pub fn pattern(&self, pad: usize) -> u64 {
        self.bank_pattern(self.bank(), pad)
    }

    /// How hard a pad plays on a step of the current bank, 0..1 (clamped).
    /// Ignores a bad index.
    pub fn set_velocity(&self, pad: usize, step: usize, v: f32) {
        self.set_bank_velocity(self.bank(), pad, step, v);
    }

    /// A step's velocity in the current bank, 0..1. Every step starts at 1,
    /// and a bad index reads 1.
    pub fn velocity(&self, pad: usize, step: usize) -> f32 {
        self.bank_velocity(self.bank(), pad, step)
    }

    /// Makes `bank` (0 = A) the one being edited, that recording writes to and
    /// that plays outside song mode. Edits apply at once; while playing, the
    /// bank starts at the next wrap of the pattern, or on the next step with
    /// [`set_switch_now`](Self::set_switch_now). Until then
    /// [`playing_bank`](Self::playing_bank) is still the old one. In song mode
    /// the chain decides what plays, and this is only the bank being edited.
    /// Ignores a bad index.
    pub fn set_bank(&self, bank: usize) {
        if bank < BANKS {
            self.bank.store(bank as u32, Ordering::Relaxed);
        }
    }

    /// The current bank, 0..[`BANKS`].
    pub fn bank(&self) -> usize {
        (self.bank.load(Ordering::Relaxed) as usize).min(BANKS - 1)
    }

    /// A pad's steps in `bank`, as bits. Ignores a bad bank or pad.
    pub fn set_bank_pattern(&self, bank: usize, pad: usize, bits: u64) {
        if let Some(a) = self.banks.get(bank).and_then(|b| b.patterns.get(pad)) {
            a.store(bits, Ordering::Relaxed);
        }
    }

    /// A pad's steps in `bank`. A bad index reads 0.
    pub fn bank_pattern(&self, bank: usize, pad: usize) -> u64 {
        self.banks.get(bank).and_then(|b| b.patterns.get(pad)).map_or(0, |a| a.load(Ordering::Relaxed))
    }

    /// A step's velocity in `bank`, 0..1 (clamped). Ignores a bad index.
    pub fn set_bank_velocity(&self, bank: usize, pad: usize, step: usize, v: f32) {
        if let Some(a) = self.banks.get(bank).and_then(|b| cell(&b.velocities, pad, step)) {
            a.store(unit_to_u8(v), Ordering::Relaxed);
        }
    }

    /// A step's velocity in `bank`, 0..1. A bad index reads 1.
    pub fn bank_velocity(&self, bank: usize, pad: usize, step: usize) -> f32 {
        self.banks
            .get(bank)
            .and_then(|b| cell(&b.velocities, pad, step))
            .map_or(1.0, |a| a.load(Ordering::Relaxed) as f32 / VEL_MAX as f32)
    }

    /// The chance an on step plays, 0..1 (clamped; NaN reads as 1). Each time
    /// the sequencer reaches the step it plays only if a dice roll lands under
    /// the chance. 0 never plays and 1 always does. Ignores a bad index.
    pub fn set_chance(&self, bank: usize, pad: usize, step: usize, p: f32) {
        if let Some(a) = self.banks.get(bank).and_then(|b| cell(&b.chances, pad, step)) {
            a.store(unit_to_u8(p), Ordering::Relaxed);
        }
    }

    /// A step's chance, 0..1. Every step starts at 1, and a bad index reads 1.
    pub fn chance(&self, bank: usize, pad: usize, step: usize) -> f32 {
        self.banks
            .get(bank)
            .and_then(|b| cell(&b.chances, pad, step))
            .map_or(1.0, |a| a.load(Ordering::Relaxed) as f32 / VEL_MAX as f32)
    }

    /// How many evenly spaced hits an on step plays within its length, 1..=4
    /// (clamped). Ignores a bad index.
    pub fn set_ratchet(&self, bank: usize, pad: usize, step: usize, n: u8) {
        if let Some(a) = self.banks.get(bank).and_then(|b| cell(&b.ratchets, pad, step)) {
            a.store(n.clamp(1, RATCHET_MAX), Ordering::Relaxed);
        }
    }

    /// A step's ratchet count, 1..=4. Every step starts at 1, and a bad index reads 1.
    pub fn ratchet(&self, bank: usize, pad: usize, step: usize) -> u8 {
        self.banks.get(bank).and_then(|b| cell(&b.ratchets, pad, step)).map_or(1, |a| a.load(Ordering::Relaxed))
    }

    /// Sets the song: the banks to play in order, up to [`CHAIN_MAX`]. Entries
    /// that are not a bank are dropped. Safe while playing: the audio thread
    /// reads the chain at each wrap of the pattern, so a change takes effect
    /// there.
    pub fn set_chain(&self, chain: &[u8]) {
        let mut n = 0;
        for &bank in chain.iter().filter(|&&b| usize::from(b) < BANKS).take(CHAIN_MAX) {
            self.chain[n].store(bank, Ordering::Relaxed);
            n += 1;
        }
        self.chain_len.store(n as u32, Ordering::Release);
    }

    /// The song's banks in order.
    pub fn chain(&self) -> Vec<u8> {
        let n = (self.chain_len.load(Ordering::Acquire) as usize).min(CHAIN_MAX);
        self.chain[..n].iter().map(|a| a.load(Ordering::Relaxed)).collect()
    }

    /// In song mode, playing walks the chain: each entry's bank plays one pass
    /// of the pattern, and the next entry starts exactly at the wrap. With an
    /// empty chain, song mode has nothing to walk and the current bank loops.
    pub fn set_song_mode(&self, on: bool) {
        self.song_mode.store(on, Ordering::Relaxed);
    }

    pub fn song_mode(&self) -> bool {
        self.song_mode.load(Ordering::Relaxed)
    }

    /// With song loop on, the song starts over after its last entry; off, playback stops there.
    pub fn set_song_loop(&self, on: bool) {
        self.song_loop.store(on, Ordering::Relaxed);
    }

    pub fn song_loop(&self) -> bool {
        self.song_loop.load(Ordering::Relaxed)
    }

    /// The chain entry playing now, or `None` when stopped or not walking a
    /// song. Song mode turned on starts entry 0 at the next wrap; turned off,
    /// the entry playing finishes its pass (or, switching now, stops on the
    /// next step), so this can lag [`song_mode`](Self::song_mode).
    pub fn chain_position(&self) -> Option<usize> {
        match self.chain_pos.load(Ordering::Relaxed) {
            NO_POS => None,
            p => Some(p as usize),
        }
    }

    /// With `now` on, a bank switch, or song mode turned off, lands on the
    /// next step, where the sequencer carries on from the same step of the new
    /// bank. Song mode turned on still waits for the wrap, so the song's first
    /// entry plays a whole pass. Off (the default), every switch lands at the
    /// end of the pattern: the bank or song pass playing finishes, and the
    /// next one starts exactly on the wrap. Turning it on while a bank switch
    /// waits makes that switch on the next step.
    pub fn set_switch_now(&self, now: bool) {
        self.switch_now.store(now, Ordering::Relaxed);
    }

    pub fn switch_now(&self) -> bool {
        self.switch_now.load(Ordering::Relaxed)
    }

    /// The bank the sequencer is playing, from its last step, or `None` when
    /// stopped. While a switch waits for the wrap it differs from
    /// [`bank`](Self::bank).
    pub fn playing_bank(&self) -> Option<usize> {
        match self.play_bank.load(Ordering::Relaxed) {
            NO_BANK => None,
            b => Some(b as usize),
        }
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

    /// Sets the tempo of the transport the Sampler runs on, 20..300 BPM.
    pub fn set_tempo(&self, bpm: f32) {
        self.clock.set_tempo(f64::from(bpm.clamp(20.0, 300.0)));
    }

    pub fn tempo(&self) -> f32 {
        self.clock.tempo() as f32
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

    /// Plays the transport the Sampler runs on, from its start (step 1 of
    /// the pattern from beat 0), or stops it.
    pub fn set_playing(&self, on: bool) {
        self.clock.set_playing(on);
    }

    pub fn is_playing(&self) -> bool {
        self.clock.is_playing()
    }

    /// The step playing now, or `None` when stopped.
    pub fn current_step(&self) -> Option<usize> {
        match self.step.load(Ordering::Relaxed) {
            NO_STEP => None,
            s => Some(s as usize),
        }
    }

    /// Length of step `step` in beats, with swing: a sixteenth, with each
    /// even step longer and each odd one shorter by up to a third of one, so
    /// the off-beats land later.
    fn step_beats(&self, step: usize) -> f64 {
        let push = 0.25 * self.swing() as f64 / 3.0;
        if step.is_multiple_of(2) { 0.25 + push } else { 0.25 - push }
    }

    /// Length of step `step` in frames at `rate`, with swing, at the current tempo.
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

    /// Frames the song lasts at `rate`: one pass of the pattern per chain entry.
    pub fn song_frames(&self, rate: u32) -> usize {
        self.pattern_frames(rate, self.chain_len.load(Ordering::Acquire) as usize)
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

/// Where a voice's filter envelope is.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum FenvStage {
    Attack,
    Decay,
    #[default]
    Hold,
    Release,
}

/// A voice's filter envelope: a level, 0..1, that moves the cutoff. Linear
/// attack and release, exponential decay, like the amp envelope.
#[derive(Debug, Clone, Copy, Default)]
struct FilterEnv {
    level: f32,
    stage: FenvStage,
    atk_inc: f32,
    dec_coef: f32,
    sustain: f32,
    rel_full: f32,
    rel_step: f32,
}

impl FilterEnv {
    fn new(p: &PadParams, rate: f64) -> Self {
        let attack = f64::from(p.fenv_attack.max(0.0));
        let decay = f64::from(p.fenv_decay.max(0.0));
        let release = f64::from(p.fenv_release).max(MIN_RELEASE_SECONDS);
        Self {
            level: 0.0,
            stage: FenvStage::Attack,
            atk_inc: if attack > 0.0 { (1.0 / (rate * attack)).min(1.0) as f32 } else { 1.0 },
            dec_coef: if decay > 0.0 { (0.001f64.powf(1.0 / (rate * decay)) as f32).max(1.0e-20) } else { 0.0 },
            sustain: p.fenv_sustain.clamp(0.0, 1.0),
            rel_full: (1.0 / (rate * release)).min(1.0) as f32,
            rel_step: 0.0,
        }
    }

    /// Starts the release from the level it is at, so it lasts `release` seconds.
    fn release(&mut self) {
        if self.stage != FenvStage::Release {
            self.rel_step = self.level * self.rel_full;
            self.stage = FenvStage::Release;
        }
    }

    /// One frame on.
    #[inline]
    fn tick(&mut self) {
        match self.stage {
            FenvStage::Attack => {
                self.level = (self.level + self.atk_inc).min(1.0);
                if self.level >= 1.0 {
                    self.stage = if self.dec_coef > 0.0 { FenvStage::Decay } else { FenvStage::Hold };
                }
            }
            FenvStage::Decay => {
                self.level = self.sustain + (self.level - self.sustain) * self.dec_coef;
                if (self.level - self.sustain).abs() < DECAY_SNAP {
                    self.level = self.sustain;
                    self.stage = FenvStage::Hold;
                }
            }
            FenvStage::Hold => {}
            FenvStage::Release => self.level = (self.level - self.rel_step).max(0.0),
        }
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
    // Filter envelope and velocity, kept after the fields above.
    fenv: FilterEnv,
    /// The pad's cutoff (Hz), resonance and filter envelope amount (octaves),
    /// read once per block so the knobs work while the voice rings.
    cutoff: f32,
    resonance: f32,
    fenv_amount: f32,
    /// The hit's velocity, 0..1, and how far it moves the cutoff, in octaves.
    vel: f32,
    vel_octaves: f32,
    /// Frames until the envelope retunes the filter.
    retune_in: u32,
}

impl Voice {
    /// Tunes the filter to the pad's cutoff, moved by velocity and the filter envelope.
    fn tune_filter(&mut self, mode: u8, rate: f64) {
        let octaves = self.vel_octaves + self.fenv_amount * self.fenv.level;
        self.filter.tune(mode, self.cutoff * octaves.exp2(), self.resonance, rate);
    }

    /// Starts the release. `fast` is the step of a cut (4 ms from full level,
    /// whatever the pad's release is); `None` is the pad's own release, timed
    /// from the level the voice is at, so it lasts `release` seconds.
    fn release(&mut self, fast: Option<f32>) {
        let step = fast.unwrap_or(self.env.max(0.0) * self.rel_full);
        self.rel_step = if self.releasing { self.rel_step.max(step) } else { step };
        self.releasing = true;
        self.fast |= fast.is_some();
        self.fenv.release();
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
            fenv: FilterEnv::default(),
            cutoff: CUTOFF_MAX,
            resonance: 0.0,
            fenv_amount: 0.0,
            vel: 1.0,
            vel_octaves: 0.0,
            retune_in: 0,
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
    /// Drives [`render`](Self::render) when the Sampler runs on its own (an
    /// offline render, a test), reading `SamplerShared`'s clock. In the
    /// engine the graph's transport drives [`process`](Self::process) instead.
    own_clock: Transport,
    /// The transport's beats per frame and frames per beat, for this piece of the block.
    bpf: f64,
    fpb: f64,
    /// A boundary this close (in beats) is reached.
    eps: f64,
    was_playing: bool,
    /// A song that doesn't loop has ended and stopped the transport; the
    /// sequencer stays stopped until the transport has seen it.
    ended: bool,
    next_step: usize,
    /// Beats left in the step that fired last. At a block boundary this says
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
    /// The dice for step chance.
    rng: XorShift,
    /// Where the sequencer is, as of the step that fired last.
    pos: Position,
    /// The rest of a ratcheted step, per pad: hits left, beats until the next
    /// one, beats between hits and their velocity. `ratchet_active` has a bit
    /// for each pad with hits left.
    ratchet_left: [u8; PADS],
    ratchet_wait: [f64; PADS],
    ratchet_gap: [f64; PADS],
    ratchet_vel: [f32; PADS],
    ratchet_active: u32,
}

/// Which bank a step reads: the chain entry playing (NO_POS outside a song),
/// its bank, and the bank that plays outside a song.
#[derive(Clone, Copy)]
struct Position {
    chain_pos: u32,
    song_bank: usize,
    plain_bank: usize,
}

impl Position {
    fn bank(&self) -> usize {
        if self.chain_pos != NO_POS { self.song_bank } else { self.plain_bank }
    }
}

/// What starts at the top of the pattern.
enum Pass {
    /// The current bank, as outside song mode.
    Plain,
    /// Chain entry `.0`, playing bank `.1`.
    Song(u32, usize),
    /// A song that doesn't loop has played its last entry.
    End,
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

/// The Sampler as a track's source: the pads and the sequencer, on the
/// engine's transport.
pub struct SamplerDevice {
    sampler: Sampler,
    shared: Arc<SamplerShared>,
}

impl Device for SamplerDevice {
    /// Restarts the Sampler at the new rate. Its samples were at the old one,
    /// so they are dropped: the engine sends them again.
    fn prepare(&mut self, sample_rate: u32, _max_frames: usize) {
        self.sampler.set_rate(sample_rate);
        self.sampler.sync_counters(&self.shared);
    }

    fn process(&mut self, ctx: &ProcessContext, _events: &mut Events, audio: Buffers<'_>) {
        self.sampler.process(ctx, &self.shared, audio.left, audio.right);
    }
}

/// The Sampler device for `shared`, and the control side's link to its sample queues.
pub fn device(shared: Arc<SamplerShared>, sample_rate: u32) -> (SamplerDevice, SamplerLink) {
    let (mut sampler, link) = sampler(sample_rate);
    sampler.sync_counters(&shared);
    (SamplerDevice { sampler, shared }, link)
}

/// A sampler for a stream at `sample_rate`, and the control side's link to it.
pub fn sampler(sample_rate: u32) -> (Sampler, SamplerLink) {
    let (send, incoming) = rtrb::RingBuffer::new(32);
    let (garbage, returned) = rtrb::RingBuffer::new(64);
    let mut s = Sampler::offline(sample_rate);
    s.incoming = Some(incoming);
    s.garbage = Some(garbage);
    // Live dice differ from one run to the next; only offline renders are repeatable.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    s.rng = XorShift::new(nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15));
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
            own_clock: Transport::new(sample_rate),
            bpf: 0.0,
            fpb: 0.0,
            eps: 0.0,
            was_playing: false,
            ended: false,
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
            rng: XorShift::new(RNG_SEED),
            pos: Position { chain_pos: NO_POS, song_bank: 0, plain_bank: 0 },
            ratchet_left: [0; PADS],
            ratchet_wait: [0.0; PADS],
            ratchet_gap: [0.0; PADS],
            ratchet_vel: [1.0; PADS],
            ratchet_active: 0,
        }
    }

    /// Makes this sampler run at `sample_rate`. Voices stop and every sample is
    /// dropped (they were at the old rate). Not for the audio thread.
    pub fn set_rate(&mut self, sample_rate: u32) {
        let mut fresh = Sampler::offline(sample_rate);
        fresh.incoming = self.incoming.take();
        fresh.garbage = self.garbage.take();
        fresh.rng = self.rng.clone();
        fresh.hits = self.hits;
        fresh.cuts = self.cuts;
        if let Some(q) = fresh.incoming.as_mut() {
            while q.pop().is_ok() {} // sent for the old rate
        }
        *self = fresh;
    }

    /// Counts the hits and cuts already made as seen, so a new sampler does
    /// not play them again.
    pub fn sync_counters(&mut self, shared: &SamplerShared) {
        for slot in 0..SLOTS {
            self.hits[slot] = shared.pads[slot].hits.load(Ordering::Acquire);
            self.cuts[slot] = shared.pads[slot].cuts.load(Ordering::Acquire);
        }
    }

    /// Puts a sample (at this sampler's rate) in a slot directly. Not for the audio thread.
    pub fn set_sample(&mut self, slot: usize, data: Option<Track>) {
        if slot < SLOTS {
            self.samples[slot] = data;
        }
    }

    /// Runs the Sampler on its own, on `shared`'s clock: takes in new
    /// samples and the UI's hits and cuts, then adds the pads and the
    /// sequencer into `out` (interleaved, `channels` wide). For offline
    /// renders and tests; in the engine the graph calls
    /// [`process`](Self::process).
    pub fn render(&mut self, shared: &SamplerShared, out: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        let frames = out.len() / channels;
        let mut clock = self.own_clock;
        clock.run(&shared.clock, frames, |ctx, offset| {
            let piece = &mut out[offset * channels..(offset + ctx.frames) * channels];
            self.run(ctx, shared, |f, left, right, click| {
                let o = &mut piece[f * channels..(f + 1) * channels];
                if channels == 1 {
                    o[0] += 0.5 * (left + right) + click;
                } else {
                    o[0] += left + click;
                    o[1] += right + click;
                }
            });
        });
        self.own_clock = clock;
    }

    /// One piece of a block on the engine's transport: adds the pads and the
    /// sequencer into `left` and `right`. Real-time safe: no allocation,
    /// locks, logging or I/O.
    pub fn process(&mut self, ctx: &ProcessContext, shared: &SamplerShared, left: &mut [f32], right: &mut [f32]) {
        let n = ctx.frames.min(left.len()).min(right.len());
        let ctx = ProcessContext { frames: n, ..*ctx };
        self.run(&ctx, shared, |f, l, r, click| {
            left[f] += l + click;
            right[f] += r + click;
        });
    }

    /// The Sampler's work for `ctx.frames` frames: `out(frame, left, right,
    /// click)` takes each frame's pads and metronome click.
    fn run(&mut self, ctx: &ProcessContext, shared: &SamplerShared, mut out: impl FnMut(usize, f32, f32, f32)) {
        self.take_samples();
        self.bpf = ctx.beats_per_frame;
        self.fpb = ctx.frames_per_beat();
        self.eps = ctx.beats_per_frame * EDGE_FRAMES;

        if !ctx.playing || ctx.started {
            self.ended = false;
        }
        let mut playing = ctx.playing && !self.ended;
        let steps = shared.steps().max(1);
        if playing && (!self.was_playing || ctx.started) {
            self.locate(shared, ctx.beat, steps, true);
        } else if playing && ctx.jumped {
            self.locate(shared, ctx.beat, steps, false);
        } else if !playing && self.was_playing {
            self.stop_transport(shared);
            // Stopping silences the pads, as any transport stop does, instead of
            // letting each sample play to its end.
            for slot in 0..PADS {
                self.release_slot(slot);
            }
        }
        self.was_playing = playing;
        // The pattern got shorter than where the sequencer is: carry on at the
        // same place in the bar, so the wrap (and any switch waiting for it)
        // still lands on a multiple of the new length.
        self.next_step %= steps;
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

        for frame in 0..ctx.frames {
            if playing {
                if self.countdown <= self.eps && !self.fire_step(shared, steps) {
                    // A song that doesn't loop reached its end, on this frame:
                    // stop the transport, and stay stopped until it has.
                    shared.clock.set_playing(false);
                    self.ended = true;
                    self.stop_transport(shared);
                    self.was_playing = false;
                    playing = false;
                }
                if playing {
                    self.countdown -= self.bpf;
                    if self.ratchet_active != 0 {
                        self.tick_ratchets(shared);
                    }
                }
            }
            let (left, right) = self.mix_frame(shared);
            let click = self.click_frame();
            out(frame, left, right, click);
        }
    }

    /// Puts the sequencer where `beat` falls in the pattern, laid out on the
    /// beat grid from beat 0: on a step's first frame that step fires now;
    /// inside a step it waits for the next one. `fresh` starts a song from
    /// its first entry; otherwise (a loop wrap, a seek) the song carries on.
    fn locate(&mut self, shared: &SamplerShared, beat: f64, steps: usize, fresh: bool) {
        if fresh {
            self.pos.chain_pos = NO_POS;
        }
        self.ratchet_active = 0;
        self.skip_pads = 0;
        let pass: f64 = (0..steps).map(|s| shared.step_beats(s)).sum();
        let mut at = beat.rem_euclid(pass);
        if pass - at <= self.eps {
            at = 0.0;
        }
        let mut start = 0.0;
        for s in 0..steps {
            let len = shared.step_beats(s);
            if at < start + len - self.eps {
                if at - start <= self.eps {
                    self.next_step = s;
                    self.countdown = start - at; // at most `eps`: fires on this frame
                } else {
                    self.next_step = (s + 1) % steps;
                    self.countdown = start + len - at;
                    shared.step.store(s as u32, Ordering::Relaxed);
                }
                return;
            }
            start += len;
        }
        self.next_step = 0;
        self.countdown = 0.0;
    }

    fn take_samples(&mut self) {
        if let Some(b) = self.held_back.take() {
            self.give_back(b);
        }
        // While a box waits to go back, leave new samples queued: taking one
        // could need a second place to hold its old box.
        while self.held_back.is_none() {
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

    /// Stops the sequencer's own state: the step and song position, the
    /// metronome click, and any hits still to come.
    fn stop_transport(&mut self, shared: &SamplerShared) {
        shared.step.store(NO_STEP, Ordering::Relaxed);
        shared.chain_pos.store(NO_POS, Ordering::Relaxed);
        shared.play_bank.store(NO_BANK, Ordering::Relaxed);
        self.pos.chain_pos = NO_POS;
        self.click_left = 0;
        self.skip_pads = 0;
        self.ratchet_active = 0;
    }

    /// What the next pass of the pattern plays. At the top of the pattern, in
    /// song mode, that is the next chain entry; otherwise the current bank.
    fn next_pass(&self, shared: &SamplerShared) -> Pass {
        let len = (shared.chain_len.load(Ordering::Acquire) as usize).min(CHAIN_MAX);
        if !shared.song_mode.load(Ordering::Relaxed) || len == 0 {
            return Pass::Plain;
        }
        let mut pos = if self.pos.chain_pos == NO_POS { 0 } else { self.pos.chain_pos as usize + 1 };
        if pos >= len {
            if !shared.song_loop.load(Ordering::Relaxed) {
                return Pass::End;
            }
            pos = 0;
        }
        let bank = shared.chain.get(pos).map_or(0, |a| a.load(Ordering::Relaxed) as usize).min(BANKS - 1);
        Pass::Song(pos as u32, bank)
    }

    /// Where the sequencer will be once `step` fires, or `None` when a song
    /// that doesn't loop ends there. This is where every switch lands: at the
    /// wrap (step 0) the next pass starts, from the current bank or the next
    /// chain entry. Switching now, every step also picks up the current bank,
    /// and song mode turned off. Otherwise a pass plays to its end, even a
    /// song's pass after song mode is turned off. A song only starts at the
    /// wrap, so its first entry is a whole pass. Changes nothing.
    fn position_at(&self, shared: &SamplerShared, step: usize) -> Option<Position> {
        let now = shared.switch_now.load(Ordering::Relaxed);
        let mut p = self.pos;
        if step == 0 || now {
            p.plain_bank = shared.bank();
        }
        if step == 0 {
            match self.next_pass(shared) {
                Pass::Plain => p.chain_pos = NO_POS,
                Pass::Song(pos, bank) => (p.chain_pos, p.song_bank) = (pos, bank),
                Pass::End => return None,
            }
        } else if now && !shared.song_mode.load(Ordering::Relaxed) {
            p.chain_pos = NO_POS;
        }
        Some(p)
    }

    /// Plays one hit of a step. `frames` is how long a gate-mode pad holds it.
    fn step_hit(&mut self, shared: &SamplerShared, pad: usize, frames: f64, vel: f32) {
        self.start(shared, pad, Hold::Frames(frames), vel);
    }

    /// Fires the step that is due. Returns false, firing nothing, when a song
    /// that doesn't loop has played its last entry: the caller stops.
    fn fire_step(&mut self, shared: &SamplerShared, steps: usize) -> bool {
        let step = self.next_step;
        let Some(p) = self.position_at(shared, step) else { return false };
        self.pos = p;
        let playing = p.bank();
        shared.chain_pos.store(p.chain_pos, Ordering::Relaxed);
        shared.play_bank.store(playing as u32, Ordering::Relaxed);
        let bank = &shared.banks[playing];
        let length = shared.step_beats(step);
        // Pads whose hit was recorded onto this step a moment ago already sounded.
        let skip = if step == self.skip_step { self.skip_pads } else { 0 };
        self.skip_pads = 0;
        for pad in 0..PADS {
            let c = &shared.pads[pad];
            let on = bank.patterns[pad].load(Ordering::Relaxed) & (1u64 << step) != 0;
            if on && c.flags.load(Ordering::Relaxed) & MUTE == 0 && skip & (1 << pad) == 0 {
                let chance = bank.chances[pad][step].load(Ordering::Relaxed);
                if chance == VEL_MAX || (chance > 0 && self.rng.roll(chance)) {
                    let vel = bank.velocities[pad][step].load(Ordering::Relaxed) as f32 / VEL_MAX as f32;
                    let n = bank.ratchets[pad][step].load(Ordering::Relaxed).clamp(1, RATCHET_MAX);
                    let gap = length / f64::from(n);
                    self.step_hit(shared, pad, gap * self.fpb, vel);
                    self.ratchet_active &= !(1 << pad);
                    if n > 1 {
                        // The rest land at multiples of `gap` from the step's exact
                        // start, which `countdown` (zero or a bit under) marks.
                        self.ratchet_left[pad] = n - 1;
                        self.ratchet_wait[pad] = self.countdown + gap;
                        self.ratchet_gap[pad] = gap;
                        self.ratchet_vel[pad] = vel;
                        self.ratchet_active |= 1 << pad;
                    }
                }
            }
        }
        if step.is_multiple_of(STEPS_PER_BEAT) && shared.metronome.load(Ordering::Relaxed) {
            self.start_click(step == 0);
        }
        shared.step.store(step as u32, Ordering::Relaxed);
        self.countdown += length;
        self.next_step = (step + 1) % steps;
        true
    }

    /// Plays the ratchet hits that are due on this frame. They count down in
    /// beats like `countdown` does, so they are as exact as the steps.
    fn tick_ratchets(&mut self, shared: &SamplerShared) {
        let mut active = self.ratchet_active;
        while active != 0 {
            let pad = active.trailing_zeros() as usize;
            active &= active - 1;
            if self.ratchet_wait[pad] <= self.eps {
                if shared.pads[pad].flags.load(Ordering::Relaxed) & MUTE == 0 {
                    self.step_hit(shared, pad, self.ratchet_gap[pad] * self.fpb, self.ratchet_vel[pad]);
                }
                self.ratchet_left[pad] -= 1;
                if self.ratchet_left[pad] == 0 {
                    self.ratchet_active &= !(1 << pad);
                    continue;
                }
                self.ratchet_wait[pad] += self.ratchet_gap[pad];
            }
            self.ratchet_wait[pad] -= self.bpf;
        }
    }

    /// Writes a hand-played hit on `pad` into the nearest step. Where the
    /// sequencer is comes from `countdown`, the beats left in the step that
    /// fired last: in its first half the hit goes on that step, in its second
    /// half on the next one, which the sequencer then skips for this pad once
    /// (if it will play the bank written to there).
    ///
    /// Outside a song the hit goes into the bank heard on that step, so while
    /// a switch waits for the wrap it lands in the pattern still playing. In a
    /// song it goes into the bank being edited, as the chain decides what plays.
    fn record_hit(&mut self, shared: &SamplerShared, pad: usize, steps: usize, vel: f32) {
        let current = (self.next_step + steps - 1) % steps;
        let length = shared.step_beats(current);
        let ahead = self.countdown <= length * 0.5;
        let (step, at) = if ahead {
            (self.next_step, self.position_at(shared, self.next_step))
        } else {
            (current, Some(self.pos))
        };
        let target = match at {
            Some(p) if p.chain_pos == NO_POS => p.plain_bank,
            _ => shared.bank(),
        };
        if ahead && at.map(|p| p.bank()) == Some(target) {
            self.skip_step = step;
            self.skip_pads |= 1 << pad;
        }
        shared.banks[target].patterns[pad].fetch_or(1u64 << step, Ordering::Relaxed);
        shared.set_bank_velocity(target, pad, step, vel);
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
            v.cutoff = f(&c.cutoff);
            v.resonance = f(&c.resonance);
            v.fenv_amount = f(&c.fenv_amount);
            v.vel_octaves = f(&c.vel_cutoff) * (v.vel - 1.0);
            v.tune_filter(mode, self.rate);
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
        // Velocity to envelope time: soft hits shorter (or longer), full hits as set.
        let time = VEL_TIME_SPAN.powf(f64::from(p.vel_time) * f64::from(vel - 1.0));
        let attack_secs = ATTACK_SECONDS.max(f64::from(p.attack) * time);
        let release_secs = f64::from(p.release).max(MIN_RELEASE_SECONDS);
        // The default (0.004 as an f32) is the 4 ms fade exactly, as before.
        let release_secs = if (release_secs - RELEASE_SECONDS).abs() < 1e-6 { RELEASE_SECONDS } else { release_secs };
        let dec_coef = if p.decay > 0.0 {
            // Exponential, `decay` seconds to fall 60 dB of the way to sustain.
            (0.001f64.powf(1.0 / (rate * f64::from(p.decay) * time)) as f32).max(1.0e-20)
        } else {
            0.0
        };
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
            filter: VoiceFilter::default(),
            fenv: FilterEnv::new(&p, rate),
            cutoff: p.cutoff,
            resonance: p.resonance,
            fenv_amount: p.fenv_amount,
            vel,
            vel_octaves: p.vel_cutoff * (vel - 1.0),
            retune_in: 0,
        };
        self.voices[i].tune_filter(p.filter, rate);
        shared.pads[slot].played.fetch_add(1, Ordering::Relaxed);
    }

    fn release_slot(&mut self, slot: usize) {
        let fast = self.release;
        for v in self.voices.iter_mut().filter(|v| v.active && v.slot == slot) {
            v.release(Some(fast));
        }
    }

    fn mix_frame(&mut self, shared: &SamplerShared) -> (f32, f32) {
        let rate = self.rate;
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
            v.fenv.tick();
            if v.filter.mode != FILTER_OFF && v.fenv_amount != 0.0 {
                // The envelope moves the cutoff: retune every few frames, not every one.
                if v.retune_in == 0 {
                    v.retune_in = FENV_TICK;
                    v.tune_filter(v.filter.mode, rate);
                }
                v.retune_in -= 1;
            }
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
///
/// It plays the current bank, over and over, even in song mode; chances use an
/// offline sampler's fixed seed, so the same settings render the same audio.
pub fn render_pattern(samples: &[Option<Track>], shared: &SamplerShared, rate: u32, loops: usize) -> Vec<Vec<f32>> {
    let local = private_copy(shared);
    render_offline(samples, &local, rate, shared.pattern_frames(rate, loops))
}

/// Renders the song once, from the first chain entry to the end, to stereo,
/// offline and sample-exact: one pass of the pattern per entry, so
/// [`SamplerShared::song_frames`] frames. An empty chain renders nothing.
/// `shared` is read as [`render_pattern`] reads it, and the song loop is ignored.
pub fn render_song(samples: &[Option<Track>], shared: &SamplerShared, rate: u32) -> Vec<Vec<f32>> {
    let local = private_copy(shared);
    local.set_chain(&shared.chain());
    local.set_song_mode(true);
    local.set_song_loop(false);
    render_offline(samples, &local, rate, shared.song_frames(rate))
}

/// A private copy of the pads, every bank and the tempo, with the transport
/// stopped, so rendering never starts the live sampler.
fn private_copy(shared: &SamplerShared) -> SamplerShared {
    let local = SamplerShared::default();
    for pad in 0..PADS {
        local.set_pad(pad, &shared.pad(pad));
    }
    for (to, from) in local.banks.iter().zip(&shared.banks) {
        to.copy_from(from);
    }
    local.set_bank(shared.bank());
    local.set_tempo(shared.tempo());
    local.set_swing(shared.swing());
    local.set_steps(shared.steps());
    local
}

/// Plays `local` from the top for `frames` frames, with `samples` in the pads.
fn render_offline(samples: &[Option<Track>], local: &SamplerShared, rate: u32, frames: usize) -> Vec<Vec<f32>> {
    let mut sampler = Sampler::offline(rate);
    for (slot, s) in samples.iter().enumerate().take(PADS) {
        sampler.set_sample(slot, s.clone());
    }
    local.set_playing(true);

    let mut out = vec![Vec::with_capacity(frames), Vec::with_capacity(frames)];
    let mut block = vec![0.0f32; 1024 * 2];
    let mut done = 0;
    while done < frames {
        let n = (frames - done).min(1024);
        let buf = &mut block[..n * 2];
        buf.fill(0.0);
        sampler.render(local, buf, 2);
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
    fn stopping_cuts_ringing_pads_but_not_the_preview() {
        let shared = SamplerShared::default();
        let (mut s, mut link) = sampler(RATE);
        link.send(0, Some(dc(RATE as usize * 4, 0.5))).unwrap();
        link.send(PREVIEW, Some(dc(RATE as usize * 4, 0.25))).unwrap();
        shared.set_pattern(0, 1);
        shared.set_playing(true);
        run(&mut s, &shared, 2_000); // step 1 starts the long pad sample
        shared.trigger(PREVIEW);
        let out = run(&mut s, &shared, 1_000);
        assert!((out[1998] - 0.75).abs() < 1e-3, "pad and preview both sound: {}", out[1998]);
        shared.set_playing(false);
        let out = run(&mut s, &shared, 1_000);
        assert!((out[1998] - 0.25).abs() < 1e-3, "the pad stopped within the fade, the preview plays on: {}", out[1998]);
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

    /// The left channel of one hit at `vel`, divided by `vel` so hits compare by tone.
    fn hit(p: &PadParams, sample: Track, vel: f32, frames: usize) -> Vec<f32> {
        let shared = SamplerShared::default();
        shared.set_pad(0, p);
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(sample));
        shared.trigger_velocity(0, vel);
        run(&mut s, &shared, frames).chunks(2).map(|f| f[0] / vel).collect()
    }

    #[test]
    fn the_filter_envelope_sweeps_the_cutoff_down() {
        // 200 Hz, opened 6 octaves (12.8 kHz) on the hit, closing over half a second.
        let p = PadParams { fenv_amount: 6.0, fenv_attack: 0.0, fenv_decay: 0.5, fenv_sustain: 0.0, ..lp(200.0, 0.0) };
        let out = play(&p, sine(4_000.0, 96_000), 48_000);
        let early = rms(&out[100..300]);
        let late = rms(&out[24_000..]);
        assert!(early > 0.4, "open on the hit: {early}");
        assert!(late < 0.01, "closed after the decay: {late}");
        let flat = play(&lp(200.0, 0.0), sine(4_000.0, 96_000), 48_000);
        assert!(rms(&flat[100..300]) < 0.1, "with no amount it stays closed");
    }

    #[test]
    fn a_negative_amount_closes_the_filter_on_the_hit() {
        // 8 kHz pulled down 6 octaves, to 125 Hz, and held there (no decay).
        let p = PadParams { fenv_amount: -6.0, fenv_decay: 0.0, ..lp(8_000.0, 0.0) };
        let out = play(&p, sine(2_000.0, 48_000), 24_000);
        assert!(rms(&out[4_000..]) < 0.02, "{}", rms(&out[4_000..]));
        assert!(through(&lp(8_000.0, 0.0), 2_000.0) > 0.6);
    }

    #[test]
    fn the_filter_envelope_attack_opens_the_filter() {
        let p = PadParams { fenv_amount: 7.0, fenv_attack: 0.5, fenv_decay: 0.0, ..lp(100.0, 0.0) };
        let out = play(&p, sine(4_000.0, 96_000), 48_000);
        assert!(rms(&out[1_000..3_000]) < 0.05, "still closed: {}", rms(&out[1_000..3_000]));
        assert!(rms(&out[30_000..]) > 0.6, "open after the attack: {}", rms(&out[30_000..]));
    }

    #[test]
    fn the_filter_envelope_releases_with_the_pad() {
        // Held open; once let go the cutoff falls back in 50 ms while the long amp release still sounds.
        let shared = SamplerShared::default();
        let p = PadParams { gate: true, release: 2.0, fenv_amount: 6.0, fenv_decay: 0.0, fenv_release: 0.05, ..lp(200.0, 0.0) };
        shared.set_pad(0, &p);
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(sine(4_000.0, 96_000)));
        shared.trigger(0);
        let held: Vec<f32> = run(&mut s, &shared, 4_800).chunks(2).map(|f| f[0]).collect();
        shared.release(0);
        let after: Vec<f32> = run(&mut s, &shared, 9_600).chunks(2).map(|f| f[0]).collect();
        assert!(rms(&held[1_000..]) > 0.6, "open while held: {}", rms(&held[1_000..]));
        assert!(rms(&after[3_600..]) < 0.01, "closed after the release: {}", rms(&after[3_600..]));
    }

    #[test]
    fn the_filter_envelope_needs_the_filter_on() {
        let p = PadParams { fenv_amount: 6.0, vel_cutoff: 6.0, ..Default::default() };
        assert_eq!(play(&p, sine(4_000.0, 9_600), 4_800), play(&PadParams::default(), sine(4_000.0, 9_600), 4_800));
    }

    #[test]
    fn velocity_to_cutoff_makes_a_soft_hit_darker() {
        // 8 kHz; a quarter-velocity hit is 4.5 octaves lower, about 350 Hz.
        let p = PadParams { vel_cutoff: 6.0, ..lp(8_000.0, 0.0) };
        let hard = hit(&p, sine(2_000.0, 48_000), 1.0, 24_000);
        let soft = hit(&p, sine(2_000.0, 48_000), 0.25, 24_000);
        assert!(rms(&hard[8_000..]) > 0.6, "a full hit sounds as set: {}", rms(&hard[8_000..]));
        assert!(rms(&soft[8_000..]) < 0.1, "a soft hit is darker: {}", rms(&soft[8_000..]));
        let plain = hit(&lp(8_000.0, 0.0), sine(2_000.0, 48_000), 0.25, 24_000);
        assert!((rms(&plain[8_000..]) - rms(&hard[8_000..])).abs() < 0.01, "with no amount velocity is only level");
    }

    #[test]
    fn velocity_to_time_shortens_or_lengthens_a_soft_hit() {
        let p = PadParams { decay: 0.2, sustain: 0.0, vel_time: 1.0, ..Default::default() };
        let hard = hit(&p, dc(48_000, 0.5), 1.0, 9_600);
        let soft = hit(&p, dc(48_000, 0.5), 0.25, 9_600);
        // 0.1 s in, a full hit has fallen 30 dB; a soft one, with a decay about a third as long, far more.
        assert!((hard[4_800] - 0.5 * 0.001f32.powf(0.5)).abs() < 0.002, "a full hit is as set: {}", hard[4_800]);
        assert!(soft[4_800] < hard[4_800] * 0.05, "shorter: {} vs {}", soft[4_800], hard[4_800]);
        let long = hit(&PadParams { vel_time: -1.0, ..p }, dc(48_000, 0.5), 0.25, 9_600);
        assert!(long[4_800] > hard[4_800] * 5.0, "longer: {} vs {}", long[4_800], hard[4_800]);
    }

    #[test]
    fn filter_envelope_and_velocity_settings_stay_in_range() {
        let shared = SamplerShared::default();
        shared.set_pad(0, &PadParams {
            fenv_amount: 20.0,
            fenv_attack: -1.0,
            fenv_decay: 9.0,
            fenv_sustain: 2.0,
            fenv_release: f32::NAN,
            vel_cutoff: -20.0,
            vel_time: 3.0,
            ..Default::default()
        });
        let p = shared.pad(0);
        assert_eq!(
            (p.fenv_amount, p.fenv_attack, p.fenv_decay, p.fenv_sustain, p.fenv_release, p.vel_cutoff, p.vel_time),
            (8.0, 0.0, 5.0, 1.0, 0.1, -8.0, 1.0)
        );
        let d = PadParams::default();
        assert_eq!((d.fenv_amount, d.vel_cutoff, d.vel_time), (0.0, 0.0, 0.0), "off by default");
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

    // Banks, song mode, chance and ratchets.

    /// Plays `frames` frames in blocks of `block` (an odd size shows that nothing
    /// depends on where a block ends) and returns the left channel.
    fn play_blocks(s: &mut Sampler, shared: &SamplerShared, frames: usize, block: usize) -> Vec<f32> {
        let mut left = Vec::with_capacity(frames);
        let mut done = 0;
        while done < frames {
            let n = block.min(frames - done);
            left.extend(run(s, shared, n).chunks(2).map(|f| f[0]));
            done += n;
        }
        left
    }

    /// A mono sample at a flat level.
    fn flat(frames: usize, v: f32) -> Track {
        Arc::new(vec![vec![v; frames]])
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    /// Which test pad sounds in the 100 frames from `at`: 0 is the click (peaks
    /// near 0.42), 1 is the flat 0.8 one.
    fn pad_at(left: &[f32], at: usize) -> Option<usize> {
        match peak(&left[at..at + 100]) {
            p if p < 0.01 => None,
            p if p < 0.6 => Some(0),
            _ => Some(1),
        }
    }

    /// 4 steps at 120 BPM, so a pass is 24,000 frames. Pad 0 is a click and
    /// pad 1 a flat 0.8, both 100 frames long.
    fn song_rig() -> (Sampler, SamplerShared) {
        let shared = SamplerShared::default();
        shared.set_tempo(120.0);
        shared.set_steps(4);
        let mut s = Sampler::offline(RATE);
        s.set_sample(0, Some(click(100)));
        s.set_sample(1, Some(flat(100, 0.8)));
        (s, shared)
    }

    /// Bank 0 plays pad 0 on step 0, bank 1 plays pad 1 on step 0.
    fn a_and_b(shared: &SamplerShared) {
        shared.set_bank_pattern(0, 0, 0b0001);
        shared.set_bank_pattern(1, 1, 0b0001);
    }

    #[test]
    fn the_bank_functions_and_their_aliases() {
        let shared = SamplerShared::default();
        assert_eq!(shared.bank(), 0);
        shared.set_pattern(0, 0b101);
        shared.set_velocity(0, 1, 0.5);
        assert_eq!(shared.bank_pattern(0, 0), 0b101, "set_pattern writes the current bank");
        assert_eq!(shared.bank_pattern(1, 0), 0);
        assert!((shared.bank_velocity(0, 0, 1) - 0.5).abs() < 0.003);

        shared.set_bank(2);
        assert_eq!(shared.bank(), 2);
        assert_eq!(shared.pattern(0), 0, "the alias reads the current bank");
        shared.set_pattern(0, 0b10);
        shared.set_velocity(0, 1, 0.25);
        assert_eq!(shared.bank_pattern(2, 0), 0b10);
        assert_eq!(shared.bank_pattern(0, 0), 0b101, "bank 0 is untouched");
        assert!((shared.velocity(0, 1) - 0.25).abs() < 0.003);
        assert!((shared.bank_velocity(0, 0, 1) - 0.5).abs() < 0.003);

        shared.set_bank_pattern(3, 5, 7);
        shared.set_bank_velocity(3, 5, 2, 0.0);
        assert_eq!(shared.bank_pattern(3, 5), 7);
        assert_eq!(shared.bank_velocity(3, 5, 2), 0.0);

        // Bad indexes are ignored and read as the defaults.
        shared.set_bank(BANKS);
        assert_eq!(shared.bank(), 2);
        shared.set_bank_pattern(BANKS, 0, 1);
        shared.set_bank_pattern(0, PADS, 1);
        shared.set_bank_velocity(BANKS, 0, 0, 0.0);
        shared.set_bank_velocity(0, 0, MAX_STEPS, 0.0);
        assert_eq!(shared.bank_pattern(BANKS, 0), 0);
        assert_eq!(shared.bank_pattern(0, PADS), 0);
        assert_eq!(shared.bank_velocity(BANKS, 0, 0), 1.0);
        assert_eq!(shared.bank_velocity(0, 0, MAX_STEPS), 1.0);
        assert_eq!(shared.bank_velocity(0, 0, 0), 1.0);
    }

    #[test]
    fn chance_and_ratchet_have_defaults_ranges_and_bad_indexes() {
        let shared = SamplerShared::default();
        assert_eq!(shared.chance(0, 0, 0), 1.0);
        assert_eq!(shared.ratchet(0, 0, 0), 1);
        shared.set_chance(1, 2, 3, 0.5);
        shared.set_ratchet(1, 2, 3, 3);
        assert!((shared.chance(1, 2, 3) - 0.5).abs() < 0.003);
        assert_eq!(shared.ratchet(1, 2, 3), 3);
        assert_eq!(shared.chance(0, 2, 3), 1.0, "banks are separate");
        assert_eq!(shared.ratchet(0, 2, 3), 1);

        shared.set_chance(0, 0, 0, 7.0);
        assert_eq!(shared.chance(0, 0, 0), 1.0, "clamped");
        shared.set_chance(0, 0, 0, -1.0);
        assert_eq!(shared.chance(0, 0, 0), 0.0, "clamped");
        shared.set_chance(0, 0, 0, f32::NAN);
        assert_eq!(shared.chance(0, 0, 0), 1.0, "NaN reads as the default");
        shared.set_ratchet(0, 0, 0, 0);
        assert_eq!(shared.ratchet(0, 0, 0), 1, "clamped up to one hit");
        shared.set_ratchet(0, 0, 0, 9);
        assert_eq!(shared.ratchet(0, 0, 0), 4, "clamped down to four");

        shared.set_chance(BANKS, 0, 0, 0.0);
        shared.set_chance(0, PADS, 0, 0.0);
        shared.set_chance(0, 0, MAX_STEPS, 0.0);
        shared.set_ratchet(BANKS, 0, 0, 2);
        shared.set_ratchet(0, PADS, 0, 2);
        shared.set_ratchet(0, 0, MAX_STEPS, 2);
        assert_eq!(shared.chance(BANKS, 0, 0), 1.0);
        assert_eq!(shared.chance(0, PADS, 0), 1.0);
        assert_eq!(shared.chance(0, 0, MAX_STEPS), 1.0);
        assert_eq!(shared.ratchet(BANKS, 0, 0), 1);
        assert_eq!(shared.ratchet(0, PADS, 0), 1);
        assert_eq!(shared.ratchet(0, 0, MAX_STEPS), 1);
    }

    #[test]
    fn a_bank_switch_waits_for_the_wrap_and_lands_on_its_frame() {
        let (mut s, shared) = song_rig();
        shared.set_bank_pattern(0, 0, 0b1111); // A: the click, every step
        shared.set_bank_pattern(1, 1, 0b1111); // B: the flat pad, every step
        assert!(!shared.switch_now(), "switches wait for the wrap by default");
        assert_eq!(shared.playing_bank(), None, "nothing plays yet");
        shared.set_playing(true);
        let mut left = play_blocks(&mut s, &shared, 10_000, 777); // inside step 1
        assert_eq!(shared.playing_bank(), Some(0));
        shared.set_bank(1);
        assert_eq!(shared.bank(), 1, "the edit bank changes at once");
        assert_eq!(shared.playing_bank(), Some(0), "A plays on, B is queued");

        left.extend(play_blocks(&mut s, &shared, 14_000, 777)); // frames up to 23,999
        assert_eq!(shared.playing_bank(), Some(0), "the last frame of A's pass");
        assert_eq!(shared.current_step(), Some(3));
        left.extend(play_blocks(&mut s, &shared, 1, 1));
        assert_eq!(shared.playing_bank(), Some(1), "frame 24,000 starts B");
        assert_eq!(shared.current_step(), Some(0));
        left.extend(play_blocks(&mut s, &shared, 24_000 - 1, 777));

        assert_eq!(onsets(&left), vec![0, 6_000, 12_000, 18_000, 24_000, 30_000, 36_000, 42_000]);
        for at in [0, 6_000, 12_000, 18_000] {
            assert_eq!(pad_at(&left, at), Some(0), "A's step at {at}, after the switch was made");
        }
        assert!(left[23_999] == 0.0 && left[24_000] > 0.01, "B sounds from the wrap's very first frame");
        for at in [24_000, 30_000, 36_000, 42_000] {
            assert_eq!(pad_at(&left, at), Some(1), "B's step at {at}");
        }

        shared.set_playing(false);
        play_blocks(&mut s, &shared, 1, 1);
        assert_eq!(shared.playing_bank(), None, "stopped");
    }

    #[test]
    fn the_wrap_is_exact_with_swing_and_an_odd_length() {
        let (mut s, shared) = song_rig();
        shared.set_steps(7);
        shared.set_swing(0.5);
        shared.set_tempo(133.0);
        shared.set_bank_pattern(0, 0, 0b111_1111);
        shared.set_bank_pattern(1, 1, 0b000_0001);
        shared.set_playing(true);
        let pass = shared.pattern_frames(RATE, 1);
        let mut left = play_blocks(&mut s, &shared, pass / 2, 333);
        shared.set_bank(1);
        left.extend(play_blocks(&mut s, &shared, pass, 333));
        let on = onsets(&left);
        assert_eq!(on.len(), 8, "A's seven steps, then B's one");
        assert_eq!(pad_at(&left, on[6]), Some(0), "A's last step");
        assert!((on[7] as i64 - pass as i64).abs() <= 1, "B starts on the wrap: {} vs {pass}", on[7]);
        assert_eq!(pad_at(&left, on[7]), Some(1));
    }

    #[test]
    fn a_queued_switch_can_be_changed_or_taken_back_before_the_wrap() {
        let (mut s, shared) = song_rig();
        shared.set_bank_pattern(0, 0, 0b0001);
        shared.set_bank_pattern(1, 1, 0b0001);
        shared.set_bank_pattern(2, 0, 0b0011);
        shared.set_playing(true);
        play_blocks(&mut s, &shared, 3_000, 500);
        shared.set_bank(1);
        shared.set_bank(2); // the last choice before the wrap wins
        let left = play_blocks(&mut s, &shared, 21_000 + 12_000, 500);
        assert_eq!(onsets(&left), vec![21_000, 27_000], "C's steps 0 and 1");
        assert_eq!(shared.playing_bank(), Some(2));

        play_blocks(&mut s, &shared, 15_000, 500); // 3,000 into C's second pass
        shared.set_bank(1);
        shared.set_bank(2); // back to the bank that plays: nothing waits
        let left = play_blocks(&mut s, &shared, 21_000 + 1_000, 500);
        assert_eq!(onsets(&left), vec![3_000, 21_000], "C's step 1, then its next pass");
        assert_eq!(pad_at(&left, 21_000), Some(0), "C again, not B");
    }

    #[test]
    fn switching_now_changes_what_plays_on_the_next_step() {
        let (mut s, shared) = song_rig();
        shared.set_switch_now(true);
        assert!(shared.switch_now());
        shared.set_bank_pattern(0, 0, 0b1111); // bank A: every step
        shared.set_bank_pattern(1, 0, 0b0100); // bank B: step 2 only
        shared.set_playing(true);
        let mut left = play_blocks(&mut s, &shared, 3_000, 500); // inside step 0
        shared.set_bank(1);
        left.extend(play_blocks(&mut s, &shared, 21_000, 500));
        // Step 0 already played from A; step 1 reads B (nothing); step 2 plays from B.
        assert_eq!(onsets(&left), vec![0, 12_000]);
        assert_eq!(shared.playing_bank(), Some(1));

        shared.set_bank(0);
        let left = play_blocks(&mut s, &shared, 24_000, 500);
        assert_eq!(onsets(&left), vec![0, 6_000, 12_000, 18_000], "and back to A");
    }

    #[test]
    fn switching_now_while_a_switch_waits_lands_it_on_the_next_step() {
        let (mut s, shared) = song_rig();
        shared.set_bank_pattern(0, 0, 0b1111);
        shared.set_bank_pattern(1, 1, 0b1111);
        shared.set_playing(true);
        play_blocks(&mut s, &shared, 3_000, 500); // inside step 0
        shared.set_bank(1);
        let left = play_blocks(&mut s, &shared, 6_000, 500); // step 1 at 6,000
        assert_eq!(pad_at(&left, 3_000), Some(0), "queued: step 1 is still A");
        shared.set_switch_now(true);
        let left = play_blocks(&mut s, &shared, 6_000, 500); // step 2 at 12,000
        assert_eq!(onsets(&left), vec![3_000]);
        assert_eq!(pad_at(&left, 3_000), Some(1), "step 2 is B");
        assert_eq!(shared.current_step(), Some(2));
    }

    #[test]
    fn song_mode_plays_a_b_a_and_switches_on_the_wrap_frame() {
        let (mut s, shared) = song_rig();
        a_and_b(&shared);
        shared.set_chain(&[0, 1, 0]);
        shared.set_song_mode(true);
        shared.set_playing(true);
        assert_eq!(shared.chain_position(), None, "not before it plays");

        let mut left = play_blocks(&mut s, &shared, 24_000, 777);
        assert_eq!(shared.chain_position(), Some(0), "frame 23,999 is still entry 0");
        assert_eq!(shared.current_step(), Some(3));
        left.extend(play_blocks(&mut s, &shared, 1, 1));
        assert_eq!(shared.chain_position(), Some(1), "frame 24,000 starts entry 1");
        assert_eq!(shared.current_step(), Some(0));
        left.extend(play_blocks(&mut s, &shared, 72_000 - 24_001, 777));

        assert_eq!(onsets(&left), vec![0, 24_000, 48_000]);
        assert_eq!(pad_at(&left, 0), Some(0), "A");
        assert_eq!(pad_at(&left, 24_000), Some(1), "B, from its very first frame");
        assert_eq!(pad_at(&left, 48_000), Some(0), "A again");
        assert!(left[23_000..24_000].iter().all(|&x| x == 0.0));
        assert_eq!(shared.chain_position(), Some(2));
    }

    #[test]
    fn a_song_stops_at_its_end_unless_it_loops() {
        let (mut s, shared) = song_rig();
        a_and_b(&shared);
        shared.set_chain(&[0, 1]);
        shared.set_song_mode(true);
        shared.set_playing(true);
        assert!(!shared.song_loop(), "off by default");

        let left = play_blocks(&mut s, &shared, 48_000, 777);
        assert!(shared.is_playing(), "the last frame of the last pass");
        assert_eq!(shared.chain_position(), Some(1));
        assert_eq!(onsets(&left), vec![0, 24_000]);
        play_blocks(&mut s, &shared, 1, 1);
        assert!(!shared.is_playing(), "stopped on the wrap");
        assert_eq!(shared.current_step(), None);
        assert_eq!(shared.chain_position(), None);
        let left = play_blocks(&mut s, &shared, 30_000, 777);
        assert!(left.iter().all(|&x| x == 0.0), "no further step fires");

        // Play starts the song again from entry 0.
        shared.set_playing(true);
        let left = play_blocks(&mut s, &shared, 24_001, 777);
        assert_eq!(onsets(&left), vec![0, 24_000]);
        assert_eq!(pad_at(&left, 0), Some(0));
        assert_eq!(shared.chain_position(), Some(1));

        // With the loop on it goes round and keeps playing.
        let (mut s, shared) = song_rig();
        a_and_b(&shared);
        shared.set_chain(&[0, 1]);
        shared.set_song_mode(true);
        shared.set_song_loop(true);
        assert!(shared.song_loop());
        shared.set_playing(true);
        let left = play_blocks(&mut s, &shared, 96_000, 777);
        assert_eq!(onsets(&left), vec![0, 24_000, 48_000, 72_000]);
        assert_eq!(pad_at(&left, 48_000), Some(0), "back to entry 0 on the wrap");
        assert_eq!(pad_at(&left, 72_000), Some(1));
        assert!(shared.is_playing());
        assert_eq!(shared.chain_position(), Some(1));
        play_blocks(&mut s, &shared, 1, 1);
        assert_eq!(shared.chain_position(), Some(0));
    }

    #[test]
    fn the_chain_is_stored_clamped_and_reported() {
        let shared = SamplerShared::default();
        assert!(shared.chain().is_empty());
        assert!(!shared.song_mode(), "off by default");
        shared.set_chain(&[0, 1, 2, 3, 4, 200, 0]);
        assert_eq!(shared.chain(), vec![0, 1, 2, 3, 0], "entries that are no bank are dropped");
        shared.set_chain(&[1; CHAIN_MAX + 6]);
        assert_eq!(shared.chain().len(), CHAIN_MAX);
        shared.set_chain(&[]);
        assert!(shared.chain().is_empty());

        shared.set_chain(&[0, 1]);
        shared.set_steps(4);
        shared.set_tempo(120.0);
        assert_eq!(shared.song_frames(RATE), 48_000);
        shared.set_chain(&[0, 1, 0, 3]);
        assert_eq!(shared.song_frames(RATE), 96_000);

        // The position is only there while a song plays.
        let (mut s, shared) = song_rig();
        shared.set_chain(&[0, 1]);
        assert_eq!(shared.chain_position(), None, "stopped");
        shared.set_playing(true);
        play_blocks(&mut s, &shared, 100, 100);
        assert_eq!(shared.chain_position(), None, "song mode is off");
        shared.set_song_mode(true);
        assert_eq!(shared.chain_position(), None, "still the plain pass until the wrap");
        play_blocks(&mut s, &shared, 24_000, 1_000);
        assert_eq!(shared.chain_position(), Some(0), "the song starts at the wrap");
        shared.set_playing(false);
        play_blocks(&mut s, &shared, 10, 10);
        assert_eq!(shared.chain_position(), None, "stopped again");
    }

    #[test]
    fn song_mode_ignores_the_current_bank_and_an_empty_chain_plays_it() {
        let (mut s, shared) = song_rig();
        a_and_b(&shared);
        shared.set_chain(&[0]);
        shared.set_song_mode(true);
        shared.set_song_loop(true);
        shared.set_playing(true);
        let mut left = play_blocks(&mut s, &shared, 3_000, 500);
        shared.set_bank(1); // the bank being edited; the song still plays A
        left.extend(play_blocks(&mut s, &shared, 45_000, 500));
        assert_eq!(onsets(&left), vec![0, 24_000]);
        assert_eq!(pad_at(&left, 24_000), Some(0));

        // Nothing to walk: the current bank loops, as outside song mode.
        let (mut s, shared) = song_rig();
        a_and_b(&shared);
        shared.set_song_mode(true);
        shared.set_bank(1);
        shared.set_playing(true);
        let left = play_blocks(&mut s, &shared, 48_000, 500);
        assert_eq!(onsets(&left), vec![0, 24_000]);
        assert_eq!(pad_at(&left, 0), Some(1));
        assert_eq!(shared.chain_position(), None);
        assert!(shared.is_playing());
    }

    #[test]
    fn the_chain_can_change_while_the_song_plays() {
        let (mut s, shared) = song_rig();
        a_and_b(&shared);
        shared.set_chain(&[0, 1]);
        shared.set_song_mode(true);
        shared.set_playing(true);
        play_blocks(&mut s, &shared, 30_000, 777); // in entry 1
        shared.set_chain(&[0, 1, 0]);
        let left = play_blocks(&mut s, &shared, 42_000, 777);
        assert!(shared.is_playing(), "a third entry now follows");
        assert_eq!(onsets(&left), vec![18_000], "entry 2 starts at 48,000");
        assert_eq!(pad_at(&left, 18_000), Some(0));
        play_blocks(&mut s, &shared, 1, 1);
        assert!(!shared.is_playing(), "and the song ends after it, at 72,000");

        // A shorter chain ends the song at the next wrap.
        let (mut s, shared) = song_rig();
        a_and_b(&shared);
        shared.set_chain(&[0, 1, 0]);
        shared.set_song_mode(true);
        shared.set_playing(true);
        play_blocks(&mut s, &shared, 10_000, 777);
        shared.set_chain(&[0]);
        play_blocks(&mut s, &shared, 14_000, 777);
        assert!(shared.is_playing());
        play_blocks(&mut s, &shared, 1, 1);
        assert!(!shared.is_playing());
    }

    /// A plays the click and B the flat pad, on every step; the song is B.
    fn song_switch_rig(song: bool) -> (Sampler, SamplerShared) {
        let (s, shared) = song_rig();
        shared.set_bank_pattern(0, 0, 0b1111);
        shared.set_bank_pattern(1, 1, 0b1111);
        shared.set_chain(&[1]);
        shared.set_song_mode(song);
        shared.set_song_loop(true);
        shared.set_playing(true);
        (s, shared)
    }

    #[test]
    fn song_mode_turned_on_or_off_waits_for_the_wrap() {
        // On: the song starts at entry 0 on the next wrap.
        let (mut s, shared) = song_switch_rig(false);
        play_blocks(&mut s, &shared, 10_000, 777);
        shared.set_song_mode(true);
        assert_eq!(shared.chain_position(), None, "queued");
        let left = play_blocks(&mut s, &shared, 14_000, 777);
        assert_eq!(pad_at(&left, 2_000), Some(0), "bank A until the wrap (step 2)");
        assert_eq!(pad_at(&left, 8_000), Some(0), "and step 3");
        assert_eq!(shared.chain_position(), None);
        let left = play_blocks(&mut s, &shared, 6_000, 777);
        assert_eq!(pad_at(&left, 0), Some(1), "then the chain's bank B, on the wrap");
        assert_eq!(shared.chain_position(), Some(0));

        // Off: the song's pass plays to its end, then the current bank.
        let (mut s, shared) = song_switch_rig(true);
        let left = play_blocks(&mut s, &shared, 7_000, 777); // steps 0 and 1
        assert_eq!(pad_at(&left, 0), Some(1));
        assert_eq!(pad_at(&left, 6_000), Some(1));
        shared.set_song_mode(false);
        assert_eq!(shared.chain_position(), Some(0), "song mode is off, but its pass still plays");
        let left = play_blocks(&mut s, &shared, 17_000, 777); // to 24,000
        assert_eq!(pad_at(&left, 5_000), Some(1), "step 2 is still the song's B");
        assert_eq!(pad_at(&left, 11_000), Some(1), "and step 3");
        assert_eq!(shared.playing_bank(), Some(1));
        let left = play_blocks(&mut s, &shared, 6_000, 777);
        assert_eq!(pad_at(&left, 0), Some(0), "the wrap plays bank A (the current one)");
        assert_eq!(shared.chain_position(), None);
        assert_eq!(shared.playing_bank(), Some(0));
    }

    #[test]
    fn switching_now_stops_a_song_on_the_next_step_but_starts_one_at_the_wrap() {
        // On: entry 0 still waits for the wrap, so it plays a whole pass, even
        // when the song doesn't loop and has only that entry.
        let (mut s, shared) = song_switch_rig(false);
        shared.set_switch_now(true);
        shared.set_song_loop(false);
        play_blocks(&mut s, &shared, 10_000, 777); // inside step 1
        shared.set_song_mode(true);
        let left = play_blocks(&mut s, &shared, 14_000, 777);
        assert_eq!(pad_at(&left, 2_000), Some(0), "step 2 is still bank A");
        assert_eq!(pad_at(&left, 8_000), Some(0), "and step 3");
        assert_eq!(shared.chain_position(), None);
        let left = play_blocks(&mut s, &shared, 24_000, 777);
        assert_eq!(onsets(&left), vec![0, 6_000, 12_000, 18_000], "all four steps of entry 0");
        assert_eq!(pad_at(&left, 0), Some(1), "the song's B from the wrap");
        assert_eq!(shared.chain_position(), Some(0));
        assert!(shared.is_playing());
        play_blocks(&mut s, &shared, 1, 1);
        assert!(!shared.is_playing(), "the song ends after its one whole pass");

        // Off: the next step reads the current bank again.
        let (mut s, shared) = song_switch_rig(true);
        shared.set_switch_now(true);
        play_blocks(&mut s, &shared, 7_000, 777); // steps 0 and 1
        shared.set_song_mode(false);
        let left = play_blocks(&mut s, &shared, 17_000, 777);
        assert_eq!(pad_at(&left, 5_000), Some(0), "step 2 plays bank A (the current one)");
        assert_eq!(shared.chain_position(), None);
    }

    #[test]
    fn stopping_with_a_switch_queued_plays_the_queued_bank_from_the_top() {
        let (mut s, shared) = song_rig();
        shared.set_bank_pattern(0, 0, 0b1111);
        shared.set_bank_pattern(1, 1, 0b1111);
        shared.set_playing(true);
        play_blocks(&mut s, &shared, 10_000, 500);
        shared.set_bank(1);
        assert_eq!(shared.playing_bank(), Some(0), "B waits");
        shared.set_playing(false);
        play_blocks(&mut s, &shared, 500, 500);
        assert_eq!(shared.playing_bank(), None);
        shared.set_playing(true);
        let left = play_blocks(&mut s, &shared, 24_000, 500);
        assert_eq!(onsets(&left), vec![0, 6_000, 12_000, 18_000]);
        assert_eq!(pad_at(&left, 0), Some(1), "play starts B on its first step");
        assert_eq!(shared.playing_bank(), Some(1));
    }

    #[test]
    fn a_shorter_pattern_keeps_a_waiting_switch_on_the_bar() {
        let (mut s, shared) = song_rig();
        shared.set_steps(8);
        shared.set_bank_pattern(0, 0, 0xFF);
        shared.set_bank_pattern(1, 1, 0xFF);
        shared.set_playing(true);
        play_blocks(&mut s, &shared, 40_000, 500); // inside step 6
        shared.set_bank(1);
        shared.set_steps(4); // step 7 is past the new end
        let left = play_blocks(&mut s, &shared, 14_000, 500); // to 54,000
        assert_eq!(onsets(&left), vec![2_000, 8_000], "steps at 42,000 and 48,000");
        assert_eq!(pad_at(&left, 2_000), Some(0), "42,000 is step 3 of the new length, still A");
        assert_eq!(pad_at(&left, 8_000), Some(1), "B starts on the bar, at 48,000");
        assert_eq!(shared.current_step(), Some(0));
    }

    #[test]
    fn render_song_renders_the_chain_once_sample_exact() {
        let (_, shared) = song_rig();
        a_and_b(&shared);
        shared.set_chain(&[0, 1, 0]);
        let samples = [Some(click(100)), Some(flat(100, 0.8))];
        let out = render_song(&samples, &shared, RATE);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), 72_000);
        assert_eq!(out[1].len(), 72_000);
        assert_eq!(onsets(&out[0]), vec![0, 24_000, 48_000]);
        assert_eq!(pad_at(&out[0], 24_000), Some(1));
        assert!(!shared.is_playing(), "the live transport is untouched");
        assert_eq!(shared.chain_position(), None);

        // The loop flag and song mode don't change an export, and nothing is left over.
        shared.set_song_loop(true);
        shared.set_song_mode(true);
        assert_eq!(render_song(&samples, &shared, RATE), out);

        // It is what playing the song live gives.
        let (mut s, live) = song_rig();
        a_and_b(&live);
        live.set_chain(&[0, 1, 0]);
        live.set_song_mode(true);
        live.set_playing(true);
        assert_eq!(play_blocks(&mut s, &live, 72_000, 999), out[0]);

        // An empty chain renders nothing.
        shared.set_chain(&[]);
        let out = render_song(&samples, &shared, RATE);
        assert!(out[0].is_empty() && out[1].is_empty());
    }

    #[test]
    fn render_song_length_follows_the_exact_pass_length() {
        let (_, shared) = song_rig();
        shared.set_tempo(130.0); // a step is 5,538.46 frames, a pass 22,153.85
        a_and_b(&shared);
        shared.set_chain(&[0, 1, 0]);
        let samples = [Some(click(100)), Some(flat(100, 0.8))];
        let out = render_song(&samples, &shared, RATE);
        assert_eq!(out[0].len(), 66_462, "three passes, rounded once");
        assert_eq!(out[0].len(), shared.song_frames(RATE));
        assert_eq!(onsets(&out[0]), vec![0, 22_154, 44_308], "entries start on the wrap, not 22,153 apart");
    }

    #[test]
    fn render_pattern_copies_every_bank_chance_and_ratchet() {
        let (_, shared) = song_rig();
        a_and_b(&shared);
        shared.set_bank_pattern(1, 0, 0b0100); // bank B also plays pad 0 on step 2
        shared.set_ratchet(1, 0, 2, 2);
        shared.set_bank_pattern(2, 0, 0b0001);
        shared.set_chance(2, 0, 0, 0.0);
        let samples = [Some(click(100)), Some(flat(100, 0.8))];

        // The current bank plays, as before.
        let out = render_pattern(&samples, &shared, RATE, 1);
        assert_eq!(onsets(&out[0]), vec![0]);
        assert_eq!(pad_at(&out[0], 0), Some(0));

        shared.set_bank(1);
        let out = render_pattern(&samples, &shared, RATE, 2);
        assert_eq!(out[0].len(), 48_000);
        assert_eq!(onsets(&out[0]), vec![0, 12_000, 15_000, 24_000, 36_000, 39_000]);
        assert_eq!(pad_at(&out[0], 0), Some(1));
        assert_eq!(pad_at(&out[0], 15_000), Some(0), "the ratchet's second hit");

        shared.set_bank(2);
        assert!(render_pattern(&samples, &shared, RATE, 1)[0].iter().all(|&x| x == 0.0), "its chance is 0");

        // Even in song mode, a pattern render is the current bank looped.
        shared.set_bank(0);
        shared.set_chain(&[1, 1]);
        shared.set_song_mode(true);
        let out = render_pattern(&samples, &shared, RATE, 2);
        assert_eq!(onsets(&out[0]), vec![0, 24_000]);
        assert_eq!(pad_at(&out[0], 0), Some(0));
        assert!(!shared.is_playing());
    }

    #[test]
    fn a_chance_of_zero_never_plays_and_one_always_does() {
        let shared = SamplerShared::default();
        shared.set_tempo(120.0);
        shared.set_steps(16);
        shared.set_pattern(0, 0xFFFF);
        let samples = [Some(click(100))];
        assert_eq!(onsets(&render_pattern(&samples, &shared, RATE, 4)[0]).len(), 64, "chance 1 is the default");

        for step in 0..16 {
            shared.set_chance(0, 0, step, 0.0);
        }
        let out = render_pattern(&samples, &shared, RATE, 4);
        assert!(out[0].iter().all(|&x| x == 0.0), "chance 0 never plays");

        shared.set_chance(0, 0, 3, 1.0);
        let out = render_pattern(&samples, &shared, RATE, 4);
        assert_eq!(onsets(&out[0]), vec![3 * 6_000, 19 * 6_000, 35 * 6_000, 51 * 6_000]);

        // A step that is off stays off whatever its chance.
        shared.set_pattern(0, 0);
        shared.set_chance(0, 0, 3, 1.0);
        assert!(render_pattern(&samples, &shared, RATE, 1)[0].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn a_chance_in_between_plays_that_share_of_the_steps_and_repeats() {
        let shared = SamplerShared::default();
        shared.set_tempo(300.0); // 2,400 frames a step keeps this quick
        shared.set_steps(64);
        shared.set_pattern(0, u64::MAX);
        let samples = [Some(click(100))];
        let passes = 8; // 512 steps in all

        for step in 0..64 {
            shared.set_chance(0, 0, step, 0.5);
        }
        let first = render_pattern(&samples, &shared, RATE, passes);
        let hits = onsets(&first[0]);
        assert!((200..=312).contains(&hits.len()), "about half of 512: {}", hits.len());
        assert!(hits.len() < 512);

        let again = render_pattern(&samples, &shared, RATE, passes);
        assert_eq!(first, again, "an export comes out the same every time");
        // The dice are not the same each pass.
        let pass = 64 * 2_400;
        let in_pass = |n: usize| -> Vec<usize> { hits.iter().filter(|&&h| h / pass == n).map(|&h| h % pass).collect() };
        assert_ne!(in_pass(0), in_pass(1));

        for step in 0..64 {
            shared.set_chance(0, 0, step, 0.25);
        }
        let quarter = onsets(&render_pattern(&samples, &shared, RATE, passes)[0]).len();
        assert!((90..=170).contains(&quarter), "about a quarter of 512: {quarter}");
        assert!(quarter < hits.len());

        // A song export repeats too.
        shared.set_chain(&[0]);
        let a = render_song(&samples, &shared, RATE);
        let b = render_song(&samples, &shared, RATE);
        assert_eq!(a, b);
    }

    #[test]
    fn a_ratchet_splits_the_step_into_evenly_spaced_hits() {
        let (_, shared) = song_rig(); // 6,000 frames a step
        shared.set_pattern(0, 0b0101);
        let samples = [Some(click(100))];
        let onsets_of = |shared: &SamplerShared| onsets(&render_pattern(&samples, shared, RATE, 1)[0]);
        assert_eq!(onsets_of(&shared), vec![0, 12_000]);

        shared.set_ratchet(0, 0, 0, 2);
        assert_eq!(onsets_of(&shared), vec![0, 3_000, 12_000]);
        shared.set_ratchet(0, 0, 0, 4);
        assert_eq!(onsets_of(&shared), vec![0, 1_500, 3_000, 4_500, 12_000]);
        shared.set_ratchet(0, 0, 2, 4);
        assert_eq!(onsets_of(&shared), vec![0, 1_500, 3_000, 4_500, 12_000, 13_500, 15_000, 16_500]);
        shared.set_ratchet(0, 0, 2, 1);

        // A step's own length counts, so swing moves the hits with it.
        shared.set_swing(1.0); // steps are 8,000 and 4,000 frames
        shared.set_pattern(0, 0b0011);
        shared.set_ratchet(0, 0, 0, 2);
        shared.set_ratchet(0, 0, 1, 4);
        assert_eq!(onsets_of(&shared), vec![0, 4_000, 8_000, 9_000, 10_000, 11_000]);
        shared.set_swing(0.0);

        // A length that isn't a whole number of frames: hits land on the frame
        // after the exact position, as the steps themselves do.
        shared.set_tempo(130.0); // a step is 5,538.46 frames
        shared.set_pattern(0, 0b0001);
        shared.set_ratchet(0, 0, 0, 3);
        assert_eq!(onsets_of(&shared), vec![0, 1_847, 3_693]);
    }

    #[test]
    fn ratcheted_hits_play_at_the_steps_velocity_and_across_blocks() {
        let (mut s, shared) = song_rig();
        shared.set_pattern(0, 0b0001);
        shared.set_velocity(0, 0, 0.5);
        shared.set_ratchet(0, 0, 0, 4);
        shared.set_playing(true);
        let left = play_blocks(&mut s, &shared, 24_000, 997); // odd blocks cut across the hits
        assert_eq!(onsets(&left), vec![0, 1_500, 3_000, 4_500]);
        let first = peak(&left[0..100]);
        assert!((first - 0.5 * 10.0 / 24.0).abs() < 0.01, "half of the click's peak: {first}");
        for at in [1_500, 3_000, 4_500] {
            assert!((peak(&left[at..at + 100]) - first).abs() < 1e-6, "hit at {at}");
        }
        let rendered = render_pattern(&[Some(click(100))], &shared, RATE, 1);
        assert_eq!(left, rendered[0], "live and offline agree");
    }

    #[test]
    fn ratchets_follow_chance_mute_and_stopping() {
        let (mut s, shared) = song_rig();
        shared.set_pattern(0, 0b0001);
        shared.set_ratchet(0, 0, 0, 4);
        shared.set_chance(0, 0, 0, 0.0);
        let samples = [Some(click(100))];
        assert!(render_pattern(&samples, &shared, RATE, 1)[0].iter().all(|&x| x == 0.0), "a step that doesn't play doesn't ratchet");
        shared.set_chance(0, 0, 0, 1.0);

        // Muting mid-step silences the hits still to come.
        shared.set_playing(true);
        let mut left = play_blocks(&mut s, &shared, 2_000, 500);
        shared.set_pad(0, &PadParams { mute: true, ..Default::default() });
        left.extend(play_blocks(&mut s, &shared, 10_000, 500));
        assert_eq!(onsets(&left), vec![0, 1_500]);

        // So does stopping, and the next play starts clean.
        shared.set_pad(0, &PadParams::default());
        shared.set_playing(false);
        play_blocks(&mut s, &shared, 10, 10);
        shared.set_playing(true);
        let mut left = play_blocks(&mut s, &shared, 100, 100);
        shared.set_playing(false);
        left.extend(play_blocks(&mut s, &shared, 10_000, 500));
        assert_eq!(onsets(&left), vec![0], "the hits after the stop never come");
        shared.set_playing(true);
        let left = play_blocks(&mut s, &shared, 6_000, 500);
        assert_eq!(onsets(&left), vec![0, 1_500, 3_000, 4_500]);
    }

    #[test]
    fn recording_writes_into_the_current_bank() {
        let (mut s, shared) = recording_rig();
        shared.set_bank(2);
        run(&mut s, &shared, 12_500);
        shared.trigger(0);
        run(&mut s, &shared, 100);
        assert_eq!(shared.bank_pattern(2, 0), 1 << 2);
        assert_eq!(shared.bank_pattern(0, 0), 0, "not into bank A");
        assert_eq!(shared.pattern(0), 1 << 2, "the alias reads it back");
        assert_eq!(shared.recorded(), 1);

        // A hit late in a step goes to the next, which this bank's own sequencer then skips once.
        let (mut s, shared) = recording_rig();
        shared.set_bank(1);
        run(&mut s, &shared, 12_000 + 4_500);
        shared.trigger(0);
        run(&mut s, &shared, 100);
        assert_eq!(shared.bank_pattern(1, 0), 1 << 3);
        assert_eq!(shared.played(0), 1, "the hand hit, over an empty bank");
        run(&mut s, &shared, 2_000); // step 3 fires at 18,000
        assert_eq!(shared.played(0), 1, "the sequencer skips it this pass");
        run(&mut s, &shared, 16 * 6_000 - 100);
        assert_eq!(shared.played(0), 2, "and plays it on the next");
    }

    #[test]
    fn a_hit_at_the_end_of_a_pass_goes_to_the_queued_banks_first_step_once() {
        let (mut s, shared) = recording_rig();
        shared.set_bank_pattern(0, 0, 0xFFFF); // A plays pad 0 on every step
        run(&mut s, &shared, 30_000);
        shared.set_bank(1); // B, empty, waits for the wrap at 96,000
        run(&mut s, &shared, 90_000 + 4_500 - 30_000); // late in A's last step
        assert_eq!(shared.played(0), 16, "all of A's pass");
        shared.trigger(0);
        run(&mut s, &shared, 100);
        assert_eq!(shared.bank_pattern(1, 0), 1, "written to B's step 0, which plays next");
        assert_eq!(shared.bank_pattern(0, 0), 0xFFFF, "A is untouched");
        assert_eq!(shared.played(0), 17, "the hand hit");
        run(&mut s, &shared, 2_000); // the wrap at 96,000 starts B
        assert_eq!(shared.playing_bank(), Some(1));
        assert_eq!(shared.played(0), 17, "B's step 0 is skipped this pass");
        run(&mut s, &shared, 96_000);
        assert_eq!(shared.played(0), 18, "and plays on the next");
    }

    #[test]
    fn a_hit_while_a_switch_waits_goes_into_the_pattern_playing() {
        let (mut s, shared) = recording_rig();
        run(&mut s, &shared, 25_000); // 1,000 frames into step 4
        shared.set_bank(1); // B waits for the wrap at 96,000
        shared.trigger(0);
        run(&mut s, &shared, 100);
        assert_eq!(shared.bank_pattern(0, 0), 1 << 4, "on A's step 4, which was playing");
        assert_eq!(shared.bank_pattern(1, 0), 0, "not B");

        run(&mut s, &shared, 34_500 - 25_100); // late in step 5
        shared.trigger(0);
        run(&mut s, &shared, 100);
        assert_eq!(shared.bank_pattern(0, 0), 1 << 4 | 1 << 6, "on A's step 6, which plays next");
        assert_eq!(shared.played(0), 2, "the two hand hits");
        run(&mut s, &shared, 2_000); // step 6 fires at 36,000
        assert_eq!(shared.played(0), 2, "A's step 6 is skipped this pass");
        run(&mut s, &shared, 96_000 - 36_600 + 1); // through the wrap
        assert_eq!(shared.playing_bank(), Some(1), "B from the wrap");
        assert_eq!(shared.bank_pattern(1, 0), 0);
        assert_eq!(shared.played(0), 2, "B is empty");
    }

    #[test]
    fn recording_in_song_mode_does_not_skip_a_bank_it_did_not_write_to() {
        let (mut s, shared) = recording_rig();
        shared.set_bank_pattern(0, 0, 0xFFFF); // the song plays A, every step
        shared.set_chain(&[0]);
        shared.set_song_mode(true);
        shared.set_song_loop(true);
        shared.set_bank(1); // while the edit bank is B
        run(&mut s, &shared, 12_000 + 4_500); // late in step 2
        assert_eq!(shared.played(0), 3, "steps 0 to 2 of A");
        shared.trigger(0);
        run(&mut s, &shared, 100);
        assert_eq!(shared.bank_pattern(1, 0), 1 << 3, "written to the current bank");
        assert_eq!(shared.bank_pattern(0, 0), 0xFFFF);
        assert_eq!(shared.played(0), 4);
        run(&mut s, &shared, 2_000); // step 3 fires at 18,000
        assert_eq!(shared.played(0), 5, "A's step 3 still plays");
    }

    // The Sampler on the engine's transport and graph.

    use crate::graph::{Graph, Mixer};
    use crate::testing::realtime;

    /// The Sampler as the only track of a graph, as the engine runs it, with
    /// `samples` sent through its queue.
    struct OnGraph {
        mixer: Mixer,
        graph: Graph,
        clock: Transport,
        link: SamplerLink,
    }

    impl OnGraph {
        fn new(shared: &Arc<SamplerShared>, samples: &[(usize, Track)]) -> Self {
            let (dev, mut link) = device(shared.clone(), RATE);
            let mut mixer = Mixer::new();
            let t = mixer.add_track("Sampler").unwrap();
            mixer.set_source(t, Some(Box::new(dev))).unwrap();
            let graph = mixer.start(RATE);
            for (slot, data) in samples {
                link.send(*slot, Some(data.clone())).unwrap();
            }
            Self { mixer, graph, clock: Transport::new(RATE), link }
        }

        /// Plays `frames` frames in blocks of `block`; returns left and right.
        fn play(&mut self, shared: &SamplerShared, frames: usize, block: usize) -> (Vec<f32>, Vec<f32>) {
            let (mut l, mut r) = (Vec::with_capacity(frames), Vec::with_capacity(frames));
            let mut done = 0;
            while done < frames {
                let n = block.min(frames - done);
                self.graph.run(&mut self.clock, shared.clock(), n);
                let (a, b) = self.graph.master();
                l.extend_from_slice(a);
                r.extend_from_slice(b);
                done += n;
            }
            (l, r)
        }
    }

    /// The same busy pattern on two Samplers: swing, an odd length, pitch,
    /// pan, a filter and a ratchet.
    fn busy(s: &SamplerShared) {
        s.set_tempo(133.0);
        s.set_swing(0.4);
        s.set_steps(12);
        s.set_pattern(0, 0b1010_0101_0011);
        s.set_pattern(1, 0b0100_0010_1000);
        s.set_pad(1, &PadParams { gain: 0.7, pan: -0.3, pitch: 3.0, filter: FILTER_LOW_PASS, cutoff: 3_000.0, ..Default::default() });
        s.set_ratchet(0, 0, 4, 3);
        s.set_metronome(true);
    }

    #[test]
    fn the_sampler_on_its_track_sounds_exactly_as_on_its_own() {
        let samples = [(0, click(400)), (1, sine(220.0, 30_000))];
        let on_track = Arc::new(SamplerShared::default());
        let alone = SamplerShared::default();
        busy(&on_track);
        busy(&alone);
        let mut g = OnGraph::new(&on_track, &samples);
        let mut s = Sampler::offline(RATE);
        for (slot, data) in &samples {
            s.set_sample(*slot, Some(data.clone()));
        }
        on_track.set_playing(true);
        alone.set_playing(true);

        let frames = 2 * RATE as usize;
        let (l, r) = g.play(&on_track, frames, 512);
        let mut out = vec![0.0; frames * 2];
        for block in out.chunks_mut(512 * 2) {
            s.render(&alone, block, 2);
        }
        assert!(l.iter().any(|&x| x != 0.0), "something played");
        let (al, ar): (Vec<f32>, Vec<f32>) = out.chunks(2).map(|f| (f[0], f[1])).unzip();
        assert!(l == al && r == ar, "the track adds nothing and loses nothing: bit for bit the same");
    }

    #[test]
    fn steps_follow_the_clock_through_a_tempo_change() {
        let shared = Arc::new(SamplerShared::default());
        shared.set_tempo(120.0);
        shared.set_steps(16);
        shared.set_pattern(0, 0xFFFF);
        let mut g = OnGraph::new(&shared, &[(0, click(100))]);
        shared.set_playing(true);
        let (mut left, _) = g.play(&shared, 27_000, 1_000); // to beat 1.125, inside step 4
        shared.set_tempo(90.0); // a beat is now 32,000 frames, a sixteenth 8,000
        left.extend(g.play(&shared, 33_000, 1_000).0);
        // Step 5 is at beat 1.25: 0.125 beats after the change, 4,000 frames at 90.
        assert_eq!(onsets(&left), vec![0, 6_000, 12_000, 18_000, 24_000, 31_000, 39_000, 47_000, 55_000]);
        // Each one is where the transport's clock reaches the step's beat.
        let beat = |frame: usize| if frame <= 27_000 { frame as f64 / 24_000.0 } else { 1.125 + (frame - 27_000) as f64 / 32_000.0 };
        for (k, &at) in onsets(&left).iter().enumerate() {
            assert!((beat(at) - k as f64 * 0.25).abs() < 1e-9, "step {k} at frame {at} is beat {}", beat(at));
        }
    }

    #[test]
    fn a_transport_loop_starts_the_pattern_again_on_its_frame() {
        let shared = Arc::new(SamplerShared::default());
        shared.set_tempo(120.0);
        shared.set_steps(16);
        shared.set_pattern(0, 0b0000_1111); // the click on the first beat's four steps
        shared.set_pattern(1, 0b0001_0000); // the flat pad on beat 2, past the loop
        shared.clock().set_loop(0.0, 1.0);
        shared.clock().set_looping(true);
        let mut g = OnGraph::new(&shared, &[(0, click(100)), (1, flat(100, 0.8))]);
        shared.set_playing(true);
        let (left, _) = g.play(&shared, 72_000, 777);
        let on = onsets(&left);
        assert_eq!(on, (0..12).map(|k| k * 6_000).collect::<Vec<_>>(), "four steps a beat, again from each wrap");
        assert!(on.iter().all(|&at| pad_at(&left, at) == Some(0)), "the step past the loop never plays");
    }

    #[test]
    fn a_song_end_stops_the_transport_every_track_runs_on() {
        let shared = Arc::new(SamplerShared::default());
        shared.set_tempo(120.0);
        shared.set_steps(4);
        shared.set_bank_pattern(0, 0, 0b0001);
        shared.set_chain(&[0]);
        shared.set_song_mode(true);
        shared.set_song_loop(false);
        let mut g = OnGraph::new(&shared, &[(0, click(100))]);
        shared.set_playing(true);
        let (left, _) = g.play(&shared, 30_000, 1_000);
        assert_eq!(onsets(&left), vec![0], "one pass, then the end");
        assert!(!shared.clock().is_playing(), "the song's end stopped the transport");
        let at = shared.clock().position_frames();
        g.play(&shared, 5_000, 1_000);
        assert_eq!(shared.clock().position_frames(), at, "and the clock holds still");
        // Play again: the song starts over from its first entry.
        shared.set_playing(true);
        let (left, _) = g.play(&shared, 1_000, 1_000);
        assert_eq!(onsets(&left), vec![0]);
    }

    #[test]
    fn a_new_rate_does_not_play_old_hits_again() {
        let shared = Arc::new(SamplerShared::default());
        let (mut dev, mut link) = device(shared.clone(), RATE);
        link.send(0, Some(dc(48_000, 0.5))).unwrap();
        shared.trigger(0);
        shared.release(0);
        let (mut l, mut r) = (vec![0.0; 256], vec![0.0; 256]);
        let ctx = ProcessContext::stopped(RATE, 256);
        dev.process(&ctx, &mut Events::with_capacity(1), Buffers { left: &mut l, right: &mut r });
        assert!(l[255] > 0.49, "the hit plays");
        dev.prepare(44_100, 4_096);
        link.collect();
        link.send(0, Some(dc(44_100, 0.5))).unwrap();
        let (mut l, mut r) = (vec![0.0; 256], vec![0.0; 256]);
        dev.process(&ProcessContext::stopped(44_100, 256), &mut Events::with_capacity(1), Buffers { left: &mut l, right: &mut r });
        assert!(l.iter().all(|&x| x == 0.0), "the same hit is not played again at the new rate");
    }

    #[test]
    fn the_sampler_track_never_allocates_on_the_audio_thread() {
        let shared = Arc::new(SamplerShared::default());
        busy(&shared);
        shared.set_chain(&[0, 1, 0]);
        shared.set_song_mode(true);
        shared.set_song_loop(true);
        shared.set_recording(true);
        let mut g = OnGraph::new(&shared, &[(0, click(400)), (1, sine(330.0, 20_000))]);
        shared.set_playing(true);
        let mut total = 0;
        for i in 0..300 {
            // The main thread's side of a busy session, between blocks.
            match i % 6 {
                0 => shared.trigger_velocity(2 + i % 3, 0.6),
                1 => g.link.send(3, Some(click(200 + i))).unwrap(),
                2 => shared.set_bank(i % 4),
                3 => shared.set_tempo(100.0 + (i % 50) as f32),
                4 => shared.cut(0),
                _ => g.link.collect(),
            }
            if i == 150 {
                shared.set_playing(false);
            }
            if i == 160 {
                shared.set_playing(true);
            }
            let (_, n) = realtime(|| g.graph.run(&mut g.clock, shared.clock(), 512));
            total += n;
        }
        g.mixer.collect();
        assert_eq!(total, 0, "the audio thread allocated or freed {total} times");
    }

    #[test]
    fn a_full_return_queue_holds_old_samples_back_and_frees_none() {
        let shared = SamplerShared::default();
        let (mut s, mut link) = sampler(RATE);
        // Fill the return queue, as if the control side had stopped collecting.
        while s.garbage.as_mut().unwrap().push(Box::default()).is_ok() {}
        link.send.push(Box::new(SampleSlot { slot: 0, data: Some(click(10)) })).unwrap();
        link.send.push(Box::new(SampleSlot { slot: 1, data: Some(click(10)) })).unwrap();
        let mut out = vec![0.0; 128];
        let (_, allocs) = realtime(|| s.render(&shared, &mut out, 2));
        assert_eq!(allocs, 0, "nothing freed on the audio thread");
        assert!(s.held_back.is_some(), "the first one's box waits");
        assert_eq!(link.send.slots(), 31, "the second sample waits in its queue");
        link.collect();
        run(&mut s, &shared, 64);
        assert!(s.held_back.is_none() && s.samples[1].is_some(), "both arrive once there is room");
    }
}
