//! The Sampler (Phase 2): the main window's pads, step sequencer, Library,
//! projects and export.
//!
//! The document (which sample is on which pad, its settings, the pattern,
//! tempo) lives here, behind a mutex. Each change goes to the engine's
//! atomics through the [`SamplerHandle`] and to every window as a
//! `sampler:state` event, so the main window and the Analyzer agree.
//! Lock order: the document, then the audio engine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::async_runtime::spawn_blocking;
use tauri::{AppHandle, Emitter, Manager};
use tunekore_analyzer::{decode_file, even, export_wav, onsets, peaks, slices, WavFormat, EXTENSIONS};
use tunekore_engine::{render_pattern, render_song, resample, CHAIN_MAX, PadParams, SamplerHandle, SamplerShared, Track, MAX_STEPS, PADS, PREVIEW};

use crate::Audio;

/// Waveform resolution per pad.
const PAD_PEAKS: usize = 160;
/// Exports are 48 kHz.
const EXPORT_RATE: u32 = 48_000;
/// The Library lists at most this many files from one folder.
const LIBRARY_LIMIT: usize = 20_000;
/// Patterns in a project: A to D.
pub const BANK: usize = 4;
/// Undo steps kept.
const UNDO_LIMIT: usize = 200;
/// Edits of the same kind this close together undo as one (a slider drag, painting steps).
const MERGE_WINDOW: Duration = Duration::from_millis(700);
/// Decoded files kept in memory, so previews and reloads are instant.
const CACHE_FILES: usize = 48;
/// Project files end in `.tunek`.
pub const PROJECT_EXT: &str = "tunek";
const FORMAT: &str = "tunekore-project";
const VERSION: u32 = 1;

/// When a switch of pattern, or of song mode, made while playing lands.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum PatternSwitch {
    /// At the end of the pattern: the next one starts exactly on the wrap.
    #[default]
    End,
    /// On the next step.
    Now,
}

/// A pad's settings as the UI and the project file see them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct PadSettings {
    /// -60 (off) to +6.
    pub gain_db: f32,
    pub pan: f32,
    /// Semitones, -24..24.
    pub pitch: f32,
    pub start: f32,
    pub end: f32,
    pub gate: bool,
    pub reverse: bool,
    pub mute: bool,
    /// 0 = none; pads in the same group 1..=8 cut each other off (open and closed hats).
    pub choke: u8,
    /// Amp envelope: seconds, seconds, level 0..1, seconds.
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
    /// 0 off, 1 low-pass, 2 high-pass, 3 band-pass.
    pub filter: u8,
    /// Hz.
    pub cutoff: f32,
    /// 0..1.
    pub resonance: f32,
}

impl Default for PadSettings {
    fn default() -> Self {
        Self {
            gain_db: 0.0,
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
            release: 0.004,
            filter: 0,
            cutoff: 20_000.0,
            resonance: 0.0,
        }
    }
}

impl PadSettings {
    fn params(&self) -> PadParams {
        let gain = if self.gain_db <= -60.0 { 0.0 } else { 10f32.powf(self.gain_db.min(6.0) / 20.0) };
        PadParams {
            gain,
            pan: self.pan,
            pitch: self.pitch,
            start: self.start,
            end: self.end,
            gate: self.gate,
            reverse: self.reverse,
            mute: self.mute,
            choke: self.choke.min(8),
            attack: self.attack,
            decay: self.decay,
            sustain: self.sustain,
            release: self.release,
            filter: self.filter,
            cutoff: self.cutoff,
            resonance: self.resonance,
        }
    }
}

struct Decoded {
    rate: u32,
    data: Track,
}

struct Sample {
    path: PathBuf,
    name: String,
    rate: u32,
    data: Track,
    peaks: Vec<f32>,
}

#[derive(Default, Clone)]
struct Pad {
    sample: Option<Arc<Sample>>,
    /// A sample the project names but that isn't on disk.
    missing: Option<PathBuf>,
    settings: PadSettings,
    /// This pad's steps in each pattern, as bits.
    patterns: [u64; BANK],
    /// How hard each step plays, per pattern, 255 = full. Empty until a velocity is set.
    velocity: Vec<[u8; MAX_STEPS]>,
    /// How likely each step is to play, per pattern, 255 = always. Empty until set.
    chance: Vec<[u8; MAX_STEPS]>,
    /// Hits per step (1..=4), per pattern. Empty until set.
    ratchet: Vec<[u8; MAX_STEPS]>,
}

impl Pad {
    fn chance(&self, pattern: usize, step: usize) -> u8 {
        self.chance.get(pattern).map_or(255, |v| v[step])
    }

    fn ratchet(&self, pattern: usize, step: usize) -> u8 {
        self.ratchet.get(pattern).map_or(1, |v| v[step].max(1))
    }

    fn set_extras(&mut self, pattern: usize, step: usize, chance: u8, ratchet: u8) {
        if self.chance.len() < BANK {
            self.chance.resize(BANK, [255; MAX_STEPS]);
        }
        if self.ratchet.len() < BANK {
            self.ratchet.resize(BANK, [1; MAX_STEPS]);
        }
        self.chance[pattern][step] = chance;
        self.ratchet[pattern][step] = ratchet.clamp(1, 4);
    }

    fn velocity(&self, pattern: usize, step: usize) -> u8 {
        self.velocity.get(pattern).map_or(255, |v| v[step])
    }

    fn set_velocity(&mut self, pattern: usize, step: usize, v: u8) {
        if self.velocity.len() < BANK {
            self.velocity.resize(BANK, [255; MAX_STEPS]);
        }
        self.velocity[pattern][step] = v;
    }
}

struct Doc {
    pads: Vec<Pad>,
    tempo: f32,
    swing: f32,
    steps: usize,
    /// The pattern that plays and is edited, 0..BANK.
    current: usize,
    /// Song mode: the patterns to play in order, and whether it plays the chain or loops one pattern.
    chain: Vec<u8>,
    song_mode: bool,
    song_loop: bool,
    /// When a switch made while playing lands.
    switch: PatternSwitch,
    path: Option<PathBuf>,
    dirty: bool,
    history: History,
    /// Bumped on every change, so autosave writes only when something changed.
    revision: u64,
}

impl Default for Doc {
    fn default() -> Self {
        Self {
            pads: (0..PADS).map(|_| Pad::default()).collect(),
            tempo: 120.0,
            swing: 0.0,
            steps: 16,
            current: 0,
            chain: Vec::new(),
            song_mode: false,
            song_loop: true,
            switch: PatternSwitch::End,
            path: None,
            dirty: false,
            history: History::default(),
            revision: 0,
        }
    }
}

/// The parts of the document that undo restores. Samples are shared, so a
/// snapshot costs a few hundred bytes.
#[derive(Clone)]
struct Snapshot {
    pads: Vec<Pad>,
    tempo: f32,
    swing: f32,
    steps: usize,
    current: usize,
    chain: Vec<u8>,
    song_mode: bool,
    song_loop: bool,
    switch: PatternSwitch,
}

#[derive(Default)]
struct History {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The kind and time of the last edit, to merge a run of them into one step.
    last: Option<(String, Instant)>,
}

impl Doc {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            pads: self.pads.clone(),
            tempo: self.tempo,
            swing: self.swing,
            steps: self.steps,
            current: self.current,
            chain: self.chain.clone(),
            song_mode: self.song_mode,
            song_loop: self.song_loop,
            switch: self.switch,
        }
    }

    /// Records `before` as an undo step for an edit of kind `key`, unless it
    /// continues a run of the same kind.
    fn checkpoint(&mut self, key: &str, before: Snapshot) {
        let now = Instant::now();
        // A recording take ("take:N") is one step however long it runs.
        let merge = matches!(&self.history.last, Some((k, t)) if k == key && (key.starts_with("take:") || now.duration_since(*t) < MERGE_WINDOW));
        self.history.last = Some((key.to_string(), now));
        self.dirty = true;
        self.revision += 1;
        if merge {
            return;
        }
        self.history.undo.push(before);
        if self.history.undo.len() > UNDO_LIMIT {
            self.history.undo.remove(0);
        }
        self.history.redo.clear();
    }
}

/// Decoded files by path, and the order they came in (oldest first).
type Cache = (HashMap<PathBuf, Arc<Decoded>>, Vec<PathBuf>);

pub struct SamplerState {
    doc: Mutex<Doc>,
    cache: Mutex<Cache>,
    handle: SamplerHandle,
    /// Counts recording takes, so each take undoes as one step.
    take: std::sync::atomic::AtomicU32,
}

impl SamplerState {
    pub fn new(handle: SamplerHandle) -> Self {
        let s = Self { doc: Mutex::default(), cache: Mutex::default(), handle, take: Default::default() };
        s.apply_all(&s.doc.lock().unwrap());
        s
    }

    fn apply_pad(&self, doc: &Doc, pad: usize) {
        let p = &doc.pads[pad];
        self.handle.set_pad(pad, &p.settings.params());
        // The engine holds every pattern, so song mode can move between them on the bar.
        for b in 0..BANK {
            self.handle.set_bank_pattern(b, pad, p.patterns[b]);
            for step in 0..MAX_STEPS {
                self.handle.set_bank_velocity(b, pad, step, p.velocity(b, step) as f32 / 255.0);
                self.handle.set_chance(b, pad, step, p.chance(b, step) as f32 / 255.0);
                self.handle.set_ratchet(b, pad, step, p.ratchet(b, step));
            }
        }
    }

    fn apply_all(&self, doc: &Doc) {
        for pad in 0..PADS {
            self.apply_pad(doc, pad);
        }
        self.handle.set_tempo(doc.tempo);
        self.handle.set_swing(doc.swing);
        self.handle.set_steps(doc.steps);
        self.handle.set_bank(doc.current);
        self.handle.set_chain(&doc.chain);
        self.handle.set_song_mode(doc.song_mode);
        self.handle.set_song_loop(doc.song_loop);
        self.handle.set_switch_now(doc.switch == PatternSwitch::Now);
    }

    /// Whether the document has changes that aren't saved.
    pub fn is_dirty(&self) -> bool {
        self.doc.lock().unwrap().dirty
    }

    /// Puts `snap` back, reloading only the pads whose sample changed.
    fn restore(&self, app: &AppHandle, doc: &mut Doc, snap: Snapshot) -> Result<(), String> {
        let mut a = audio(app).lock().unwrap();
        for (pad, (now, then)) in doc.pads.iter().zip(&snap.pads).enumerate() {
            let same = match (&now.sample, &then.sample) {
                (Some(x), Some(y)) => Arc::ptr_eq(x, y),
                (None, None) => true,
                _ => false,
            };
            if !same {
                self.handle.cut(pad);
                a.engine.load_sample(pad, then.sample.as_ref().map(|x| (x.rate, x.data.clone())))?;
            }
        }
        drop(a);
        doc.pads = snap.pads;
        doc.tempo = snap.tempo;
        doc.swing = snap.swing;
        doc.steps = snap.steps;
        doc.current = snap.current;
        doc.chain = snap.chain;
        doc.song_mode = snap.song_mode;
        doc.song_loop = snap.song_loop;
        doc.switch = snap.switch;
        doc.dirty = true;
        doc.revision += 1;
        doc.history.last = None;
        self.apply_all(doc);
        Ok(())
    }

    fn decode(&self, path: &Path) -> Result<Arc<Decoded>, String> {
        if let Some(d) = self.cache.lock().unwrap().0.get(path) {
            return Ok(d.clone());
        }
        let song = decode_file(path)?;
        if song.frames() < 2 {
            return Err(format!("{} has no audio", file_name(path)));
        }
        let d = Arc::new(Decoded { rate: song.sample_rate, data: song.channels });
        let mut cache = self.cache.lock().unwrap();
        let (map, order) = &mut *cache;
        if map.insert(path.to_path_buf(), d.clone()).is_none() {
            order.push(path.to_path_buf());
            if order.len() > CACHE_FILES {
                let old = order.remove(0);
                map.remove(&old);
            }
        }
        Ok(d)
    }
}

// ---- What the UI sees ----

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pads: Vec<PadView>,
    tempo: f32,
    swing: f32,
    steps: usize,
    project_name: String,
    project_path: Option<String>,
    dirty: bool,
    can_undo: bool,
    can_redo: bool,
    /// The pattern being edited, 0 = A. Outside song mode it is also the one
    /// that plays, from the next switch: a tick's `playing_pattern` says
    /// which plays now.
    pattern: usize,
    /// Which patterns have any steps.
    patterns_used: Vec<bool>,
    metronome: bool,
    recording: bool,
    chain: Vec<u8>,
    song_mode: bool,
    song_loop: bool,
    pattern_switch: PatternSwitch,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PadView {
    index: usize,
    name: Option<String>,
    path: Option<String>,
    missing: bool,
    seconds: f64,
    channels: usize,
    rate: u32,
    peaks: Vec<f32>,
    settings: PadSettings,
    /// One entry per step of the pattern's length.
    steps: Vec<bool>,
    /// How hard each step plays, 0..1, one per step.
    velocity: Vec<f32>,
    /// How likely each step is to play, 0..1.
    chance: Vec<f32>,
    /// Hits per step, 1..=4.
    ratchet: Vec<u8>,
}

fn view(doc: &Doc, live: &SamplerHandle) -> View {
    let pads = doc
        .pads
        .iter()
        .enumerate()
        .map(|(index, p)| {
            let s = p.sample.as_deref();
            PadView {
                index,
                name: s.map(|s| s.name.clone()).or_else(|| p.missing.as_deref().map(file_stem)),
                path: s.map(|s| s.path.display().to_string()).or_else(|| p.missing.as_ref().map(|m| m.display().to_string())),
                missing: p.missing.is_some(),
                seconds: s.map_or(0.0, |s| s.data[0].len() as f64 / s.rate as f64),
                channels: s.map_or(0, |s| s.data.len()),
                rate: s.map_or(0, |s| s.rate),
                peaks: s.map(|s| s.peaks.clone()).unwrap_or_default(),
                settings: p.settings,
                steps: (0..doc.steps).map(|i| p.patterns[doc.current] & (1u64 << i) != 0).collect(),
                velocity: (0..doc.steps).map(|i| p.velocity(doc.current, i) as f32 / 255.0).collect(),
                chance: (0..doc.steps).map(|i| p.chance(doc.current, i) as f32 / 255.0).collect(),
                ratchet: (0..doc.steps).map(|i| p.ratchet(doc.current, i)).collect(),
            }
        })
        .collect();
    View {
        pads,
        tempo: doc.tempo,
        swing: doc.swing,
        steps: doc.steps,
        project_name: doc.path.as_deref().map(file_stem).unwrap_or_else(|| "Untitled".into()),
        project_path: doc.path.as_ref().map(|p| p.display().to_string()),
        dirty: doc.dirty,
        can_undo: !doc.history.undo.is_empty(),
        can_redo: !doc.history.redo.is_empty(),
        pattern: doc.current,
        patterns_used: (0..BANK).map(|b| doc.pads.iter().any(|p| p.patterns[b] != 0)).collect(),
        metronome: live.metronome(),
        recording: live.is_recording(),
        chain: doc.chain.clone(),
        song_mode: doc.song_mode,
        song_loop: doc.song_loop,
        pattern_switch: doc.switch,
    }
}

fn state(app: &AppHandle) -> tauri::State<'_, SamplerState> {
    app.state::<SamplerState>()
}

fn audio(app: &AppHandle) -> &'static Mutex<Audio> {
    *app.state::<&'static Mutex<Audio>>().inner()
}

/// Sends the document to every window, and returns it.
fn publish(app: &AppHandle, doc: &Doc) -> View {
    let v = view(doc, &state(app).handle);
    let _ = app.emit("sampler:state", &v);
    v
}

/// Runs `f` on the document as an undoable edit of kind `key`, marks it
/// changed, and publishes it.
fn edit(app: &AppHandle, key: &str, f: impl FnOnce(&SamplerState, &mut Doc) -> Result<(), String>) -> Result<View, String> {
    let s = state(app);
    let mut doc = s.doc.lock().unwrap();
    let before = doc.snapshot();
    f(&s, &mut doc)?;
    doc.checkpoint(key, before);
    Ok(publish(app, &doc))
}

fn check_pad(pad: usize) -> Result<(), String> {
    if pad < PADS { Ok(()) } else { Err(format!("there is no pad {}", pad + 1)) }
}

/// Decodes `path` and puts it on `pad` with fresh settings, keeping the
/// pad's steps. Blocking: call it off the UI thread.
pub fn load_pad(app: &AppHandle, pad: usize, path: &Path, settings: PadSettings) -> Result<(), String> {
    check_pad(pad)?;
    let s = state(app);
    let d = s.decode(path)?;
    let sample = Arc::new(Sample {
        path: path.to_path_buf(),
        name: file_stem(path),
        rate: d.rate,
        data: d.data.clone(),
        peaks: peaks(&d.data, PAD_PEAKS),
    });
    let mut doc = s.doc.lock().unwrap();
    let before = doc.snapshot();
    s.handle.cut(pad);
    audio(app).lock().unwrap().engine.load_sample(pad, Some((d.rate, d.data.clone())))?;
    let p = &mut doc.pads[pad];
    p.sample = Some(sample);
    p.missing = None;
    p.settings = settings;
    s.apply_pad(&doc, pad);
    // A run of loads (a Finder drop, a chop) undoes as one step.
    doc.checkpoint("load", before);
    Ok(())
}

/// Sends the current document to every window.
pub fn publish_now(app: &AppHandle) {
    let s = state(app);
    let doc = s.doc.lock().unwrap();
    publish(app, &doc);
}

/// The first pad with nothing on it.
pub fn first_free_pad(app: &AppHandle) -> Option<usize> {
    let s = state(app);
    let doc = s.doc.lock().unwrap();
    doc.pads.iter().position(|p| p.sample.is_none() && p.missing.is_none())
}

// ---- Commands: pads and pattern ----

#[tauri::command]
pub fn sampler_state(app: AppHandle) -> View {
    let s = state(&app);
    let doc = s.doc.lock().unwrap();
    view(&doc, &s.handle)
}

#[tauri::command]
pub async fn sampler_load(pad: usize, path: String, app: AppHandle) -> Result<View, String> {
    let a = app.clone();
    spawn_blocking(move || load_pad(&a, pad, Path::new(&path), PadSettings::default())).await.map_err(|e| e.to_string())??;
    let s = state(&app);
    let doc = s.doc.lock().unwrap();
    Ok(publish(&app, &doc))
}

#[tauri::command]
pub fn sampler_clear(pad: usize, app: AppHandle) -> Result<View, String> {
    check_pad(pad)?;
    edit(&app, &format!("clear:{pad}"), |s, doc| {
        s.handle.cut(pad);
        audio(&app).lock().unwrap().engine.load_sample(pad, None)?;
        doc.pads[pad] = Pad::default();
        s.apply_pad(doc, pad);
        Ok(())
    })
}

/// Swaps two pads: samples, settings and steps.
#[tauri::command]
pub fn sampler_move(from: usize, to: usize, app: AppHandle) -> Result<View, String> {
    check_pad(from)?;
    check_pad(to)?;
    if from == to {
        return Ok(sampler_state(app));
    }
    edit(&app, &format!("move:{from}:{to}"), |s, doc| {
        doc.pads.swap(from, to);
        let mut a = audio(&app).lock().unwrap();
        for pad in [from, to] {
            s.handle.cut(pad);
            let data = doc.pads[pad].sample.as_ref().map(|x| (x.rate, x.data.clone()));
            a.engine.load_sample(pad, data)?;
            s.apply_pad(doc, pad);
        }
        Ok(())
    })
}

#[tauri::command]
pub fn sampler_settings(pad: usize, settings: PadSettings, app: AppHandle) -> Result<View, String> {
    check_pad(pad)?;
    edit(&app, &format!("settings:{pad}"), |s, doc| {
        doc.pads[pad].settings = settings;
        s.apply_pad(doc, pad);
        Ok(())
    })
}

#[tauri::command]
pub fn sampler_step(pad: usize, step: usize, on: bool, app: AppHandle) -> Result<View, String> {
    check_pad(pad)?;
    if step >= MAX_STEPS {
        return Err(format!("there is no step {}", step + 1));
    }
    edit(&app, "steps", |s, doc| {
        let bit = 1u64 << step;
        let cur = doc.current;
        let p = &mut doc.pads[pad].patterns[cur];
        *p = if on { *p | bit } else { *p & !bit };
        s.apply_pad(doc, pad);
        Ok(())
    })
}

/// Clears one pad's steps, or every pad's when `pad` is `None`.
#[tauri::command]
pub fn sampler_clear_steps(pad: Option<usize>, app: AppHandle) -> Result<View, String> {
    edit(&app, &format!("clear-steps:{pad:?}"), |s, doc| {
        for i in 0..PADS {
            if pad.is_none_or(|p| p == i) {
                let cur = doc.current;
                doc.pads[i].patterns[cur] = 0;
                s.apply_pad(doc, i);
            }
        }
        Ok(())
    })
}

#[tauri::command]
pub fn sampler_transport(tempo: f32, swing: f32, steps: usize, app: AppHandle) -> Result<View, String> {
    edit(&app, "transport", |s, doc| {
        doc.tempo = tempo.clamp(40.0, 300.0);
        doc.swing = swing.clamp(0.0, 1.0);
        doc.steps = steps.clamp(1, MAX_STEPS);
        s.handle.set_tempo(doc.tempo);
        s.handle.set_swing(doc.swing);
        s.handle.set_steps(doc.steps);
        Ok(())
    })
}

/// How hard one step plays, 0..1, in the current pattern.
#[tauri::command]
pub fn sampler_velocity(pad: usize, step: usize, velocity: f32, app: AppHandle) -> Result<View, String> {
    check_pad(pad)?;
    if step >= MAX_STEPS {
        return Err(format!("there is no step {}", step + 1));
    }
    edit(&app, "velocity", |s, doc| {
        let cur = doc.current;
        doc.pads[pad].set_velocity(cur, step, (velocity.clamp(0.0, 1.0) * 255.0).round() as u8);
        s.apply_pad(doc, pad);
        Ok(())
    })
}

/// Turns the click on or off. A live setting, not part of the project.
#[tauri::command]
pub fn sampler_metronome(on: bool, app: AppHandle) -> View {
    let s = state(&app);
    s.handle.set_metronome(on);
    let doc = s.doc.lock().unwrap();
    publish(&app, &doc)
}

/// Arms or disarms recording: pads played while the pattern runs are written into its steps.
#[tauri::command]
pub fn sampler_record(on: bool, app: AppHandle) -> View {
    let s = state(&app);
    if on && !s.handle.is_recording() {
        s.take.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    s.handle.set_recording(on);
    let doc = s.doc.lock().unwrap();
    publish(&app, &doc)
}

/// Copies steps the engine recorded into the document, as one undo step per take.
fn sync_recorded(app: &AppHandle) {
    let s = state(app);
    let mut doc = s.doc.lock().unwrap();
    let before = doc.snapshot();
    let cur = doc.current;
    let mut changed = false;
    for pad in 0..PADS {
        let live = s.handle.bank_pattern(cur, pad);
        let added = live & !doc.pads[pad].patterns[cur];
        if doc.pads[pad].patterns[cur] != live {
            doc.pads[pad].patterns[cur] = live;
            changed = true;
        }
        // Recorded hits carry how hard they were played.
        for step in (0..MAX_STEPS).filter(|i| added & (1u64 << i) != 0) {
            let v = (s.handle.bank_velocity(cur, pad, step) * 255.0).round() as u8;
            doc.pads[pad].set_velocity(cur, step, v);
        }
    }
    if changed {
        let take = s.take.load(std::sync::atomic::Ordering::Relaxed);
        doc.checkpoint(&format!("take:{take}"), before);
        publish(app, &doc);
    }
}

/// A step's chance to play (0..1) and its ratchet (1..=4 hits), in the current pattern.
#[tauri::command]
pub fn sampler_step_extras(pad: usize, step: usize, chance: f32, ratchet: u8, app: AppHandle) -> Result<View, String> {
    check_pad(pad)?;
    if step >= MAX_STEPS {
        return Err(format!("there is no step {}", step + 1));
    }
    edit(&app, &format!("extras:{pad}:{step}"), |s, doc| {
        let cur = doc.current;
        doc.pads[pad].set_extras(cur, step, (chance.clamp(0.0, 1.0) * 255.0).round() as u8, ratchet);
        s.apply_pad(doc, pad);
        Ok(())
    })
}

/// Song mode: `chain` lists the patterns (0 = A) to play in order; with
/// `song_mode` on, play runs through them, looping at the end if `song_loop`.
#[tauri::command]
pub fn sampler_song(chain: Vec<u8>, song_mode: bool, song_loop: bool, app: AppHandle) -> Result<View, String> {
    edit(&app, "song", |s, doc| {
        doc.chain = chain.into_iter().filter(|&b| (b as usize) < BANK).take(CHAIN_MAX).collect();
        doc.song_mode = song_mode;
        doc.song_loop = song_loop;
        s.apply_all(doc);
        Ok(())
    })
}

/// Switches to pattern `index` (0 = A). It shows for editing at once; while
/// playing, it starts at the end of the pattern playing, or on the next step
/// with the switch set to now.
#[tauri::command]
pub fn sampler_pattern(index: usize, app: AppHandle) -> Result<View, String> {
    if index >= BANK {
        return Err(format!("there is no pattern {index}"));
    }
    edit(&app, "pattern", |s, doc| {
        doc.current = index;
        s.apply_all(doc);
        Ok(())
    })
}

/// When a pattern or song mode switch made while playing lands: at the end of
/// the pattern, or on the next step. Saved with the project.
#[tauri::command]
pub fn sampler_switch(mode: PatternSwitch, app: AppHandle) -> Result<View, String> {
    edit(&app, "switch", |s, doc| {
        doc.switch = mode;
        s.apply_all(doc);
        Ok(())
    })
}

/// Copies pattern `from` over pattern `to` and switches to it.
#[tauri::command]
pub fn sampler_copy_pattern(from: usize, to: usize, app: AppHandle) -> Result<View, String> {
    if from >= BANK || to >= BANK {
        return Err("there are only four patterns".into());
    }
    edit(&app, &format!("copy:{from}:{to}"), |s, doc| {
        for p in doc.pads.iter_mut() {
            p.patterns[to] = p.patterns[from];
            for step in 0..MAX_STEPS {
                let (v, c, r) = (p.velocity(from, step), p.chance(from, step), p.ratchet(from, step));
                p.set_velocity(to, step, v);
                p.set_extras(to, step, c, r);
            }
        }
        doc.current = to;
        s.apply_all(doc);
        Ok(())
    })
}

/// Steps back one edit.
#[tauri::command]
pub fn sampler_undo(app: AppHandle) -> Result<View, String> {
    let s = state(&app);
    let mut doc = s.doc.lock().unwrap();
    let Some(snap) = doc.history.undo.pop() else { return Ok(view(&doc, &s.handle)) };
    let now = doc.snapshot();
    doc.history.redo.push(now);
    s.restore(&app, &mut doc, snap)?;
    Ok(publish(&app, &doc))
}

/// Steps forward again after an undo.
#[tauri::command]
pub fn sampler_redo(app: AppHandle) -> Result<View, String> {
    let s = state(&app);
    let mut doc = s.doc.lock().unwrap();
    let Some(snap) = doc.history.redo.pop() else { return Ok(view(&doc, &s.handle)) };
    let now = doc.snapshot();
    doc.history.undo.push(now);
    s.restore(&app, &mut doc, snap)?;
    Ok(publish(&app, &doc))
}

/// Starts the pattern from step 1, or stops it.
#[tauri::command]
pub async fn sampler_play(playing: bool, app: AppHandle) -> Result<(), String> {
    if playing {
        let a = app.clone();
        spawn_blocking(move || audio(&a).lock().unwrap().ensure_open()).await.map_err(|e| e.to_string())??;
    }
    state(&app).handle.set_playing(playing);
    Ok(())
}

/// Plays a pad, as if it were pressed. Touches only atomics.
#[tauri::command]
pub fn sampler_trigger(pad: usize, app: AppHandle) {
    if pad < PADS {
        state(&app).handle.trigger(pad);
    }
}

#[tauri::command]
pub fn sampler_release(pad: usize, app: AppHandle) {
    if pad < PADS {
        state(&app).handle.release(pad);
    }
}

#[derive(Serialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Tick {
    playing: bool,
    step: Option<usize>,
    played: Vec<u32>,
    /// Which chain entry the song is on, in song mode.
    chain_position: Option<usize>,
    /// The pattern playing now. While a switch waits for the end of the
    /// pattern it differs from the view's `pattern`.
    playing_pattern: Option<usize>,
    /// A song pass is playing; it stays true after song mode is turned off,
    /// until that pass ends.
    song_playing: bool,
}

/// Sends the step and each pad's play count to the UI about 30 times a
/// second while anything changes.
pub fn spawn_tick(app: AppHandle, handle: SamplerHandle) {
    std::thread::spawn(move || {
        let mut last: Option<Tick> = None;
        let mut recorded = handle.recorded();
        loop {
            std::thread::sleep(Duration::from_millis(33));
            let r = handle.recorded();
            if r != recorded {
                recorded = r;
                sync_recorded(&app);
            }
            let now = Tick {
                playing: handle.is_playing(),
                step: handle.current_step(),
                played: (0..PADS).map(|p| handle.played(p)).collect(),
                chain_position: handle.chain_position(),
                playing_pattern: handle.playing_bank(),
                song_playing: handle.song_playing(),
            };
            if last.as_ref() != Some(&now) {
                let _ = app.emit("sampler:tick", &now);
                last = Some(now);
            }
        }
    });
}

// ---- Library ----

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryItem {
    path: String,
    name: String,
    /// The folder it is in, relative to the Library's root ("" at the top).
    folder: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryScan {
    root: String,
    items: Vec<LibraryItem>,
    truncated: bool,
}

/// Lists the audio files under `dir`, skipping hidden files and plugin or app bundles.
pub fn scan(dir: &Path) -> Result<LibraryScan, String> {
    if !dir.is_dir() {
        return Err(format!("{} is not a folder", dir.display()));
    }
    let mut items = Vec::new();
    let mut truncated = false;
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    'walk: while let Some((d, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                let bundle = [".app", ".vst3", ".clap", ".component", ".bundle"].iter().any(|b| name.ends_with(b));
                if !bundle && depth < 12 {
                    stack.push((path, depth + 1));
                }
            } else if is_audio(&path) {
                if items.len() == LIBRARY_LIMIT {
                    truncated = true;
                    break 'walk;
                }
                let folder = path
                    .parent()
                    .and_then(|p| p.strip_prefix(dir).ok())
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                items.push(LibraryItem { name: file_stem(&path), path: path.display().to_string(), folder });
            }
        }
    }
    items.sort_by(|a, b| {
        (a.folder.to_lowercase(), a.name.to_lowercase()).cmp(&(b.folder.to_lowercase(), b.name.to_lowercase()))
    });
    Ok(LibraryScan { root: dir.display().to_string(), items, truncated })
}

fn is_audio(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.contains(&e.to_lowercase().as_str()))
}

#[tauri::command]
pub async fn library_scan(dir: String) -> Result<LibraryScan, String> {
    spawn_blocking(move || scan(Path::new(&dir))).await.map_err(|e| e.to_string())?
}

/// The folder the Library opens on first: Splice's library (wherever
/// Splice is set to keep it, then `~/Splice`, then a `Splice` folder at the
/// top of any mounted drive), then tuneKore's own samples (where stems are
/// chopped to), then Music. A Splice folder opens at its `sounds` folder.
#[tauri::command]
pub fn library_default(app: AppHandle) -> Option<String> {
    let home = app.path().home_dir().ok()?;
    let music = app.path().audio_dir().unwrap_or_else(|_| home.join("Music"));
    let drives: Vec<PathBuf> = std::fs::read_dir("/Volumes")
        .map(|d| d.flatten().map(|e| e.path().join("Splice")).collect())
        .unwrap_or_default();
    splice_configured(&home)
        .into_iter()
        .chain(std::iter::once(home.join("Splice")))
        .chain(drives)
        .find(|p| p.is_dir())
        .map(|p| if p.join("sounds").is_dir() { p.join("sounds") } else { p })
        .or_else(|| [music.join("tuneKore/Samples"), music].into_iter().find(|p| p.is_dir()))
        .map(|p| p.display().to_string())
}

/// The folder Splice is set to keep its library in. Splice writes it only
/// to its helper log, as `"splice_folder":"<path>"`, each time it starts.
fn splice_configured(home: &Path) -> Option<PathBuf> {
    use std::io::{Read, Seek, SeekFrom};
    let log = home.join("Library/Application Support/com.splice.Splice/logs/log-helper-core.log");
    let mut f = std::fs::File::open(log).ok()?;
    // The setting is logged at every start, so the last few megabytes are enough.
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(4 << 20))).ok()?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    last_splice_folder(&String::from_utf8_lossy(&bytes))
}

fn last_splice_folder(log: &str) -> Option<PathBuf> {
    const KEY: &str = "\"splice_folder\":\"";
    let start = log.rfind(KEY)? + KEY.len();
    let end = log[start..].find('"')?;
    let path = log[start..start + end].replace("\\/", "/");
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Plays a file from the Library on the preview slot.
#[tauri::command]
pub async fn library_preview(path: String, app: AppHandle) -> Result<(), String> {
    let a = app.clone();
    spawn_blocking(move || -> Result<(), String> {
        let s = state(&a);
        let d = s.decode(Path::new(&path))?;
        s.handle.cut(PREVIEW);
        let mut audio = audio(&a).lock().unwrap();
        audio.ensure_open()?;
        audio.engine.load_sample(PREVIEW, Some((d.rate, d.data.clone())))?;
        drop(audio);
        // The sample reaches the audio thread on its next block.
        std::thread::sleep(Duration::from_millis(15));
        s.handle.trigger(PREVIEW);
        s.handle.release(PREVIEW);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn library_stop(app: AppHandle) {
    state(&app).handle.cut(PREVIEW);
}

// ---- Projects ----

#[derive(Serialize, Deserialize)]
struct ProjectFile {
    format: String,
    version: u32,
    tempo: f32,
    swing: f32,
    steps: usize,
    /// The pattern that was showing, 0 = A.
    #[serde(default)]
    current: usize,
    /// Song mode's chain of patterns (0 = A), and its switches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    chain: Vec<u8>,
    #[serde(default)]
    song_mode: bool,
    #[serde(default = "yes")]
    song_loop: bool,
    /// When a switch made while playing lands: "end" (of the pattern) or
    /// "now" (the next step). Projects from before it open with "end".
    #[serde(default)]
    pattern_switch: PatternSwitch,
    pads: Vec<PadFile>,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Default)]
struct PadFile {
    /// Relative to the project file when it sits under the project's folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sample: Option<String>,
    #[serde(default)]
    settings: PadSettings,
    /// This pad's steps in patterns A to D, one character per step: `x` plays, `.` rests.
    #[serde(default)]
    patterns: Vec<String>,
    /// How hard each step plays (0..255), per pattern; left out while every step is full.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    velocity: Vec<Vec<u8>>,
    /// Chance per step (255 = always), per pattern; left out while every step always plays.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    chance: Vec<Vec<u8>>,
    /// Hits per step (1..=4), per pattern; left out while every step plays once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ratchet: Vec<Vec<u8>>,
}

fn to_file(doc: &Doc, project: &Path) -> ProjectFile {
    let base = project.parent().unwrap_or(Path::new(""));
    let pads = doc
        .pads
        .iter()
        .map(|p| {
            let path = p.sample.as_ref().map(|s| s.path.clone()).or_else(|| p.missing.clone());
            PadFile {
                sample: path.map(|path| match path.strip_prefix(base) {
                    Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
                    Err(_) => path.display().to_string(),
                }),
                settings: p.settings,
                patterns: p
                    .patterns
                    .iter()
                    .map(|bits| {
                        (0..MAX_STEPS)
                            .map(|i| if bits & (1u64 << i) != 0 { 'x' } else { '.' })
                            .collect::<String>()
                            .trim_end_matches('.')
                            .to_string()
                    })
                    .collect(),
                velocity: if p.velocity.iter().flatten().all(|&v| v == 255) {
                    Vec::new()
                } else {
                    p.velocity.iter().map(|v| v.to_vec()).collect()
                },
                chance: if p.chance.iter().flatten().all(|&v| v == 255) {
                    Vec::new()
                } else {
                    p.chance.iter().map(|v| v.to_vec()).collect()
                },
                ratchet: if p.ratchet.iter().flatten().all(|&v| v <= 1) {
                    Vec::new()
                } else {
                    p.ratchet.iter().map(|v| v.to_vec()).collect()
                },
            }
        })
        .collect();
    ProjectFile {
        format: FORMAT.into(),
        version: VERSION,
        tempo: doc.tempo,
        swing: doc.swing,
        steps: doc.steps,
        current: doc.current,
        chain: doc.chain.clone(),
        song_mode: doc.song_mode,
        song_loop: doc.song_loop,
        pattern_switch: doc.switch,
        pads,
    }
}

fn pattern_bits(s: &str) -> u64 {
    s.chars().take(MAX_STEPS).enumerate().filter(|(_, c)| *c == 'x' || *c == 'X').fold(0, |b, (i, _)| b | (1u64 << i))
}

#[tauri::command]
pub fn project_new(app: AppHandle) -> Result<View, String> {
    let s = state(&app);
    let mut doc = s.doc.lock().unwrap();
    s.handle.set_playing(false);
    let mut a = audio(&app).lock().unwrap();
    for pad in 0..PADS {
        s.handle.cut(pad);
        a.engine.load_sample(pad, None)?;
    }
    drop(a);
    *doc = Doc::default();
    s.apply_all(&doc);
    clear_autosave(&app);
    Ok(publish(&app, &doc))
}

#[tauri::command]
pub async fn project_open(path: String, app: AppHandle) -> Result<View, String> {
    let a = app.clone();
    let view = spawn_blocking(move || {
        let project = PathBuf::from(&path);
        load_project(&a, &project, Some(project.clone()), false)
    })
    .await
    .map_err(|e| e.to_string())??;
    clear_autosave(&app);
    Ok(view)
}

/// Reads a project file into the document. `as_path` is the project it
/// belongs to (the file itself, or the original project for a recovered
/// autosave); `dirty` marks it unsaved. Blocking.
fn load_project(a: &AppHandle, project: &Path, as_path: Option<PathBuf>, dirty: bool) -> Result<View, String> {
    let text = std::fs::read_to_string(project).map_err(|e| format!("can't read {}: {e}", project.display()))?;
    let mut file: ProjectFile = serde_json::from_str(&text).map_err(|e| format!("not a tuneKore project ({e})"))?;
    if file.format != FORMAT {
        return Err("not a tuneKore project".into());
    }
    if file.version > VERSION {
        return Err("this project was saved by a newer tuneKore".into());
    }
    let s = state(a);
    let base = project.parent().unwrap_or(Path::new("")).to_path_buf();
    // Decode before taking the locks.
    let mut pads: Vec<Pad> = Vec::with_capacity(PADS);
    let listed = std::mem::take(&mut file.pads);
    for pf in listed.into_iter().chain(std::iter::repeat_with(PadFile::default)).take(PADS) {
        let mut patterns = [0u64; BANK];
        for (b, text) in pf.patterns.iter().take(BANK).enumerate() {
            patterns[b] = pattern_bits(text);
        }
        let mut pad = Pad { settings: pf.settings, patterns, ..Default::default() };
        for (b, steps) in pf.velocity.iter().take(BANK).enumerate() {
            for (i, &v) in steps.iter().take(MAX_STEPS).enumerate() {
                pad.set_velocity(b, i, v);
            }
        }
        for b in 0..BANK.min(pf.chance.len().max(pf.ratchet.len())) {
            for i in 0..MAX_STEPS {
                let c = pf.chance.get(b).and_then(|v| v.get(i)).copied().unwrap_or(255);
                let r = pf.ratchet.get(b).and_then(|v| v.get(i)).copied().unwrap_or(1);
                pad.set_extras(b, i, c, r);
            }
        }
        if let Some(rel) = &pf.sample {
            let p = Path::new(rel);
            let full = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
            match s.decode(&full) {
                Ok(d) => {
                    pad.sample = Some(Arc::new(Sample {
                        name: file_stem(&full),
                        rate: d.rate,
                        peaks: peaks(&d.data, PAD_PEAKS),
                        data: d.data.clone(),
                        path: full,
                    }))
                }
                Err(_) => pad.missing = Some(full),
            }
        }
        pads.push(pad);
    }
    let mut doc = s.doc.lock().unwrap();
    s.handle.set_playing(false);
    let mut au = audio(a).lock().unwrap();
    for (i, pad) in pads.iter().enumerate() {
        s.handle.cut(i);
        au.engine.load_sample(i, pad.sample.as_ref().map(|x| (x.rate, x.data.clone())))?;
    }
    drop(au);
    *doc = Doc {
        pads,
        tempo: file.tempo.clamp(40.0, 300.0),
        swing: file.swing.clamp(0.0, 1.0),
        steps: file.steps.clamp(1, MAX_STEPS),
        current: file.current.min(BANK - 1),
        chain: file.chain.iter().copied().filter(|&b| (b as usize) < BANK).take(CHAIN_MAX).collect(),
        song_mode: file.song_mode,
        song_loop: file.song_loop,
        switch: file.pattern_switch,
        path: as_path,
        dirty,
        history: History::default(),
        revision: 1,
    };
    s.apply_all(&doc);
    Ok(publish(a, &doc))
}

/// Saves to `path` (as `.tunek`), or to the project's own file when `path` is `None`.
#[tauri::command]
pub fn project_save(path: Option<String>, app: AppHandle) -> Result<View, String> {
    let s = state(&app);
    let mut doc = s.doc.lock().unwrap();
    let mut project = path.map(PathBuf::from).or_else(|| doc.path.clone()).ok_or("choose where to save the project")?;
    if project.extension().and_then(|e| e.to_str()) != Some(PROJECT_EXT) {
        project.set_extension(PROJECT_EXT);
    }
    let json = serde_json::to_string_pretty(&to_file(&doc, &project)).map_err(|e| e.to_string())?;
    let tmp = project.with_extension(format!("{PROJECT_EXT}.part"));
    std::fs::write(&tmp, json + "\n").map_err(|e| format!("can't write {}: {e}", project.display()))?;
    std::fs::rename(&tmp, &project).map_err(|e| e.to_string())?;
    doc.path = Some(project);
    doc.dirty = false;
    clear_autosave(&app);
    Ok(publish(&app, &doc))
}

// ---- Autosave and recovery ----

/// How often unsaved work is written to the recovery file.
const AUTOSAVE_EVERY: Duration = Duration::from_secs(20);

fn autosave_dir(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("autosave"))
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recovery {
    project_name: String,
    project_path: Option<String>,
    /// Unix seconds.
    saved_at: u64,
}

/// Writes unsaved work to the app's data folder every 20 seconds while it
/// changes, so a crash loses at most that much. Saving clears it.
pub fn spawn_autosave(app: AppHandle) {
    std::thread::spawn(move || {
        let mut written = 0u64;
        loop {
            std::thread::sleep(AUTOSAVE_EVERY);
            let Some(dir) = autosave_dir(&app) else { continue };
            let file = dir.join("recovery.tunek");
            let snapshot = {
                let s = state(&app);
                let doc = s.doc.lock().unwrap();
                if !doc.dirty || doc.revision == written {
                    None
                } else {
                    let meta = Recovery {
                        project_name: doc.path.as_deref().map(file_stem).unwrap_or_else(|| "Untitled".into()),
                        project_path: doc.path.as_ref().map(|p| p.display().to_string()),
                        saved_at: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_secs()),
                    };
                    Some((serde_json::to_string_pretty(&to_file(&doc, &file)), serde_json::to_string(&meta), doc.revision))
                }
            };
            let Some((Ok(json), Ok(meta), rev)) = snapshot else { continue };
            let write = || -> std::io::Result<()> {
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join("recovery.tunek.part"), json)?;
                std::fs::rename(dir.join("recovery.tunek.part"), &file)?;
                std::fs::write(dir.join("recovery.json"), meta)
            };
            if write().is_ok() {
                written = rev;
            }
        }
    });
}

/// Removes the recovery files: the work is saved, or the user chose to drop it.
pub fn clear_autosave(app: &AppHandle) {
    if let Some(dir) = autosave_dir(app) {
        let _ = std::fs::remove_file(dir.join("recovery.tunek"));
        let _ = std::fs::remove_file(dir.join("recovery.json"));
    }
}

/// Unsaved work left by a session that didn't close normally, if any.
#[tauri::command]
pub fn autosave_check(app: AppHandle) -> Option<Recovery> {
    let dir = autosave_dir(&app)?;
    if !dir.join("recovery.tunek").exists() {
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(dir.join("recovery.json")).ok()?).ok()
}

/// Loads the unsaved work, marked unsaved, under its original project's name.
#[tauri::command]
pub async fn autosave_recover(app: AppHandle) -> Result<View, String> {
    let meta = autosave_check(app.clone()).ok_or("there is nothing to recover")?;
    let dir = autosave_dir(&app).ok_or("no data folder")?;
    let a = app.clone();
    spawn_blocking(move || load_project(&a, &dir.join("recovery.tunek"), meta.project_path.map(PathBuf::from), true))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn autosave_discard(app: AppHandle) {
    clear_autosave(&app);
}

// ---- Export ----

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Exported {
    path: String,
    seconds: f64,
    format: WavFormat,
}

/// What a render needs, copied out of the document so the render runs without its lock.
struct RenderJob {
    samples: Vec<Option<Arc<Sample>>>,
    shared: SamplerShared,
    project: String,
    /// Pad names, for stem file names.
    names: Vec<String>,
    /// Render the song chain once instead of looping the current pattern.
    song: bool,
}

/// Copies the document for a render. With `song`, the render is the song
/// mode chain played once; otherwise `loops` passes of the current pattern.
fn render_job(app: &AppHandle, song: bool) -> Result<RenderJob, String> {
    let s = state(app);
    let doc = s.doc.lock().unwrap();
    let banks: Vec<usize> = if song { doc.chain.iter().map(|&b| b as usize).collect() } else { vec![doc.current] };
    if song && banks.is_empty() {
        return Err("the song is empty: add patterns to it first".into());
    }
    if doc.pads.iter().all(|p| p.sample.is_none() || banks.iter().all(|&b| p.patterns[b] == 0)) {
        return Err("there is nothing to play yet: add steps first".into());
    }
    let shared = SamplerShared::default();
    for (i, p) in doc.pads.iter().enumerate() {
        shared.set_pad(i, &p.settings.params());
        for b in 0..BANK {
            shared.set_bank_pattern(b, i, p.patterns[b]);
            for step in 0..MAX_STEPS {
                shared.set_bank_velocity(b, i, step, p.velocity(b, step) as f32 / 255.0);
                shared.set_chance(b, i, step, p.chance(b, step) as f32 / 255.0);
                shared.set_ratchet(b, i, step, p.ratchet(b, step));
            }
        }
    }
    shared.set_bank(doc.current);
    shared.set_chain(&doc.chain);
    shared.set_tempo(doc.tempo);
    shared.set_swing(doc.swing);
    shared.set_steps(doc.steps);
    Ok(RenderJob {
        samples: doc.pads.iter().map(|p| p.sample.clone()).collect(),
        shared,
        project: doc.path.as_deref().map(file_stem).unwrap_or_else(|| "Untitled".into()),
        names: doc.pads.iter().map(|p| p.sample.as_ref().map(|x| x.name.clone()).unwrap_or_default()).collect(),
        song,
    })
}

/// The job's samples at the export rate.
fn tracks(job: &RenderJob) -> Result<Vec<Option<Track>>, String> {
    let mut tracks: Vec<Option<Track>> = Vec::with_capacity(PADS);
    for s in &job.samples {
        tracks.push(match s {
            None => None,
            Some(s) if s.rate == EXPORT_RATE => Some(s.data.clone()),
            Some(s) => Some(Arc::new(resample(&s.data, s.rate, EXPORT_RATE)?)),
        });
    }
    Ok(tracks)
}

/// Renders `loops` passes of the pattern at 48 kHz. Blocking.
fn render(job: &RenderJob, loops: usize) -> Result<Vec<Vec<f32>>, String> {
    Ok(render_with(job, &tracks(job)?, loops))
}

fn render_with(job: &RenderJob, tracks: &[Option<Track>], loops: usize) -> Vec<Vec<f32>> {
    if job.song {
        render_song(tracks, &job.shared, EXPORT_RATE)
    } else {
        render_pattern(tracks, &job.shared, EXPORT_RATE, loops.clamp(1, 64))
    }
}

/// A file name with the characters macOS and Windows reject replaced.
fn safe(name: &str) -> String {
    name.chars().map(|c| if "/\\:*?\"<>|".contains(c) { '-' } else { c }).collect()
}

/// Renders each pad that plays in the pattern to its own 48 kHz WAV in
/// `dir`, the same length as a full export, so the stems line up in any DAW.
/// Choke groups still apply, so a stem sounds as it does in the mix.
#[tauri::command]
pub async fn sampler_export_stems(dir: String, loops: usize, song: bool, app: AppHandle) -> Result<Vec<Exported>, String> {
    let job = render_job(&app, song)?;
    spawn_blocking(move || {
        let tracks = tracks(&job)?;
        let mut out = Vec::new();
        for pad in 0..PADS {
            let plays = (0..BANK).any(|b| job.shared.bank_pattern(b, pad) != 0);
            if job.samples[pad].is_none() || !plays {
                continue;
            }
            // Keep every pad's pattern but silence the others, so chokes behave as in the mix.
            let only: Vec<Option<Track>> = tracks.iter().enumerate().map(|(i, t)| if i == pad { t.clone() } else { None }).collect();
            let audio = render_with(&job, &only, loops);
            if audio[0].iter().all(|&x| x == 0.0) {
                continue; // this pad doesn't play in what's rendered
            }
            let path = Path::new(&dir).join(format!("{} - {:02} {}.wav", safe(&job.project), pad + 1, safe(&job.names[pad])));
            let format = export_wav(&audio, EXPORT_RATE, &path)?;
            out.push(Exported { seconds: audio[0].len() as f64 / EXPORT_RATE as f64, path: path.display().to_string(), format });
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Copies every sample that lives outside the project's folder into
/// `<project> Samples/` beside it, points the pads there, and saves, so the
/// project folder can move or be shared as one piece.
#[tauri::command]
pub async fn project_collect(app: AppHandle) -> Result<View, String> {
    let (project, todo) = {
        let s = state(&app);
        let doc = s.doc.lock().unwrap();
        let project = doc.path.clone().ok_or("save the project first, then collect its samples")?;
        let base = project.parent().unwrap_or(Path::new("")).to_path_buf();
        let todo: Vec<(usize, PathBuf)> = doc
            .pads
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.sample.as_ref().map(|x| (i, x.path.clone())))
            .filter(|(_, path)| !path.starts_with(&base))
            .collect();
        (project, todo)
    };
    let folder = project.with_file_name(format!("{} Samples", file_stem(&project)));
    let copied = spawn_blocking(move || -> Result<Vec<(usize, PathBuf, PathBuf)>, String> {
        std::fs::create_dir_all(&folder).map_err(|e| format!("can't make {}: {e}", folder.display()))?;
        let mut done = Vec::new();
        for (pad, from) in todo {
            let name = from.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "sample.wav".into());
            let size = std::fs::metadata(&from).map(|m| m.len()).ok();
            // Reuse a copy already there (same name and size); otherwise find a free name.
            let mut to = folder.join(&name);
            let mut n = 2;
            while to.exists() && std::fs::metadata(&to).map(|m| m.len()).ok() != size {
                to = folder.join(format!("{} {n}.{}", file_stem(&from), from.extension().and_then(|e| e.to_str()).unwrap_or("wav")));
                n += 1;
            }
            if !to.exists() {
                std::fs::copy(&from, &to).map_err(|e| format!("can't copy {}: {e}", from.display()))?;
            }
            done.push((pad, from, to));
        }
        Ok(done)
    })
    .await
    .map_err(|e| e.to_string())??;
    {
        let s = state(&app);
        let mut doc = s.doc.lock().unwrap();
        for (pad, from, to) in copied {
            // Only repoint a pad that still holds the sample we copied.
            if let Some(x) = doc.pads[pad].sample.clone().filter(|x| x.path == from) {
                doc.pads[pad].sample = Some(Arc::new(Sample { path: to, name: x.name.clone(), rate: x.rate, data: x.data.clone(), peaks: x.peaks.clone() }));
            }
        }
    }
    project_save(None, app)
}

/// Renders `loops` passes of the pattern to a 48 kHz WAV.
#[tauri::command]
pub async fn sampler_export(path: String, loops: usize, song: bool, app: AppHandle) -> Result<Exported, String> {
    let job = render_job(&app, song)?;
    spawn_blocking(move || {
        let mut path = PathBuf::from(path);
        if path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref() != Some("wav") {
            path.set_extension("wav");
        }
        let out = render(&job, loops)?;
        let format = export_wav(&out, EXPORT_RATE, &path)?;
        Ok(Exported { seconds: out[0].len() as f64 / EXPORT_RATE as f64, path: path.display().to_string(), format })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bounced {
    pad: usize,
    path: String,
}

/// Resamples: renders `loops` passes of the pattern and puts the result on
/// the first empty pad. The WAV goes in Music/tuneKore/Samples/Bounces.
#[tauri::command]
pub async fn sampler_bounce(loops: usize, song: bool, app: AppHandle) -> Result<Bounced, String> {
    let job = render_job(&app, song)?;
    let pad = first_free_pad(&app).ok_or("every pad is in use: clear one to bounce onto it")?;
    let dir = app.path().audio_dir().map_err(|e| e.to_string())?.join("tuneKore/Samples/Bounces");
    let a = app.clone();
    let result = spawn_blocking(move || -> Result<Bounced, String> {
        std::fs::create_dir_all(&dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
        let name: String = job.project.chars().map(|c| if "/\\:*?\"<>|".contains(c) { '-' } else { c }).collect();
        let path = (1..)
            .map(|n| dir.join(format!("{name} bounce {n}.wav")))
            .find(|p| !p.exists())
            .expect("an unused name");
        let out = render(&job, loops)?;
        export_wav(&out, EXPORT_RATE, &path)?;
        load_pad(&a, pad, &path, PadSettings::default())?;
        Ok(Bounced { pad, path: path.display().to_string() })
    })
    .await
    .map_err(|e| e.to_string())?;
    publish_now(&app);
    if let Ok(b) = &result {
        if let Some(folder) = Path::new(&b.path).parent() {
            let _ = app.emit("sampler:chopped", folder.display().to_string());
        }
    }
    result
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Chopped {
    pads: Vec<usize>,
    folder: String,
}

/// Slices a pad's sample (between its start and end) at its hits into up to
/// `count` pieces, and puts them on the empty pads in order. The WAVs go in
/// Music/tuneKore/Samples/Chops.
#[tauri::command]
pub async fn sampler_chop(pad: usize, count: usize, app: AppHandle) -> Result<Chopped, String> {
    check_pad(pad)?;
    let (sample, settings, free) = {
        let s = state(&app);
        let doc = s.doc.lock().unwrap();
        let p = &doc.pads[pad];
        let sample = p.sample.clone().ok_or("put a sample on this pad first")?;
        let free: Vec<usize> = doc.pads.iter().enumerate().filter(|(_, x)| x.sample.is_none() && x.missing.is_none()).map(|(i, _)| i).collect();
        (sample, p.settings, free)
    };
    if free.is_empty() {
        return Err("there are no empty pads to chop onto: clear some first".into());
    }
    let count = count.clamp(2, PADS).min(free.len());
    let root = app.path().audio_dir().map_err(|e| e.to_string())?.join("tuneKore/Samples/Chops");
    let a = app.clone();
    let result = spawn_blocking(move || -> Result<Chopped, String> {
        let name: String = sample.name.chars().map(|c| if "/\\:*?\"<>|".contains(c) { '-' } else { c }).collect();
        let folder = (1..).map(|n| root.join(format!("{name} chop {n}"))).find(|p| !p.exists()).expect("an unused name");
        std::fs::create_dir_all(&folder).map_err(|e| format!("can't make {}: {e}", folder.display()))?;
        let frames = sample.data[0].len();
        let from = (settings.start as f64 * frames as f64) as usize;
        let to = ((settings.end as f64 * frames as f64) as usize).min(frames);
        let mut starts = onsets(&sample.data, sample.rate, from, to, count);
        if starts.len() < 2 {
            starts = even(from, to, count);
        }
        let mut pads = Vec::new();
        for (i, (st, en)) in slices(&starts, to, to - from).into_iter().enumerate() {
            let path = folder.join(format!("{name} {:02}.wav", i + 1));
            let piece: Vec<Vec<f32>> = sample.data.iter().map(|c| c[st..en].to_vec()).collect();
            export_wav(&piece, sample.rate, &path)?;
            load_pad(&a, free[i], &path, PadSettings { pitch: settings.pitch, gain_db: settings.gain_db, ..Default::default() })?;
            pads.push(free[i]);
        }
        Ok(Chopped { pads, folder: folder.display().to_string() })
    })
    .await
    .map_err(|e| e.to_string())?;
    publish_now(&app);
    if let Ok(c) = &result {
        let _ = app.emit("sampler:chopped", &c.folder);
    }
    result
}

fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Sample".into())
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_round_trip_through_text() {
        let mut doc = Doc::default();
        doc.pads[0].patterns[0] = 0b1000_0001;
        doc.pads[2].patterns[3] = 1u64 << 63;
        let file = to_file(&doc, Path::new("/tmp/beat.tunek"));
        assert_eq!(file.pads[0].patterns[0], "x......x");
        assert_eq!(file.pads[1].patterns, vec![""; BANK]);
        assert_eq!(pattern_bits(&file.pads[0].patterns[0]), 0b1000_0001);
        assert_eq!(pattern_bits(&file.pads[2].patterns[3]), 1u64 << 63);
    }

    #[test]
    fn sample_paths_are_relative_inside_the_project_folder() {
        let mut doc = Doc::default();
        doc.pads[0].missing = Some(PathBuf::from("/music/beats/kick.wav"));
        doc.pads[1].missing = Some(PathBuf::from("/elsewhere/snare.wav"));
        let file = to_file(&doc, Path::new("/music/beats/song.tunek"));
        assert_eq!(file.pads[0].sample.as_deref(), Some("kick.wav"));
        assert_eq!(file.pads[1].sample.as_deref(), Some("/elsewhere/snare.wav"));
        let json = serde_json::to_string(&file).unwrap();
        let back: ProjectFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pads.len(), PADS);
    }

    #[test]
    fn edits_of_one_kind_in_a_run_undo_as_one_step() {
        let mut doc = Doc::default();
        for step in 0..4 {
            let before = doc.snapshot();
            doc.pads[0].patterns[0] |= 1 << step;
            doc.checkpoint("steps", before);
        }
        assert_eq!(doc.history.undo.len(), 1, "painting four steps is one undo");
        assert_eq!(doc.history.undo[0].pads[0].patterns[0], 0);

        let before = doc.snapshot();
        doc.tempo = 90.0;
        doc.checkpoint("transport", before);
        assert_eq!(doc.history.undo.len(), 2, "a different kind of edit is a new step");
        assert!(doc.dirty);

        doc.history.redo.push(doc.snapshot());
        let before = doc.snapshot();
        doc.swing = 0.5;
        doc.checkpoint("swing", before);
        assert!(doc.history.redo.is_empty(), "a new edit drops the redo steps");
    }

    #[test]
    fn the_pattern_switch_saves_and_older_projects_open_with_the_end() {
        let mut doc = Doc::default();
        assert_eq!(doc.switch, PatternSwitch::End);
        doc.switch = PatternSwitch::Now;
        let json = serde_json::to_string(&to_file(&doc, Path::new("/tmp/a.tunek"))).unwrap();
        assert!(json.contains(r#""pattern_switch":"now""#), "{json}");
        let back: ProjectFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pattern_switch, PatternSwitch::Now);

        // A project saved before the switch existed has no such key.
        let old = r#"{"format":"tunekore-project","version":1,"tempo":120,"swing":0,"steps":16,"pads":[]}"#;
        let file: ProjectFile = serde_json::from_str(old).unwrap();
        assert_eq!(file.pattern_switch, PatternSwitch::End);
    }

    #[test]
    fn velocities_are_saved_only_when_set() {
        let mut doc = Doc::default();
        let file = to_file(&doc, Path::new("/tmp/a.tunek"));
        assert!(file.pads[0].velocity.is_empty());
        doc.pads[0].set_velocity(1, 3, 128);
        let file = to_file(&doc, Path::new("/tmp/a.tunek"));
        assert_eq!(file.pads[0].velocity.len(), BANK);
        assert_eq!(file.pads[0].velocity[1][3], 128);
        assert_eq!(file.pads[0].velocity[0][3], 255);
        assert_eq!(doc.pads[1].velocity(2, 5), 255, "unset reads full");
    }

    #[test]
    fn splice_folder_comes_from_the_last_start_in_its_log() {
        let log = r#"x {"splice_folder":"/Users/n/Splice"} y
z Loaded user settings {"a":1,"splice_folder":"/Volumes/fitMusic/Splice","b":2}"#;
        assert_eq!(last_splice_folder(log), Some(PathBuf::from("/Volumes/fitMusic/Splice")));
        assert_eq!(last_splice_folder(r#"{"splice_folder":"\/Volumes\/X\/Splice"}"#), Some(PathBuf::from("/Volumes/X/Splice")));
        assert_eq!(last_splice_folder("nothing here"), None);
        assert_eq!(last_splice_folder(r#"{"splice_folder":""}"#), None);
    }

    #[test]
    fn gain_in_db_maps_to_linear() {
        let p = |db| PadSettings { gain_db: db, ..Default::default() }.params().gain;
        assert_eq!(p(0.0), 1.0);
        assert!((p(-6.0) - 0.501).abs() < 0.001);
        assert_eq!(p(-60.0), 0.0);
    }

    #[test]
    fn scan_finds_audio_and_skips_hidden_and_bundles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("Drums/Kicks")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::create_dir_all(root.join("Synth.vst3")).unwrap();
        for f in ["Drums/Kicks/Kick 1.wav", "Drums/snare.AIFF", "loop.mp3", "notes.txt", ".hidden/x.wav", "Synth.vst3/y.wav"] {
            std::fs::write(root.join(f), b"").unwrap();
        }
        let s = scan(root).unwrap();
        let names: Vec<(&str, &str)> = s.items.iter().map(|i| (i.folder.as_str(), i.name.as_str())).collect();
        assert_eq!(names, vec![("", "loop"), ("Drums", "snare"), ("Drums/Kicks", "Kick 1")]);
        assert!(!s.truncated);
    }
}
