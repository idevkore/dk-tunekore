//! MIDI learn's logic: which controller moves which control, how a
//! controller's 0–127 (or 0–16383) lands on the control's range, what
//! happens when the knob and the value disagree, and the `midi` section of
//! the project file. Nothing here knows about Tauri or the engine: a control
//! is a target id that [`super::targets`] resolves.
//!
//! The rules:
//! - Learning takes the first control change that arrives. When its 14-bit
//!   partner (CC n and n + 32, n < 32) follows within [`PAIR_WAIT`], the
//!   mapping takes both halves.
//! - One controller moves one control, and a control listens to one
//!   controller: a new learn takes the controller and the control from any
//!   mapping that had either (the newest wins). A project file with
//!   duplicates reads the same way, the later entry winning.
//! - A run of moves with no gap longer than [`GESTURE_GAP`] is one gesture,
//!   and a gesture is one undo step (its key is [`Mapper::gesture_key`]).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How long learning waits for the fine half of a 14-bit pair.
pub const PAIR_WAIT: Duration = Duration::from_millis(40);
/// Moves closer together than this are one gesture, and one undo step.
pub const GESTURE_GAP: Duration = Duration::from_millis(500);
/// Pick-up takes over when the knob comes this close to the value (as a
/// fraction of its travel, two steps of a 7-bit knob).
pub const PICKUP_RANGE: f64 = 2.0 / 127.0;

/// Control changes that aren't knobs or faders, so learning skips them:
/// bank select (0, 32), data entry (6, 38), data increment and decrement
/// and the NRPN and RPN numbers (96–101). 120–127 are channel mode
/// messages, which [`parse_cc`] never returns.
const NOT_LEARNED: [u8; 10] = [0, 32, 6, 38, 96, 97, 98, 99, 100, 101];

/// One control change as it arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cc {
    /// 1..=16, as controllers label them.
    pub channel: u8,
    /// 0..=119.
    pub cc: u8,
    pub value: u8,
}

/// Reads a control change, or `None` for anything else (notes, clock,
/// channel mode messages 120–127).
pub fn parse_cc(msg: &[u8]) -> Option<Cc> {
    match *msg {
        [status, cc, value, ..] if status & 0xF0 == 0xB0 && cc < 120 && value < 128 => {
            Some(Cc { channel: (status & 0x0F) + 1, cc, value })
        }
        _ => None,
    }
}

/// Where a mapping's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Source {
    /// 1..=16.
    pub channel: u8,
    /// The controller; for a 14-bit pair its coarse half, 0..=31, with the
    /// fine half on `cc + 32`.
    pub cc: u8,
    /// 7, or 14 for a pair.
    pub bits: u8,
}

impl Source {
    pub fn seven(channel: u8, cc: u8) -> Self {
        Self { channel, cc, bits: 7 }
    }

    pub fn fourteen(channel: u8, coarse: u8) -> Self {
        Self { channel, cc: coarse, bits: 14 }
    }

    /// Whether a control change on `channel`, number `cc`, feeds this source.
    pub fn listens(&self, channel: u8, cc: u8) -> bool {
        channel == self.channel && (cc == self.cc || (self.bits == 14 && cc == self.cc + 32))
    }

    /// Whether two sources share a controller number on a channel.
    pub fn overlaps(&self, other: &Source) -> bool {
        let mine = [Some(self.cc), (self.bits == 14).then(|| self.cc + 32)];
        mine.iter().flatten().any(|&n| other.listens(self.channel, n))
    }

    /// Whether the numbers make sense: a channel 1..=16, a 7-bit controller
    /// 0..=119 or a 14-bit pair's coarse half 0..=31.
    pub fn is_valid(&self) -> bool {
        (1..=16).contains(&self.channel)
            && match self.bits {
                7 => self.cc < 120,
                14 => self.cc < 32,
                _ => false,
            }
    }

    /// "Ch 1 · CC 74", or "Ch 1 · CC 7+39 (14-bit)".
    pub fn describe(&self) -> String {
        if self.bits == 14 {
            format!("Ch {} · CC {}+{} (14-bit)", self.channel, self.cc, self.cc + 32)
        } else {
            format!("Ch {} · CC {}", self.channel, self.cc)
        }
    }
}

/// How a position along a knob's travel, 0..1, maps onto a control's range.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Curve {
    /// Straight from `min` to `max`: pan, resonance, sustain, swing, tempo.
    Linear { min: f64, max: f64 },
    /// Equal turns give equal ratios: frequencies. `min` is above 0.
    Log { min: f64, max: f64 },
    /// `min + (max - min) * t^power`: times, so short ones get most of the
    /// travel (the Inspector's time sliders are the same, with power 2).
    Power { min: f64, max: f64, power: f64 },
    /// A level in dB that feels like a mixer's fader: the gain follows the
    /// cube of the travel, `max_db` at the top, 0 dB about four fifths up,
    /// and off (`floor_db`) at the bottom tenth.
    #[serde(rename_all = "camelCase")]
    Fader { floor_db: f64, max_db: f64 },
}

impl Curve {
    /// The value at position `t`, 0..1.
    pub fn value(&self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match *self {
            Curve::Linear { min, max } => min + (max - min) * t,
            Curve::Log { min, max } => min * (max / min).powf(t),
            Curve::Power { min, max, power } => min + (max - min) * t.powf(power),
            Curve::Fader { floor_db, max_db } => {
                let gain = 10f64.powf(max_db / 20.0) * t * t * t;
                if gain <= 0.0 {
                    floor_db
                } else {
                    (20.0 * gain.log10()).clamp(floor_db, max_db)
                }
            }
        }
    }

    /// Where `value` sits along the travel, 0..1: the inverse of [`Curve::value`].
    pub fn position(&self, value: f64) -> f64 {
        let t = match *self {
            Curve::Linear { min, max } => (value - min) / (max - min),
            Curve::Log { min, max } => (value.max(min) / min).ln() / (max / min).ln(),
            Curve::Power { min, max, power } => ((value - min) / (max - min)).max(0.0).powf(1.0 / power),
            Curve::Fader { floor_db, max_db } => {
                if value <= floor_db {
                    0.0
                } else {
                    (10f64.powf((value - max_db) / 20.0)).cbrt()
                }
            }
        };
        if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.0 }
    }
}

/// A control that a controller can move.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    /// Stable, saved in projects: `sampler.pad.3.cutoff`, `transport.tempo`.
    pub id: String,
    /// "Cutoff".
    pub label: String,
    /// What it belongs to, for lists: "Selected pad", "Pad 3", "Output".
    pub group: String,
    pub curve: Curve,
    /// Values snap to whole multiples of this (1 BPM, a semitone); 0 for none.
    pub step: f64,
    /// Bipolar controls (pan, pitch): a knob's middle (64, or 8192) lands
    /// exactly on the middle of the range.
    pub centered: bool,
}

impl TargetInfo {
    /// The value for a knob at `t`, 0..1, snapped to the target's step.
    pub fn value(&self, t: f64) -> f64 {
        let v = self.curve.value(t);
        if self.step > 0.0 { (v / self.step).round() * self.step } else { v }
    }

    pub fn position(&self, value: f64) -> f64 {
        self.curve.position(value)
    }
}

/// A controller value as a position 0..1. For a centered target the middle
/// value (64, or 8192 for 14 bits) is exactly one half, so a knob at its
/// detent puts pan in the centre.
pub fn knob_position(raw: u16, bits: u8, centered: bool) -> f64 {
    let top = if bits == 14 { 16383.0 } else { 127.0 };
    let raw = f64::from(raw).min(top);
    if !centered {
        return raw / top;
    }
    let mid = (top + 1.0) / 2.0;
    if raw <= mid { raw / mid / 2.0 } else { 0.5 + (raw - mid) / (top - mid) / 2.0 }
}

/// A controller moving a control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub source: Source,
    pub target: String,
}

/// What a knob does when it and the value it moves disagree (after
/// switching pads, opening a project, or moving the control on screen).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Takeover {
    /// Nothing changes until the knob reaches the value, then it follows.
    #[default]
    Pickup,
    /// The value jumps to the knob.
    Jump,
    /// The value moves toward the end the knob is heading for, in
    /// proportion, until the two meet.
    Scale,
}

/// One control's new value, for a target provider to apply.
#[derive(Debug, Clone, PartialEq)]
pub struct Move {
    pub target: String,
    pub value: f64,
}

/// What the mapper knows about each mapped knob.
#[derive(Debug, Clone, Copy, Default)]
struct Knob {
    /// Where the knob was at its last message, 0..1.
    last: Option<f64>,
    /// The control's value as read back after this knob last set it: while
    /// it still reads the same, nothing else has moved the control.
    written: Option<f64>,
    /// The knob and the value move together (taken over).
    synced: bool,
}

/// A learn waiting for its control change.
#[derive(Debug, Clone)]
struct Learning {
    target: String,
    /// The first control change, while learning waits [`PAIR_WAIT`] to see
    /// if its 14-bit partner follows.
    first: Option<(Cc, Instant)>,
}

/// What a mapper needs from the controls: their ranges and their values now.
pub trait Lookup {
    fn info(&self, target: &str) -> Option<TargetInfo>;
    fn get(&self, target: &str) -> Option<f64>;
}

/// The mappings and the state of every knob that moves them.
#[derive(Default)]
pub struct Mapper {
    mappings: Vec<Mapping>,
    /// Entries of the project's `midi` section this build can't read,
    /// written back as they were.
    kept: Vec<Value>,
    learning: Option<Learning>,
    knobs: HashMap<Source, Knob>,
    /// The latest value of every controller seen, by (channel, number).
    raw: HashMap<(u8, u8), u8>,
    /// Sources whose value changed since [`Mapper::moves`] last ran.
    changed: Vec<Source>,
    gesture: u64,
    last_move: Option<Instant>,
    pub takeover: Takeover,
}

impl Mapper {
    pub fn mappings(&self) -> &[Mapping] {
        &self.mappings
    }

    /// The target being learned, if any.
    pub fn learning(&self) -> Option<&str> {
        self.learning.as_ref().map(|l| l.target.as_str())
    }

    /// Starts learning `target`: the next control change maps to it. `None` stops.
    pub fn learn(&mut self, target: Option<String>) {
        self.learning = target.map(|target| Learning { target, first: None });
    }

    /// Maps `source` to `target`, taking both from any mapping that had either.
    pub fn assign(&mut self, source: Source, target: String) -> Mapping {
        self.mappings.retain(|m| m.target != target && !m.source.overlaps(&source));
        self.knobs.retain(|s, _| !s.overlaps(&source));
        let m = Mapping { source, target };
        self.mappings.push(m.clone());
        m
    }

    /// Forgets the mapping on `target`. Returns whether there was one.
    pub fn forget(&mut self, target: &str) -> bool {
        let before = self.mappings.len();
        self.mappings.retain(|m| m.target != target);
        let gone = self.mappings.len() != before;
        if gone {
            self.knobs.retain(|s, _| self.mappings.iter().any(|m| m.source == *s));
        }
        gone
    }

    /// Forgets every mapping, for a new project.
    pub fn clear(&mut self) {
        self.mappings.clear();
        self.kept.clear();
        self.knobs.clear();
        self.learning = None;
    }

    /// The undo key for the current gesture: edits with it merge into one step.
    pub fn gesture_key(&self) -> String {
        format!("midi:{}", self.gesture)
    }

    /// Takes one control change. Returns the mapping it made, if it
    /// completed a learn.
    pub fn feed(&mut self, at: Instant, cc: Cc) -> Option<Mapping> {
        self.raw.insert((cc.channel, cc.cc), cc.value);
        if self.learning.is_some() {
            return self.learn_from(at, cc);
        }
        if self.last_move.is_none_or(|t| at.saturating_duration_since(t) > GESTURE_GAP) {
            self.gesture += 1;
        }
        self.last_move = Some(at);
        for m in &self.mappings {
            if m.source.listens(cc.channel, cc.cc) && !self.changed.contains(&m.source) {
                self.changed.push(m.source);
            }
        }
        None
    }

    fn learn_from(&mut self, at: Instant, cc: Cc) -> Option<Mapping> {
        let learning = self.learning.as_mut()?;
        match learning.first {
            None => {
                if NOT_LEARNED.contains(&cc.cc) {
                    return None;
                }
                if cc.cc < 64 {
                    // It may be half of a 14-bit pair: wait a moment for the other half.
                    learning.first = Some((cc, at));
                    return None;
                }
                self.finish(Source::seven(cc.channel, cc.cc))
            }
            Some((first, _)) => {
                let partner = cc.channel == first.channel && (cc.cc == first.cc + 32 || cc.cc + 32 == first.cc);
                if partner {
                    let coarse = first.cc.min(cc.cc);
                    return self.finish(Source::fourteen(cc.channel, coarse));
                }
                None
            }
        }
    }

    /// Completes a learn that waited for a 14-bit partner that didn't come.
    pub fn tick(&mut self, now: Instant) -> Option<Mapping> {
        let (first, at) = self.learning.as_ref()?.first?;
        if now.saturating_duration_since(at) < PAIR_WAIT {
            return None;
        }
        // No partner: a 7-bit controller (a lone 32–63 is one too).
        self.finish(Source::seven(first.channel, first.cc))
    }

    /// When a waiting learn completes, if one waits.
    pub fn deadline(&self) -> Option<Instant> {
        self.learning.as_ref()?.first.map(|(_, at)| at + PAIR_WAIT)
    }

    fn finish(&mut self, source: Source) -> Option<Mapping> {
        let target = self.learning.take()?.target;
        let m = self.assign(source, target);
        // The knob is where the learning message left it, so pick-up knows which side it's on.
        let pos = self.raw_value(&source).map(|raw| knob_position(raw, source.bits, false));
        self.knobs.insert(source, Knob { last: pos, written: None, synced: false });
        Some(m)
    }

    /// A source's latest value: 0..=127, or 0..=16383 for a 14-bit pair.
    fn raw_value(&self, s: &Source) -> Option<u16> {
        let coarse = u16::from(*self.raw.get(&(s.channel, s.cc))?);
        if s.bits == 14 {
            let fine = u16::from(self.raw.get(&(s.channel, s.cc + 32)).copied().unwrap_or(0));
            Some((coarse << 7) | fine)
        } else {
            Some(coarse)
        }
    }

    /// The new values for every control whose knob moved since the last
    /// call, after the takeover rule. Controls that don't exist now (a
    /// plugin that was removed) are skipped; their mappings stay.
    pub fn moves(&mut self, controls: &impl Lookup) -> Vec<Move> {
        let changed = std::mem::take(&mut self.changed);
        let mut out = Vec::new();
        for source in changed {
            let Some(m) = self.mappings.iter().find(|m| m.source == source) else { continue };
            let (Some(info), Some(raw)) = (controls.info(&m.target), self.raw_value(&source)) else { continue };
            let Some(now) = controls.get(&m.target) else { continue };
            let k = knob_position(raw, source.bits, info.centered);
            let cur = info.position(now);
            let knob = self.knobs.entry(source).or_default();
            // Still holding the value this knob last wrote: nothing else moved the control.
            let ours = knob.written == Some(now);
            let synced = ours && knob.synced;
            let near = |p: f64| (p - cur).abs() <= PICKUP_RANGE;
            let to = match self.takeover {
                Takeover::Jump => Some(k),
                Takeover::Pickup => {
                    let crossed = knob.last.is_some_and(|l| near(l) || (l - cur) * (k - cur) <= 0.0);
                    (synced || near(k) || crossed).then_some(k)
                }
                Takeover::Scale => {
                    if synced || near(k) {
                        Some(k)
                    } else {
                        match knob.last {
                            Some(l) if k < l && l > 0.0 => Some(cur * k / l),
                            Some(l) if k > l && l < 1.0 => Some(cur + (1.0 - cur) * (k - l) / (1.0 - l)),
                            _ => None,
                        }
                    }
                }
            };
            knob.last = Some(k);
            let Some(t) = to else { continue };
            knob.synced = (t - k).abs() <= PICKUP_RANGE;
            let value = info.value(t);
            if value != now {
                out.push(Move { target: m.target.clone(), value });
            } else {
                // Already there: the knob holds it now, with nothing to write.
                knob.written = Some(now);
            }
        }
        out
    }

    /// Notes what each moved control reads now that its moves were applied,
    /// so the next move knows whether something else changed it since.
    pub fn settle(&mut self, moves: &[Move], controls: &impl Lookup) {
        for mv in moves {
            let Some(m) = self.mappings.iter().find(|m| m.target == mv.target) else { continue };
            if let Some(knob) = self.knobs.get_mut(&m.source) {
                knob.written = controls.get(&mv.target);
            }
        }
    }

    // ---- The `midi` section ----

    /// The mappings as the project's `midi` section.
    pub fn to_file(&self) -> Value {
        let mut list: Vec<Value> = self
            .mappings
            .iter()
            .map(|m| serde_json::to_value(MappingFile::from(m)).unwrap_or(Value::Null))
            .collect();
        list.extend(self.kept.iter().cloned());
        json!({ "mappings": list })
    }

    /// Reads a project's `midi` section (`None`: the project has none).
    /// Entries this build can't read are kept to be written back.
    pub fn read_file(value: Option<&Value>) -> Result<FileMappings, String> {
        let mut out = FileMappings::default();
        let Some(value) = value else { return Ok(out) };
        let damaged = || "the project's MIDI mappings are damaged".to_string();
        let obj = value.as_object().ok_or_else(damaged)?;
        let list = match obj.get("mappings") {
            None | Some(Value::Null) => return Ok(out),
            Some(Value::Array(list)) => list,
            Some(_) => return Err(damaged()),
        };
        for entry in list {
            match serde_json::from_value::<MappingFile>(entry.clone()).ok().and_then(MappingFile::mapping) {
                Some(m) => out.mappings.push(m),
                None => out.kept.push(entry.clone()),
            }
        }
        Ok(out)
    }

    /// Replaces the mappings with a project's.
    pub fn load(&mut self, file: FileMappings) {
        self.clear();
        for m in file.mappings {
            self.assign(m.source, m.target);
        }
        self.kept = file.kept;
    }
}

/// A project's mappings, read and checked, waiting to be applied.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FileMappings {
    pub mappings: Vec<Mapping>,
    pub kept: Vec<Value>,
}

/// One mapping in the project file:
/// `{ "target": "sampler.pad.selected.cutoff", "channel": 1, "cc": 74 }`,
/// with `"bits": 14` for a 14-bit pair (`cc` is its coarse half).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MappingFile {
    target: String,
    channel: u8,
    cc: u8,
    #[serde(default = "seven_bits", skip_serializing_if = "is_seven")]
    bits: u8,
}

fn seven_bits() -> u8 {
    7
}

fn is_seven(b: &u8) -> bool {
    *b == 7
}

impl From<&Mapping> for MappingFile {
    fn from(m: &Mapping) -> Self {
        Self { target: m.target.clone(), channel: m.source.channel, cc: m.source.cc, bits: m.source.bits }
    }
}

impl MappingFile {
    fn mapping(self) -> Option<Mapping> {
        let source = Source { channel: self.channel, cc: self.cc, bits: self.bits };
        (source.is_valid() && !self.target.trim().is_empty()).then_some(Mapping { source, target: self.target })
    }
}
