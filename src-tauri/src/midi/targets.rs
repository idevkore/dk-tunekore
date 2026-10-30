//! What a controller can move: the target registry and the built-in targets.
//!
//! A target is a control with a stable id, saved in projects with its
//! mapping. Ids are dot-separated, and their first part names the provider
//! that owns them:
//!
//! | Id | Control |
//! | --- | --- |
//! | `sampler.pad.selected.<param>` | the selected pad's `<param>`: it follows the selection |
//! | `sampler.pad.<1..16>.<param>` | that pad's `<param>` only |
//! | `sampler.swing` | the Sampler's swing |
//! | `transport.tempo` | the tempo |
//! | `master.level` | the master level |
//! | `track.<id>.volume`, `track.<id>.pan` | a track's mix (track 1 is the Sampler's) |
//!
//! Pad params: see [`PAD_PARAMS`]. A module with controls of its own
//! (plugins, #37: `plugin.<id>.<param>`; the Mixer) implements [`Targets`]
//! for its prefix and calls `midi::register_targets` from its setup; the
//! mapping code doesn't change. A target that goes away (a removed plugin)
//! keeps its mappings, which do nothing until it is back.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tunekore_engine::{TrackId, PADS};

use super::map::{Curve, Move, TargetInfo};
use crate::sampler::{self, PadSettings};

/// The controls one module offers, under its id prefixes. `C` is the
/// context it needs: the app, in the app.
pub trait Targets<C>: Send + Sync + 'static {
    /// The first part of every id it owns: `["sampler", "transport"]`.
    fn prefixes(&self) -> &'static [&'static str];

    /// Every target it has now, for the list to learn from.
    fn list(&self, ctx: &C) -> Vec<TargetInfo>;

    /// One target's range and name, or `None` when it doesn't exist now.
    fn info(&self, ctx: &C, id: &str) -> Option<TargetInfo>;

    /// A target's value now, in its own units (dB, Hz, BPM).
    fn get(&self, ctx: &C, id: &str) -> Option<f64>;

    /// Applies one batch of moves. Every batch of one gesture carries the
    /// same `undo_key`: a module with undo should make the gesture one step.
    fn set(&self, ctx: &C, moves: &[Move], undo_key: &str) -> Result<(), String>;

    /// Called at most 30 times a second after `set` moved something, to
    /// tell the UI. Keep it small: it goes out while a knob turns.
    fn changed(&self, _ctx: &C) {}

    /// Called once a gesture that moved something here has ended (no move
    /// for [`super::map::GESTURE_GAP`]): the time to send the full state.
    fn settled(&self, _ctx: &C) {}
}

/// Every module's targets.
pub struct Registry<C> {
    providers: Vec<Arc<dyn Targets<C>>>,
}

impl<C: 'static> Default for Registry<C> {
    fn default() -> Self {
        Self { providers: Vec::new() }
    }
}

impl<C: 'static> Clone for Registry<C> {
    fn clone(&self) -> Self {
        Self { providers: self.providers.clone() }
    }
}

/// An id's first part: `sampler` for `sampler.pad.3.cutoff`.
fn prefix(id: &str) -> &str {
    id.split('.').next().unwrap_or("")
}

impl<C: 'static> Registry<C> {
    /// Adds a module's targets. Panics if another module has one of its prefixes.
    pub fn register(&mut self, targets: Arc<dyn Targets<C>>) {
        for p in targets.prefixes() {
            assert!(self.provider(p).is_none(), "two modules have MIDI targets called {p}.*");
        }
        self.providers.push(targets);
    }

    /// The provider that owns `id`, and its place in the list.
    pub fn provider(&self, id: &str) -> Option<(usize, &Arc<dyn Targets<C>>)> {
        let p = prefix(id);
        self.providers.iter().enumerate().find(|(_, t)| t.prefixes().contains(&p))
    }

    pub fn list(&self, ctx: &C) -> Vec<TargetInfo> {
        self.providers.iter().flat_map(|t| t.list(ctx)).collect()
    }

    pub fn info(&self, ctx: &C, id: &str) -> Option<TargetInfo> {
        self.provider(id)?.1.info(ctx, id)
    }

    pub fn get(&self, ctx: &C, id: &str) -> Option<f64> {
        self.provider(id)?.1.get(ctx, id)
    }

    /// Hands each provider its share of `moves`. Returns the providers that
    /// moved something (to tell later through [`Registry::changed`]) and
    /// the errors.
    pub fn apply(&self, ctx: &C, moves: &[Move], undo_key: &str) -> (Vec<usize>, Vec<String>) {
        let mut touched = Vec::new();
        let mut errors = Vec::new();
        for (i, t) in self.providers.iter().enumerate() {
            let mine: Vec<Move> = moves.iter().filter(|m| t.prefixes().contains(&prefix(&m.target))).cloned().collect();
            if mine.is_empty() {
                continue;
            }
            match t.set(ctx, &mine, undo_key) {
                Ok(()) => touched.push(i),
                Err(e) => errors.push(e),
            }
        }
        (touched, errors)
    }

    pub fn changed(&self, ctx: &C, which: &[usize]) {
        for &i in which {
            if let Some(t) = self.providers.get(i) {
                t.changed(ctx);
            }
        }
    }

    pub fn settled(&self, ctx: &C, which: &[usize]) {
        for &i in which {
            if let Some(t) = self.providers.get(i) {
                t.settled(ctx);
            }
        }
    }
}

// ---- The Sampler: pads, swing, tempo ----

/// A pad setting a controller can move.
pub struct PadParam {
    /// The last part of its id: `cutoff` in `sampler.pad.3.cutoff`.
    pub name: &'static str,
    pub label: &'static str,
    pub curve: Curve,
    pub step: f64,
    pub centered: bool,
    pub get: fn(&PadSettings) -> f32,
    pub set: fn(&mut PadSettings, f32),
}

const TIME: f64 = 2.0;

/// The pad settings a controller can move, with the same ranges and curves
/// as the Inspector's sliders: levels as a fader, the cutoff on a log scale,
/// times squared (short times get most of the travel), pan, pitch and the
/// bipolar amounts centred on the knob's middle.
pub const PAD_PARAMS: [PadParam; 18] = [
    PadParam { name: "level", label: "Level", curve: Curve::Fader { floor_db: -60.0, max_db: 6.0 }, step: 0.1, centered: false, get: |s| s.gain_db, set: |s, v| s.gain_db = v },
    PadParam { name: "pan", label: "Pan", curve: Curve::Linear { min: -1.0, max: 1.0 }, step: 0.0, centered: true, get: |s| s.pan, set: |s, v| s.pan = v },
    PadParam { name: "pitch", label: "Pitch", curve: Curve::Linear { min: -24.0, max: 24.0 }, step: 1.0, centered: true, get: |s| s.pitch, set: |s, v| s.pitch = v },
    PadParam {
        name: "start",
        label: "Start",
        curve: Curve::Linear { min: 0.0, max: 1.0 },
        step: 0.0,
        centered: false,
        get: |s| s.start,
        // As in the Inspector: the start pushes the end along, never past it.
        set: |s, v| {
            s.start = v.min(0.999);
            s.end = s.end.max(s.start + 0.001).min(1.0);
        },
    },
    PadParam {
        name: "end",
        label: "End",
        curve: Curve::Linear { min: 0.0, max: 1.0 },
        step: 0.0,
        centered: false,
        get: |s| s.end,
        set: |s, v| {
            s.end = v.max(0.001);
            s.start = s.start.min(s.end - 0.001).max(0.0);
        },
    },
    PadParam { name: "cutoff", label: "Cutoff", curve: Curve::Log { min: 20.0, max: 20_000.0 }, step: 0.0, centered: false, get: |s| s.cutoff, set: |s, v| s.cutoff = v },
    PadParam { name: "resonance", label: "Resonance", curve: Curve::Linear { min: 0.0, max: 1.0 }, step: 0.0, centered: false, get: |s| s.resonance, set: |s, v| s.resonance = v },
    PadParam { name: "attack", label: "Attack", curve: Curve::Power { min: 0.0, max: 2.0, power: TIME }, step: 0.0, centered: false, get: |s| s.attack, set: |s, v| s.attack = v },
    PadParam { name: "decay", label: "Decay", curve: Curve::Power { min: 0.0, max: 5.0, power: TIME }, step: 0.0, centered: false, get: |s| s.decay, set: |s, v| s.decay = v },
    PadParam { name: "sustain", label: "Sustain", curve: Curve::Linear { min: 0.0, max: 1.0 }, step: 0.0, centered: false, get: |s| s.sustain, set: |s, v| s.sustain = v },
    PadParam { name: "release", label: "Release", curve: Curve::Power { min: 0.001, max: 5.0, power: TIME }, step: 0.0, centered: false, get: |s| s.release, set: |s, v| s.release = v },
    PadParam { name: "fenvAmount", label: "Filter env amount", curve: Curve::Linear { min: -8.0, max: 8.0 }, step: 0.0, centered: true, get: |s| s.fenv_amount, set: |s, v| s.fenv_amount = v },
    PadParam { name: "fenvAttack", label: "Filter env attack", curve: Curve::Power { min: 0.0, max: 2.0, power: TIME }, step: 0.0, centered: false, get: |s| s.fenv_attack, set: |s, v| s.fenv_attack = v },
    PadParam { name: "fenvDecay", label: "Filter env decay", curve: Curve::Power { min: 0.0, max: 5.0, power: TIME }, step: 0.0, centered: false, get: |s| s.fenv_decay, set: |s, v| s.fenv_decay = v },
    PadParam { name: "fenvSustain", label: "Filter env sustain", curve: Curve::Linear { min: 0.0, max: 1.0 }, step: 0.0, centered: false, get: |s| s.fenv_sustain, set: |s, v| s.fenv_sustain = v },
    PadParam { name: "fenvRelease", label: "Filter env release", curve: Curve::Power { min: 0.001, max: 5.0, power: TIME }, step: 0.0, centered: false, get: |s| s.fenv_release, set: |s, v| s.fenv_release = v },
    PadParam { name: "velCutoff", label: "Velocity to cutoff", curve: Curve::Linear { min: -8.0, max: 8.0 }, step: 0.0, centered: true, get: |s| s.vel_cutoff, set: |s, v| s.vel_cutoff = v },
    PadParam { name: "velTime", label: "Velocity to time", curve: Curve::Linear { min: -1.0, max: 1.0 }, step: 0.0, centered: true, get: |s| s.vel_time, set: |s, v| s.vel_time = v },
];

pub const TEMPO: &str = "transport.tempo";
pub const SWING: &str = "sampler.swing";

/// Which pad a `sampler.pad.…` id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadRef {
    Selected,
    /// 0-based.
    Pad(usize),
}

/// Reads `sampler.pad.<selected|1..16>.<param>`.
pub fn parse_pad(id: &str) -> Option<(PadRef, &'static PadParam)> {
    let rest = id.strip_prefix("sampler.pad.")?;
    let (pad, name) = rest.split_once('.')?;
    let pad = match pad {
        "selected" => PadRef::Selected,
        n => {
            let n: usize = n.parse().ok()?;
            if n == 0 || n > PADS || n.to_string() != pad {
                return None;
            }
            PadRef::Pad(n - 1)
        }
    };
    Some((pad, PAD_PARAMS.iter().find(|p| p.name == name)?))
}

/// The id of a pad target.
pub fn pad_id(pad: PadRef, param: &PadParam) -> String {
    match pad {
        PadRef::Selected => format!("sampler.pad.selected.{}", param.name),
        PadRef::Pad(i) => format!("sampler.pad.{}.{}", i + 1, param.name),
    }
}

fn pad_info(pad: PadRef, p: &PadParam) -> TargetInfo {
    TargetInfo {
        id: pad_id(pad, p),
        label: p.label.into(),
        group: match pad {
            PadRef::Selected => "Selected pad".into(),
            PadRef::Pad(i) => format!("Pad {}", i + 1),
        },
        curve: p.curve,
        step: p.step,
        centered: p.centered,
    }
}

/// The tempo and swing, by id.
pub fn transport_info(id: &str) -> Option<TargetInfo> {
    match id {
        TEMPO => Some(TargetInfo {
            id: TEMPO.into(),
            label: "Tempo".into(),
            group: "Transport".into(),
            curve: Curve::Linear { min: 40.0, max: 300.0 },
            step: 1.0,
            centered: false,
        }),
        SWING => Some(TargetInfo {
            id: SWING.into(),
            label: "Swing".into(),
            group: "Transport".into(),
            curve: Curve::Linear { min: 0.0, max: 1.0 },
            step: 0.01,
            centered: false,
        }),
        _ => None,
    }
}

/// Every Sampler target id: the selected pad's, each pad's, swing and tempo.
pub fn sampler_ids() -> Vec<String> {
    let pads = std::iter::once(PadRef::Selected).chain((0..PADS).map(PadRef::Pad));
    let mut ids: Vec<String> = vec![TEMPO.into(), SWING.into()];
    for pad in pads {
        ids.extend(PAD_PARAMS.iter().map(|p| pad_id(pad, p)));
    }
    ids
}

/// The Sampler's targets. `focus` is the pad selected in the UI.
pub struct SamplerTargets {
    pub focus: Arc<AtomicUsize>,
}

impl SamplerTargets {
    fn pad(&self, r: PadRef) -> usize {
        match r {
            PadRef::Selected => self.focus.load(Ordering::Relaxed).min(PADS - 1),
            PadRef::Pad(i) => i,
        }
    }

    fn describe(id: &str) -> Option<TargetInfo> {
        match parse_pad(id) {
            Some((pad, p)) => Some(pad_info(pad, p)),
            None => transport_info(id),
        }
    }
}

impl Targets<tauri::AppHandle> for SamplerTargets {
    fn prefixes(&self) -> &'static [&'static str] {
        &["sampler", "transport"]
    }

    fn list(&self, _: &tauri::AppHandle) -> Vec<TargetInfo> {
        sampler_ids().iter().filter_map(|id| Self::describe(id)).collect()
    }

    fn info(&self, _: &tauri::AppHandle, id: &str) -> Option<TargetInfo> {
        Self::describe(id)
    }

    fn get(&self, app: &tauri::AppHandle, id: &str) -> Option<f64> {
        let c = sampler::controls(app);
        let v = match parse_pad(id) {
            Some((pad, p)) => (p.get)(&c.pads[self.pad(pad)]),
            None => match id {
                TEMPO => c.tempo,
                SWING => c.swing,
                _ => return None,
            },
        };
        Some(f64::from(v))
    }

    fn set(&self, app: &tauri::AppHandle, moves: &[Move], undo_key: &str) -> Result<(), String> {
        sampler::set_controls(app, undo_key, |c| {
            for m in moves {
                let v = m.value as f32;
                match parse_pad(&m.target) {
                    Some((pad, p)) => (p.set)(&mut c.pads[self.pad(pad)], v),
                    None if m.target == TEMPO => c.tempo = v,
                    None if m.target == SWING => c.swing = v,
                    None => {}
                }
            }
        })
        .map(|_| ())
    }

    /// While a knob turns: the settings, tempo and swing alone
    /// (`sampler:controls`), which the Inspector and the transport follow.
    fn changed(&self, app: &tauri::AppHandle) {
        use tauri::Emitter;
        let _ = app.emit("sampler:controls", sampler::controls(app));
    }

    /// The turn is over: the whole view once (undo, unsaved, everything else).
    fn settled(&self, app: &tauri::AppHandle) {
        sampler::publish_now(app);
    }
}

// ---- The output and the tracks ----

pub const MASTER: &str = "master.level";
const LEVEL: Curve = Curve::Fader { floor_db: -60.0, max_db: 6.0 };

fn db(gain: f32) -> f64 {
    if gain <= 0.0 { -60.0 } else { (20.0 * f64::from(gain).log10()).max(-60.0) }
}

fn gain(db: f64) -> f32 {
    if db <= -60.0 { 0.0 } else { 10f64.powf(db.min(6.0) / 20.0) as f32 }
}

/// The master level, in dB like its slider in the Audio dialog.
pub struct MasterTarget;

fn master_info() -> TargetInfo {
    TargetInfo { id: MASTER.into(), label: "Master level".into(), group: "Output".into(), curve: LEVEL, step: 0.1, centered: false }
}

impl Targets<tauri::AppHandle> for MasterTarget {
    fn prefixes(&self) -> &'static [&'static str] {
        &["master"]
    }

    fn list(&self, _: &tauri::AppHandle) -> Vec<TargetInfo> {
        vec![master_info()]
    }

    fn info(&self, _: &tauri::AppHandle, id: &str) -> Option<TargetInfo> {
        (id == MASTER).then(master_info)
    }

    fn get(&self, app: &tauri::AppHandle, id: &str) -> Option<f64> {
        (id == MASTER).then(|| db(crate::audio(app).lock().unwrap().engine.master()))
    }

    fn set(&self, app: &tauri::AppHandle, moves: &[Move], _: &str) -> Result<(), String> {
        if let Some(m) = moves.iter().rev().find(|m| m.target == MASTER) {
            crate::audio(app).lock().unwrap().engine.set_master(gain(m.value));
        }
        Ok(())
    }
}

/// Which track and which part of its mix a `track.<id>.<volume|pan>` id names.
pub fn parse_track(id: &str) -> Option<(TrackId, bool)> {
    let (n, what) = id.strip_prefix("track.")?.split_once('.')?;
    let num: u32 = n.parse().ok()?;
    if num.to_string() != n {
        return None; // "track.01.volume" isn't track 1's
    }
    match what {
        "volume" => Some((TrackId(num), true)),
        "pan" => Some((TrackId(num), false)),
        _ => None,
    }
}

fn track_info(id: TrackId, name: &str, volume: bool) -> TargetInfo {
    TargetInfo {
        id: format!("track.{}.{}", id.0, if volume { "volume" } else { "pan" }),
        label: if volume { "Volume" } else { "Pan" }.into(),
        group: format!("Track {} · {name}", id.0),
        curve: if volume { LEVEL } else { Curve::Linear { min: -1.0, max: 1.0 } },
        step: if volume { 0.1 } else { 0.0 },
        centered: !volume,
    }
}

/// Every track's volume (in dB, as a fader) and pan. The Mixer can take
/// these over, with its own controls, by moving this provider into its module.
pub struct TrackTargets;

impl Targets<tauri::AppHandle> for TrackTargets {
    fn prefixes(&self) -> &'static [&'static str] {
        &["track"]
    }

    fn list(&self, app: &tauri::AppHandle) -> Vec<TargetInfo> {
        let tracks = crate::audio(app).lock().unwrap().engine.tracks();
        tracks.iter().flat_map(|t| [track_info(t.id, &t.name, true), track_info(t.id, &t.name, false)]).collect()
    }

    fn info(&self, app: &tauri::AppHandle, id: &str) -> Option<TargetInfo> {
        let (track, volume) = parse_track(id)?;
        let name = crate::audio(app).lock().unwrap().engine.tracks().into_iter().find(|t| t.id == track)?.name;
        Some(track_info(track, &name, volume))
    }

    fn get(&self, app: &tauri::AppHandle, id: &str) -> Option<f64> {
        let (track, volume) = parse_track(id)?;
        let mix = crate::audio(app).lock().unwrap().engine.tracks().into_iter().find(|t| t.id == track)?.mix;
        Some(if volume { db(mix.volume) } else { f64::from(mix.pan) })
    }

    fn set(&self, app: &tauri::AppHandle, moves: &[Move], _: &str) -> Result<(), String> {
        let mut audio = crate::audio(app).lock().unwrap();
        let mixer = audio.engine.mixer();
        for m in moves {
            let Some((track, volume)) = parse_track(&m.target) else { continue };
            let Some(mut mix) = mixer.track(track).map(|t| t.mix) else { continue };
            if volume {
                mix.volume = gain(m.value);
            } else {
                mix.pan = (m.value as f32).clamp(-1.0, 1.0);
            }
            mixer.set_mix(track, mix)?;
        }
        Ok(())
    }

    /// A track's mix is saved with the project: once the fader stops, the
    /// view goes out again so the window's unsaved mark is right.
    fn settled(&self, app: &tauri::AppHandle) {
        sampler::publish_now(app);
    }
}
