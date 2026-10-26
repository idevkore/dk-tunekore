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
use tunekore_analyzer::{decode_file, export_wav, peaks, WavFormat, EXTENSIONS};
use tunekore_engine::{render_pattern, resample, PadParams, SamplerHandle, SamplerShared, Track, MAX_STEPS, PADS, PREVIEW};

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
}

impl Default for PadSettings {
    fn default() -> Self {
        Self { gain_db: 0.0, pan: 0.0, pitch: 0.0, start: 0.0, end: 1.0, gate: false, reverse: false, mute: false }
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
}

struct Doc {
    pads: Vec<Pad>,
    tempo: f32,
    swing: f32,
    steps: usize,
    /// The pattern that plays and is edited, 0..BANK.
    current: usize,
    path: Option<PathBuf>,
    dirty: bool,
    history: History,
}

impl Default for Doc {
    fn default() -> Self {
        Self {
            pads: (0..PADS).map(|_| Pad::default()).collect(),
            tempo: 120.0,
            swing: 0.0,
            steps: 16,
            current: 0,
            path: None,
            dirty: false,
            history: History::default(),
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
        Snapshot { pads: self.pads.clone(), tempo: self.tempo, swing: self.swing, steps: self.steps, current: self.current }
    }

    /// Records `before` as an undo step for an edit of kind `key`, unless it
    /// continues a run of the same kind.
    fn checkpoint(&mut self, key: &str, before: Snapshot) {
        let now = Instant::now();
        let merge = matches!(&self.history.last, Some((k, t)) if k == key && now.duration_since(*t) < MERGE_WINDOW);
        self.history.last = Some((key.to_string(), now));
        self.dirty = true;
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
}

impl SamplerState {
    pub fn new(handle: SamplerHandle) -> Self {
        let s = Self { doc: Mutex::default(), cache: Mutex::default(), handle };
        s.apply_all(&s.doc.lock().unwrap());
        s
    }

    fn apply_pad(&self, doc: &Doc, pad: usize) {
        let p = &doc.pads[pad];
        self.handle.set_pad(pad, &p.settings.params());
        self.handle.set_pattern(pad, p.patterns[doc.current]);
    }

    fn apply_all(&self, doc: &Doc) {
        for pad in 0..PADS {
            self.apply_pad(doc, pad);
        }
        self.handle.set_tempo(doc.tempo);
        self.handle.set_swing(doc.swing);
        self.handle.set_steps(doc.steps);
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
        doc.dirty = true;
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
    /// The pattern being played and edited, 0 = A.
    pattern: usize,
    /// Which patterns have any steps.
    patterns_used: Vec<bool>,
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
}

fn view(doc: &Doc) -> View {
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
    let v = view(doc);
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
    view(&doc)
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

/// Switches to pattern `index` (0 = A). While playing, the switch lands on the next step.
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

/// Copies pattern `from` over pattern `to` and switches to it.
#[tauri::command]
pub fn sampler_copy_pattern(from: usize, to: usize, app: AppHandle) -> Result<View, String> {
    if from >= BANK || to >= BANK {
        return Err("there are only four patterns".into());
    }
    edit(&app, &format!("copy:{from}:{to}"), |s, doc| {
        for p in doc.pads.iter_mut() {
            p.patterns[to] = p.patterns[from];
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
    let Some(snap) = doc.history.undo.pop() else { return Ok(view(&doc)) };
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
    let Some(snap) = doc.history.redo.pop() else { return Ok(view(&doc)) };
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

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Tick {
    playing: bool,
    step: Option<usize>,
    played: Vec<u32>,
}

/// Sends the step and each pad's play count to the UI about 30 times a
/// second while anything changes.
pub fn spawn_tick(app: AppHandle, handle: SamplerHandle) {
    std::thread::spawn(move || {
        let mut last: Option<(bool, Option<usize>, Vec<u32>)> = None;
        loop {
            std::thread::sleep(Duration::from_millis(33));
            let now = (handle.is_playing(), handle.current_step(), (0..PADS).map(|p| handle.played(p)).collect::<Vec<_>>());
            if last.as_ref() != Some(&now) {
                let _ = app.emit("sampler:tick", Tick { playing: now.0, step: now.1, played: now.2.clone() });
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

/// The folder the Library opens on first: Splice's sounds, then tuneKore's
/// own samples (where stems are chopped to), then Music.
#[tauri::command]
pub fn library_default(app: AppHandle) -> Option<String> {
    let home = app.path().home_dir().ok()?;
    let music = app.path().audio_dir().unwrap_or_else(|_| home.join("Music"));
    [home.join("Splice/sounds"), home.join("Splice"), music.join("tuneKore/Samples"), music]
        .into_iter()
        .find(|p| p.is_dir())
        .map(|p| p.display().to_string())
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
    pads: Vec<PadFile>,
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
    Ok(publish(&app, &doc))
}

#[tauri::command]
pub async fn project_open(path: String, app: AppHandle) -> Result<View, String> {
    let a = app.clone();
    spawn_blocking(move || -> Result<View, String> {
        let project = PathBuf::from(&path);
        let text = std::fs::read_to_string(&project).map_err(|e| format!("can't read {}: {e}", project.display()))?;
        let mut file: ProjectFile = serde_json::from_str(&text).map_err(|e| format!("not a tuneKore project ({e})"))?;
        if file.format != FORMAT {
            return Err("not a tuneKore project".into());
        }
        if file.version > VERSION {
            return Err("this project was saved by a newer tuneKore".into());
        }
        let s = state(&a);
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
        let mut au = audio(&a).lock().unwrap();
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
            path: Some(project),
            dirty: false,
            history: History::default(),
        };
        s.apply_all(&doc);
        Ok(publish(&a, &doc))
    })
    .await
    .map_err(|e| e.to_string())?
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
    Ok(publish(&app, &doc))
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
}

fn render_job(app: &AppHandle) -> Result<RenderJob, String> {
    let s = state(app);
    let doc = s.doc.lock().unwrap();
    if doc.pads.iter().all(|p| p.patterns[doc.current] == 0 || p.sample.is_none()) {
        return Err("there is nothing in the pattern yet: add steps first".into());
    }
    let shared = SamplerShared::default();
    for (i, p) in doc.pads.iter().enumerate() {
        shared.set_pad(i, &p.settings.params());
        shared.set_pattern(i, p.patterns[doc.current]);
    }
    shared.set_tempo(doc.tempo);
    shared.set_swing(doc.swing);
    shared.set_steps(doc.steps);
    Ok(RenderJob {
        samples: doc.pads.iter().map(|p| p.sample.clone()).collect(),
        shared,
        project: doc.path.as_deref().map(file_stem).unwrap_or_else(|| "Untitled".into()),
    })
}

/// Renders `loops` passes of the pattern at 48 kHz. Blocking.
fn render(job: &RenderJob, loops: usize) -> Result<Vec<Vec<f32>>, String> {
    let mut tracks: Vec<Option<Track>> = Vec::with_capacity(PADS);
    for s in &job.samples {
        tracks.push(match s {
            None => None,
            Some(s) if s.rate == EXPORT_RATE => Some(s.data.clone()),
            Some(s) => Some(Arc::new(resample(&s.data, s.rate, EXPORT_RATE)?)),
        });
    }
    Ok(render_pattern(&tracks, &job.shared, EXPORT_RATE, loops.clamp(1, 64)))
}

/// Renders `loops` passes of the pattern to a 48 kHz WAV.
#[tauri::command]
pub async fn sampler_export(path: String, loops: usize, app: AppHandle) -> Result<Exported, String> {
    let job = render_job(&app)?;
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
pub async fn sampler_bounce(loops: usize, app: AppHandle) -> Result<Bounced, String> {
    let job = render_job(&app)?;
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
