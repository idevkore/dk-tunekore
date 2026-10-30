//! Projects: the `.tunek` file, new, open and save, autosave and recovery.
//!
//! A project file is one JSON object with a section per module:
//!
//! ```json
//! {
//!   "format": "tunekore-project",
//!   "version": 2,
//!   "transport": { "tempo": 120.0, "timeSignature": [4, 4] },
//!   "tracks": [{ "id": 1, "name": "Sampler", "mix": { "volume": 1.0, "pan": 0.0, "mute": false, "solo": false } }],
//!   "sampler": { "swing": 0.0, "steps": 16, "pads": [ ... ] }
//! }
//! ```
//!
//! `transport` and `tracks` belong to this file. Every other section belongs
//! to the module that registers a [`Section`] under its key ([`register`]),
//! from its own file: the Sampler's is `sampler`, and the Timeline (#36),
//! plugins (#37) and MIDI mapping (#20) take [`HOOKS`], which are written as
//! `null` until then. Sections are read in the order they registered, after
//! `transport` and `tracks`, so a module finds its tracks already there. A
//! section no module reads is written back as it was.
//!
//! Opening reads every section before any applies, so a file that doesn't
//! parse or doesn't pass a section's checks changes nothing. A section that
//! still fails while applying leaves the project untitled and unsaved, so
//! Save can't write a half-opened project over the file it came from.
//!
//! Version 1 files (the Sampler's fields at the top level) open as one
//! Sampler track.
//!
//! The logic is [`Project`], over any context `C` (the app's [`AppHandle`],
//! or a test's own). Locks: `Project`'s own mutexes are leaves, except
//! `busy`, which open, new, save and autosave hold throughout so a file
//! always holds one project; sections never take it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tauri::{AppHandle, Emitter, Manager};
use tunekore_engine::{track::TRACK_SLOTS, TrackId, TrackKind, TrackMix, TransportShared, SAMPLER_TRACK};

use crate::Audio;

/// Project files end in `.tunek`.
pub const PROJECT_EXT: &str = "tunek";
const FORMAT: &str = "tunekore-project";
/// The format this build writes. It opens every version up to this one.
pub const VERSION: u32 = 2;
/// Sections kept for the modules still to come. Each registers its own.
pub const HOOKS: [&str; 3] = ["timeline", "plugins", "midi"];
/// How often unsaved work is written to the recovery file.
const AUTOSAVE_EVERY: Duration = Duration::from_secs(20);

/// Puts a module's state into the project file and back. `C` is the
/// context the module needs: the app, in the app.
pub trait Section<C>: Send + Sync + 'static {
    /// The section's key in the file.
    fn key(&self) -> &'static str;

    /// This module's part of a file being written to `file` (so paths can be
    /// made relative to its folder).
    fn save(&self, ctx: &C, file: &Path) -> Result<Value, String>;

    /// Reads and checks this module's part of `file`, or `None` when the
    /// file has none (it is older than the module). Do the slow work here
    /// (decoding, loading) and every check that can fail, without changing
    /// anything; the returned step makes the change and should not fail.
    fn read(&self, ctx: &C, value: Option<&Value>, file: &Path) -> Result<Apply<C>, String>;

    /// Empties this module for a new project.
    fn clear(&self, ctx: &C) -> Result<(), String>;

    /// Changes whenever this module's saved state changes, so the project
    /// knows it has unsaved work and autosave knows when to write. Must not
    /// wait on a lock: it is asked while other locks are held.
    fn revision(&self, ctx: &C) -> u64;
}

/// Takes a lock even if a section panicked while it was held: the project's
/// own state stays usable (the section's own state is the section's).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The second half of [`Section::read`]: applies what was read.
pub type Apply<C> = Box<dyn FnOnce(&C) -> Result<(), String> + Send>;

/// The open project: where it lives, its sections, and what was last saved.
pub struct Project<C: 'static> {
    path: Mutex<Option<PathBuf>>,
    sections: Mutex<Vec<Arc<dyn Section<C>>>>,
    /// How many of `sections`, at the front, are the core ones (`transport`, `tracks`).
    core: Mutex<usize>,
    /// Sections from the file that no module here reads, written back as they were.
    kept: Mutex<Map<String, Value>>,
    /// Every section's revision at the last save or open; `None` while the
    /// work is unsaved by nature (a recovered autosave, a file that opened in part).
    saved: Mutex<Option<Vec<u64>>>,
    /// Held through open, new, save and autosave.
    busy: Mutex<()>,
    /// The last session left unsaved work and nobody has said what to do
    /// with it yet: autosave waits, so it can't write over it.
    recovery_pending: AtomicBool,
}

impl<C: 'static> Default for Project<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: 'static> Project<C> {
    pub fn new() -> Self {
        Self {
            path: Mutex::default(),
            sections: Mutex::default(),
            core: Mutex::new(0),
            kept: Mutex::default(),
            saved: Mutex::new(None),
            busy: Mutex::default(),
            recovery_pending: AtomicBool::new(false),
        }
    }

    /// Adds a module's section. Call it once, from the module's setup.
    /// Sections read and apply in the order they registered, after the core ones.
    pub fn register(&self, section: impl Section<C>) {
        let mut list = lock(&self.sections);
        assert!(list.iter().all(|s| s.key() != section.key()), "two project sections are called {}", section.key());
        list.push(Arc::new(section));
    }

    /// Adds a core section (`transport`, `tracks`): it goes before every
    /// module's, whenever it registers, so modules find their tracks applied.
    pub fn register_core(&self, section: impl Section<C>) {
        let mut list = lock(&self.sections);
        assert!(list.iter().all(|s| s.key() != section.key()), "two project sections are called {}", section.key());
        let mut core = lock(&self.core);
        list.insert(*core, Arc::new(section));
        *core += 1;
    }

    fn sections(&self) -> Vec<Arc<dyn Section<C>>> {
        lock(&self.sections).clone()
    }

    /// Runs `f` while no open, new, save or autosave runs, and keeps them
    /// waiting until it returns: for a module's background work on its own
    /// document that mustn't land in the middle of one (the Timeline's players
    /// fed again after the output's rate changes). Lock order: this (`busy`),
    /// then the module's document, then the engine. `f` must not open, save or
    /// start a project, nor call `exclusive` again: `busy` is held already.
    pub fn exclusive<R>(&self, f: impl FnOnce() -> R) -> R {
        let _busy = lock(&self.busy);
        f()
    }

    /// The project's file, or `None` before it is first saved.
    pub fn path(&self) -> Option<PathBuf> {
        lock(&self.path).clone()
    }

    /// The project's name: its file's name, or "Untitled".
    pub fn name(&self) -> String {
        self.path().as_deref().map(file_stem).unwrap_or_else(|| "Untitled".into())
    }

    fn revisions(&self, ctx: &C) -> Vec<u64> {
        self.sections().iter().map(|s| s.revision(ctx)).collect()
    }

    /// Whether anything changed since the project was last saved or opened.
    pub fn is_dirty(&self, ctx: &C) -> bool {
        let now = self.revisions(ctx);
        lock(&self.saved).as_ref() != Some(&now)
    }

    /// Counts what is there now as saved: at launch, once every module has registered.
    pub fn mark_saved(&self, ctx: &C) {
        let now = self.revisions(ctx);
        *lock(&self.saved) = Some(now);
    }

    /// The whole project as file text, for `file`. The caller holds `busy`.
    fn serialize(&self, ctx: &C, file: &Path) -> Result<String, String> {
        let mut obj = lock(&self.kept).clone();
        obj.insert("format".into(), FORMAT.into());
        obj.insert("version".into(), VERSION.into());
        for key in HOOKS {
            obj.entry(key).or_insert(Value::Null);
        }
        for s in self.sections() {
            obj.insert(s.key().into(), s.save(ctx, file)?);
        }
        serde_json::to_string_pretty(&Value::Object(obj)).map(|t| t + "\n").map_err(|e| e.to_string())
    }

    /// Opens a project from its text. `file` is where it was read from (for
    /// relative paths), `belongs_to` the project it belongs to (the file
    /// itself, or the original project for a recovered autosave), and
    /// `dirty` marks the work unsaved.
    pub fn open_text(&self, ctx: &C, text: &str, file: &Path, belongs_to: Option<PathBuf>, dirty: bool) -> Result<(), String> {
        let mut obj = parse(text)?;
        let sections = self.sections();
        let mut steps = Vec::with_capacity(sections.len());
        for s in &sections {
            // A hook written as null is a section with nothing in it yet.
            steps.push(s.read(ctx, obj.get(s.key()).filter(|v| !v.is_null()), file)?);
        }
        // From here the project changes: apply every section, even past a
        // failure. Only this part holds `busy`, so a save or autosave waits
        // for the applying, never for the decoding above.
        let _busy = lock(&self.busy);
        let mut failed = None;
        for step in steps {
            if let Err(e) = step(ctx) {
                failed.get_or_insert(e);
            }
        }
        for s in &sections {
            obj.remove(s.key());
        }
        obj.retain(|_, v| !v.is_null());
        *lock(&self.kept) = obj;
        if let Some(e) = failed {
            *lock(&self.path) = None;
            *lock(&self.saved) = None;
            return Err(format!("the project opened only in part ({e}). It is untitled now, so saving it won't write over the original"));
        }
        *lock(&self.path) = belongs_to;
        if dirty {
            *lock(&self.saved) = None;
        } else {
            self.mark_saved(ctx);
        }
        Ok(())
    }

    /// Opens `file`. Blocking: call it off the UI thread.
    pub fn open(&self, ctx: &C, file: &Path, belongs_to: Option<PathBuf>, dirty: bool) -> Result<(), String> {
        let text = std::fs::read_to_string(file).map_err(|e| format!("can't read {}: {e}", file.display()))?;
        self.open_text(ctx, &text, file, belongs_to, dirty)
    }

    /// Empties every module for a new, untitled project.
    pub fn new_project(&self, ctx: &C) -> Result<(), String> {
        let _busy = lock(&self.busy);
        let mut failed = None;
        for s in self.sections() {
            if let Err(e) = s.clear(ctx) {
                failed.get_or_insert(e);
            }
        }
        lock(&self.kept).clear();
        *lock(&self.path) = None;
        match failed {
            None => {
                self.mark_saved(ctx);
                Ok(())
            }
            Some(e) => {
                *lock(&self.saved) = None;
                Err(format!("the new project isn't empty everywhere ({e})"))
            }
        }
    }

    /// Saves to `to` (given the `.tunek` extension), or to the project's own
    /// file when `to` is `None`. Returns where it saved.
    pub fn save(&self, ctx: &C, to: Option<PathBuf>) -> Result<PathBuf, String> {
        let _busy = lock(&self.busy);
        let mut file = to.or_else(|| self.path()).ok_or("choose where to save the project")?;
        if file.extension().and_then(|e| e.to_str()) != Some(PROJECT_EXT) {
            file.set_extension(PROJECT_EXT);
        }
        // The revisions before the file is written: an edit that lands while
        // it is written (a recorded hit, say) isn't in it, so it stays unsaved.
        let now = self.revisions(ctx);
        let text = self.serialize(ctx, &file)?;
        let tmp = file.with_extension(format!("{PROJECT_EXT}.part"));
        std::fs::write(&tmp, text).map_err(|e| format!("can't write {}: {e}", file.display()))?;
        std::fs::rename(&tmp, &file).map_err(|e| e.to_string())?;
        *lock(&self.path) = Some(file.clone());
        *lock(&self.saved) = Some(now);
        Ok(file)
    }

    /// The text for an autosave to `file` and the revisions it holds, when
    /// there is unsaved work not yet written as `written`, and no recovery
    /// is waiting for an answer.
    fn autosave_text(&self, ctx: &C, file: &Path, written: Option<&Vec<u64>>) -> Option<(String, Vec<u64>)> {
        if self.recovery_pending.load(Ordering::Acquire) {
            return None;
        }
        let _busy = lock(&self.busy);
        let now = self.revisions(ctx);
        if !self.is_dirty(ctx) || written == Some(&now) {
            return None;
        }
        self.serialize(ctx, file).ok().map(|t| (t, now))
    }
}

/// Reads file text into its sections, as the current format: checks the
/// format and version, and turns a version 1 file into version 2.
fn parse(text: &str) -> Result<Map<String, Value>, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not a tuneKore project ({e})"))?;
    let Value::Object(mut obj) = value else { return Err("not a tuneKore project".into()) };
    if obj.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err("not a tuneKore project".into());
    }
    // A whole number only: a version this build can't place opens nothing.
    let version = obj
        .get("version")
        .and_then(Value::as_u64)
        .filter(|&v| v >= 1)
        .ok_or("not a tuneKore project (its version is missing or not a whole number)")?;
    if version > u64::from(VERSION) {
        return Err("this project was saved by a newer tuneKore".into());
    }
    if version < 2 {
        obj = from_v1(obj);
    }
    obj.remove("format");
    obj.remove("version");
    Ok(obj)
}

/// Version 1 was the Sampler alone: its fields at the top level, the tempo among them.
fn from_v1(mut obj: Map<String, Value>) -> Map<String, Value> {
    obj.remove("format");
    obj.remove("version");
    let tempo = obj.remove("tempo").unwrap_or(json!(120.0));
    let mut v2 = Map::new();
    v2.insert("transport".into(), json!({ "tempo": tempo, "timeSignature": [4, 4] }));
    v2.insert("tracks".into(), json!([TrackFile { id: SAMPLER_TRACK, name: "Sampler".into(), mix: TrackMix::default(), kind: None }]));
    v2.insert("sampler".into(), Value::Object(obj));
    v2
}

pub fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Untitled".into())
}

// ---- The core sections ----

/// `transport`: tempo and time signature.
pub struct TransportSection(pub Arc<TransportShared>);

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct TransportFile {
    tempo: f64,
    time_signature: [u32; 2],
}

impl Default for TransportFile {
    fn default() -> Self {
        Self { tempo: 120.0, time_signature: [4, 4] }
    }
}

impl<C: 'static> Section<C> for TransportSection {
    fn key(&self) -> &'static str {
        "transport"
    }

    fn save(&self, _: &C, _: &Path) -> Result<Value, String> {
        let sig = self.0.time_signature();
        serde_json::to_value(TransportFile { tempo: self.0.tempo(), time_signature: [sig.numerator, sig.denominator] }).map_err(|e| e.to_string())
    }

    fn read(&self, _: &C, value: Option<&Value>, _: &Path) -> Result<Apply<C>, String> {
        let file: TransportFile = match value {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the project's transport is damaged ({e})"))?,
            None => TransportFile::default(),
        };
        let t = self.0.clone();
        Ok(Box::new(move |_| {
            t.stop();
            // The Sampler's tempo box goes from 40 to 300.
            t.set_tempo(file.tempo.clamp(40.0, 300.0));
            // 4/4 first, so a signature this build can't use doesn't keep the last project's.
            t.set_time_signature(4, 4);
            t.set_time_signature(file.time_signature[0], file.time_signature[1]);
            t.seek(0.0);
            Ok(())
        }))
    }

    fn clear(&self, _: &C) -> Result<(), String> {
        self.0.stop();
        self.0.set_tempo(120.0);
        self.0.set_time_signature(4, 4);
        self.0.seek(0.0);
        Ok(())
    }

    fn revision(&self, _: &C) -> u64 {
        let sig = self.0.time_signature();
        self.0.tempo().to_bits() ^ (u64::from(sig.numerator) << 40) ^ (u64::from(sig.denominator) << 48)
    }
}

/// `tracks`: the track list and each track's mix. A module puts its devices
/// on its tracks from its own section, by track id.
pub struct TracksSection {
    audio: &'static Mutex<Audio>,
    /// The mixer's revision, read without the engine's lock.
    revision: Arc<AtomicU64>,
}

impl TracksSection {
    pub fn new(audio: &'static Mutex<Audio>) -> Self {
        let revision = audio.lock().unwrap().engine.mixer().revision_handle();
        Self { audio, revision }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackFile {
    id: TrackId,
    name: String,
    #[serde(default)]
    mix: TrackMix,
    /// Left out when it is the default: the Sampler's for track 1, audio for
    /// the rest (as every version 2 file before kinds were saved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<TrackKind>,
}

impl TrackFile {
    /// The kind it names, or its default. Only track 1 is the Sampler's.
    fn kind(&self) -> TrackKind {
        match (self.id == SAMPLER_TRACK, self.kind) {
            (true, _) => TrackKind::Sampler,
            (false, Some(TrackKind::Sampler) | None) => TrackKind::Audio,
            (false, Some(k)) => k,
        }
    }
}

/// What a track's kind is when the file doesn't say.
fn default_kind(id: TrackId) -> TrackKind {
    if id == SAMPLER_TRACK { TrackKind::Sampler } else { TrackKind::Audio }
}

/// Sets the engine's track list to `tracks`: the Sampler's track always
/// stays, others are added or removed to match. `tracks` was checked first.
fn apply_tracks(audio: &Mutex<Audio>, tracks: &[TrackFile]) -> Result<(), String> {
    let mut audio = audio.lock().unwrap();
    let mixer = audio.engine.mixer();
    for t in mixer.tracks().iter().filter(|t| t.id != SAMPLER_TRACK && tracks.iter().all(|f| f.id != t.id)) {
        mixer.remove_track(t.id)?;
    }
    if tracks.iter().all(|f| f.id != SAMPLER_TRACK) {
        mixer.set_mix(SAMPLER_TRACK, TrackMix::default())?;
        mixer.rename(SAMPLER_TRACK, "Sampler")?;
        mixer.set_kind(SAMPLER_TRACK, TrackKind::Sampler)?;
    }
    for f in tracks {
        if mixer.track(f.id).is_none() {
            mixer.add_track_as(f.id, &f.name)?;
        }
        mixer.rename(f.id, &f.name)?;
        mixer.set_mix(f.id, f.mix)?;
        mixer.set_kind(f.id, f.kind())?;
    }
    Ok(())
}

impl<C: 'static> Section<C> for TracksSection {
    fn key(&self) -> &'static str {
        "tracks"
    }

    fn save(&self, _: &C, _: &Path) -> Result<Value, String> {
        let tracks: Vec<TrackFile> = self
            .audio
            .lock()
            .unwrap()
            .engine
            .tracks()
            .into_iter()
            .map(|t| TrackFile { kind: (t.kind != default_kind(t.id)).then_some(t.kind), id: t.id, name: t.name, mix: t.mix })
            .collect();
        serde_json::to_value(tracks).map_err(|e| e.to_string())
    }

    fn read(&self, _: &C, value: Option<&Value>, _: &Path) -> Result<Apply<C>, String> {
        let tracks: Vec<TrackFile> = match value {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the project's tracks are damaged ({e})"))?,
            None => Vec::new(),
        };
        // Everything applying could trip on, checked now.
        let others = tracks.iter().filter(|t| t.id != SAMPLER_TRACK).count();
        if others + 1 > TRACK_SLOTS {
            return Err(format!("the project has {} tracks, and tuneKore runs at most {TRACK_SLOTS}", tracks.len()));
        }
        let mut ids: Vec<u32> = tracks.iter().map(|t| t.id.0).collect();
        ids.sort_unstable();
        if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
            return Err(format!("the project has two tracks numbered {}", w[0]));
        }
        if ids.contains(&0) {
            return Err("the project has a track numbered 0".into());
        }
        let audio = self.audio;
        Ok(Box::new(move |_| apply_tracks(audio, &tracks)))
    }

    fn clear(&self, _: &C) -> Result<(), String> {
        apply_tracks(self.audio, &[TrackFile { id: SAMPLER_TRACK, name: "Sampler".into(), mix: TrackMix::default(), kind: None }])
    }

    fn revision(&self, _: &C) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }
}

/// `trackIds`: the id the project's next new track gets, `{"next": n}`, so a
/// deleted track's id never comes back for another (a MIDI mapping or a plugin
/// reference to it stays dangling rather than landing on the new track). Its
/// own section so the `tracks` list keeps the shape older builds read; they
/// keep this one as a section they don't know. Opening a file without it, and
/// New, start past the highest track there is.
pub struct TrackIdsSection(pub &'static Mutex<Audio>);

#[derive(Serialize, Deserialize, Default)]
struct TrackIdsFile {
    next: u32,
}

impl<C: 'static> Section<C> for TrackIdsSection {
    fn key(&self) -> &'static str {
        "trackIds"
    }

    fn save(&self, _: &C, _: &Path) -> Result<Value, String> {
        let next = self.0.lock().unwrap().engine.mixer().next_track_id().0;
        serde_json::to_value(TrackIdsFile { next }).map_err(|e| e.to_string())
    }

    fn read(&self, _: &C, value: Option<&Value>, _: &Path) -> Result<Apply<C>, String> {
        let file: TrackIdsFile = match value {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the project's track ids are damaged ({e})"))?,
            None => TrackIdsFile::default(),
        };
        let audio = self.0;
        // Applies after `tracks`: it is registered after it.
        Ok(Box::new(move |_| {
            audio.lock().unwrap().engine.mixer().set_next_track_id(TrackId(file.next));
            Ok(())
        }))
    }

    fn clear(&self, _: &C) -> Result<(), String> {
        self.0.lock().unwrap().engine.mixer().set_next_track_id(TrackId(0));
        Ok(())
    }

    /// Changes only when tracks are added or removed, which the `tracks` section counts.
    fn revision(&self, _: &C) -> u64 {
        0
    }
}

// ---- In the app ----

fn project(app: &AppHandle) -> tauri::State<'_, Project<AppHandle>> {
    app.state::<Project<AppHandle>>()
}

/// Adds a module's section. Call it once, from the module's setup.
pub fn register(app: &AppHandle, section: impl Section<AppHandle>) {
    project(app).register(section);
}

/// [`Project::exclusive`] on the app's project: `f` runs while no open, new,
/// save or autosave does. Lock order: busy, then a module's document, then the engine.
pub fn exclusive<R>(app: &AppHandle, f: impl FnOnce() -> R) -> R {
    project(app).exclusive(f)
}

pub fn path(app: &AppHandle) -> Option<PathBuf> {
    project(app).path()
}

pub fn name(app: &AppHandle) -> String {
    project(app).name()
}

pub fn is_dirty(app: &AppHandle) -> bool {
    project(app).is_dirty(app)
}

/// Opens `file`; see [`Project::open`].
pub fn open(app: &AppHandle, file: &Path, belongs_to: Option<PathBuf>, dirty: bool) -> Result<(), String> {
    project(app).open(app, file, belongs_to, dirty)
}

pub fn new_project(app: &AppHandle) -> Result<(), String> {
    let r = project(app).new_project(app);
    clear_autosave(app);
    r
}

pub fn save(app: &AppHandle, to: Option<PathBuf>) -> Result<PathBuf, String> {
    let file = project(app).save(app, to)?;
    clear_autosave(app);
    Ok(file)
}

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
fn spawn_autosave(app: AppHandle) {
    std::thread::spawn(move || {
        let mut written: Option<Vec<u64>> = None;
        loop {
            std::thread::sleep(AUTOSAVE_EVERY);
            let Some(dir) = autosave_dir(&app) else { continue };
            let file = dir.join("recovery.tunek");
            // A section that panics costs this autosave, not the thread.
            let text = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| project(&app).autosave_text(&app, &file, written.as_ref())));
            let Ok(Some((text, now))) = text else { continue };
            let meta = Recovery {
                project_name: name(&app),
                project_path: path(&app).map(|p| p.display().to_string()),
                saved_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
            };
            let Ok(meta) = serde_json::to_string(&meta) else { continue };
            let write = || -> std::io::Result<()> {
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join("recovery.tunek.part"), text)?;
                std::fs::rename(dir.join("recovery.tunek.part"), &file)?;
                std::fs::write(dir.join("recovery.json"), meta)
            };
            if write().is_ok() {
                written = Some(now);
            }
        }
    });
}

/// Removes the recovery files: the work is saved, or the user chose to drop it.
pub fn clear_autosave(app: &AppHandle) {
    project(app).recovery_pending.store(false, Ordering::Release);
    if let Some(dir) = autosave_dir(app) {
        let _ = std::fs::remove_file(dir.join("recovery.tunek"));
        let _ = std::fs::remove_file(dir.join("recovery.json"));
    }
}

/// The recovery file and its project, when a session that didn't close
/// normally left unsaved work.
pub fn recovery(app: &AppHandle) -> Option<(PathBuf, Recovery)> {
    let dir = autosave_dir(app)?;
    let file = dir.join("recovery.tunek");
    if !file.exists() {
        return None;
    }
    let meta = serde_json::from_str(&std::fs::read_to_string(dir.join("recovery.json")).ok()?).ok()?;
    Some((file, meta))
}

/// Opens the unsaved work, marked unsaved, under its original project's name. Blocking.
pub fn recover(app: &AppHandle) -> Result<(), String> {
    let (file, meta) = recovery(app).ok_or("there is nothing to recover")?;
    let r = open(app, &file, meta.project_path.map(PathBuf::from), true);
    // Answered: autosave may write again (over the recovery file, with this work).
    project(app).recovery_pending.store(false, Ordering::Release);
    r
}

/// Unsaved work left by a session that didn't close normally, if any.
#[tauri::command]
pub fn autosave_check(app: AppHandle) -> Option<Recovery> {
    recovery(&app).map(|(_, meta)| meta)
}

#[tauri::command]
pub fn autosave_discard(app: AppHandle) {
    clear_autosave(&app);
}

/// The question about the last session's work couldn't be asked: keep its
/// files, moved aside to `recovery-<unix seconds>.tunek` (and `.json`) so
/// this session's autosave can't write over them, and let autosave carry on.
#[tauri::command]
pub fn autosave_resume(app: AppHandle) {
    if let Some(dir) = autosave_dir(&app) {
        set_aside(&dir, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    }
    project(&app).recovery_pending.store(false, Ordering::Release);
}

/// Renames the recovery files in `dir` to `recovery-<stamp>.*`, keeping them
/// out of autosave's way. Leaves them where they are if the rename fails.
fn set_aside(dir: &Path, stamp: u64) {
    for ext in ["tunek", "json"] {
        let from = dir.join(format!("recovery.{ext}"));
        if from.exists() {
            let _ = std::fs::rename(&from, dir.join(format!("recovery-{stamp}.{ext}")));
        }
    }
}

/// Project files the system asked the app to open (a double-click in the
/// Finder, a file dropped on the Dock icon, a path on the command line),
/// waiting for the main window to open them. The UI may not be listening
/// yet when they arrive at launch, so they wait here until it asks.
#[derive(Default)]
pub struct Opened(Mutex<Vec<PathBuf>>);

/// Whether `path` is a project file, by its extension.
fn is_project_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case(PROJECT_EXT))
}

/// Queues the project files among `paths` for the main window, and tells it.
pub fn files_opened(app: &AppHandle, paths: impl IntoIterator<Item = PathBuf>) {
    let files: Vec<PathBuf> = paths.into_iter().filter(|p| is_project_file(p)).collect();
    if files.is_empty() {
        return;
    }
    if let Some(opened) = app.try_state::<Opened>() {
        lock(&opened.0).extend(files);
        let _ = app.emit_to("main", "app:open-files", ());
    }
}

/// The project files waiting to open, oldest first. Asking empties the queue.
#[tauri::command]
pub fn opened_files(app: AppHandle) -> Vec<String> {
    app.try_state::<Opened>()
        .map(|o| std::mem::take(&mut *lock(&o.0)).into_iter().map(|p| p.display().to_string()).collect())
        .unwrap_or_default()
}

/// This file's commands. The Sampler answers new, open, save and recover,
/// since the UI wants its view back from them.
pub fn commands() -> crate::Commands {
    crate::commands![autosave_check, autosave_discard, autosave_resume, opened_files]
}

/// Registers the core sections, first, and starts autosave.
pub fn setup(app: &AppHandle) {
    let p = project(app);
    p.register_core(TransportSection(crate::audio(app).lock().unwrap().engine.transport()));
    p.register_core(TracksSection::new(crate::audio(app)));
    p.register_core(TrackIdsSection(crate::audio(app)));
    spawn_autosave(app.clone());
    // Windows and Linux pass the file to open as an argument; macOS sends
    // it as an event (see `run` in lib.rs).
    files_opened(app, std::env::args_os().skip(1).map(PathBuf::from));
}

/// Once every module has registered: what is there now is the saved, empty
/// project, and a recovery left by the last session waits for its answer.
pub fn ready(app: &AppHandle) {
    let p = project(app);
    p.mark_saved(app);
    p.recovery_pending.store(recovery(app).is_some(), Ordering::Release);
}

#[cfg(test)]
pub fn tests_parse(text: &str) -> Map<String, Value> {
    parse(text).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_project_files_open_from_the_finder() {
        assert!(is_project_file(Path::new("/music/beat.tunek")));
        assert!(is_project_file(Path::new("/music/BEAT.TUNEK")));
        assert!(!is_project_file(Path::new("/music/kick.wav")));
        assert!(!is_project_file(Path::new("-psn_0_12345")), "macOS's own launch argument");
        assert!(!is_project_file(Path::new("/music/tunek")));
    }

    /// A module's section for the tests: its state is a JSON value.
    struct Fake {
        key: &'static str,
        state: Arc<Mutex<Value>>,
        revision: Arc<AtomicU64>,
        fail_apply: bool,
    }

    impl Fake {
        fn new(key: &'static str) -> (Self, Arc<Mutex<Value>>, Arc<AtomicU64>) {
            let (state, revision) = (Arc::new(Mutex::new(Value::Null)), Arc::new(AtomicU64::new(0)));
            (Self { key, state: state.clone(), revision: revision.clone(), fail_apply: false }, state, revision)
        }
    }

    impl Section<()> for Fake {
        fn key(&self) -> &'static str {
            self.key
        }
        fn save(&self, _: &(), _: &Path) -> Result<Value, String> {
            Ok(self.state.lock().unwrap().clone())
        }
        fn read(&self, _: &(), value: Option<&Value>, _: &Path) -> Result<Apply<()>, String> {
            let (v, state, rev, fail) = (value.cloned().unwrap_or(Value::Null), self.state.clone(), self.revision.clone(), self.fail_apply);
            Ok(Box::new(move |_| {
                if fail {
                    return Err("the disk went away".into());
                }
                *state.lock().unwrap() = v;
                rev.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }))
        }
        fn clear(&self, _: &()) -> Result<(), String> {
            *self.state.lock().unwrap() = Value::Null;
            self.revision.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn revision(&self, _: &()) -> u64 {
            self.revision.load(Ordering::Relaxed)
        }
    }

    /// A project with the core sections on a real engine (no output open) and a fake `sampler`.
    fn rig() -> (Project<()>, Arc<TransportShared>, Arc<Mutex<Value>>, Arc<AtomicU64>) {
        let audio: &'static Mutex<Audio> = Box::leak(Box::new(Mutex::new(Audio::new(tunekore_engine::Engine::new()))));
        let transport = audio.lock().unwrap().engine.transport();
        let p = Project::new();
        // A module that registers before the core still reads after it.
        let (fake, state, rev) = Fake::new("sampler");
        p.register(fake);
        p.register(SeesTracks { audio, saw: Arc::default() });
        p.register_core(TransportSection(transport.clone()));
        p.register_core(TracksSection::new(audio));
        p.mark_saved(&());
        (p, transport, state, rev)
    }

    /// A module's section that checks, as it applies, that the file's tracks already are.
    struct SeesTracks {
        audio: &'static Mutex<Audio>,
        saw: Arc<Mutex<Vec<u32>>>,
    }

    impl Section<()> for SeesTracks {
        fn key(&self) -> &'static str {
            "plugins"
        }
        fn save(&self, _: &(), _: &Path) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn read(&self, _: &(), _: Option<&Value>, _: &Path) -> Result<Apply<()>, String> {
            let (audio, saw) = (self.audio, self.saw.clone());
            Ok(Box::new(move |_| {
                *saw.lock().unwrap() = audio.lock().unwrap().engine.tracks().iter().map(|t| t.id.0).collect();
                Ok(())
            }))
        }
        fn clear(&self, _: &()) -> Result<(), String> {
            Ok(())
        }
        fn revision(&self, _: &()) -> u64 {
            0
        }
    }

    #[test]
    fn a_module_section_applies_after_the_tracks_it_refers_to() {
        let audio: &'static Mutex<Audio> = Box::leak(Box::new(Mutex::new(Audio::new(tunekore_engine::Engine::new()))));
        let p: Project<()> = Project::new();
        let saw = Arc::new(Mutex::new(Vec::new()));
        p.register(SeesTracks { audio, saw: saw.clone() }); // "plugins" sorts before "project"
        p.register_core(TransportSection(audio.lock().unwrap().engine.transport()));
        p.register_core(TracksSection::new(audio));
        let text = r#"{"format":"tunekore-project","version":2,"tracks":[{"id":1,"name":"Sampler"},{"id":7,"name":"Keys"}]}"#;
        p.open_text(&(), text, Path::new("/a.tunek"), None, false).unwrap();
        assert_eq!(*saw.lock().unwrap(), vec![1, 7], "track 7 was there when the plugins section applied");
    }

    #[test]
    fn a_failed_clear_leaves_the_new_project_unsaved() {
        let (p, _, _, _) = rig();
        struct Stuck;
        impl Section<()> for Stuck {
            fn key(&self) -> &'static str { "timeline" }
            fn save(&self, _: &(), _: &Path) -> Result<Value, String> { Ok(Value::Null) }
            fn read(&self, _: &(), _: Option<&Value>, _: &Path) -> Result<Apply<()>, String> { Ok(Box::new(|_| Ok(()))) }
            fn clear(&self, _: &()) -> Result<(), String> { Err("a clip is still loading".into()) }
            fn revision(&self, _: &()) -> u64 { 0 }
        }
        p.register(Stuck);
        assert!(p.new_project(&()).unwrap_err().contains("a clip is still loading"));
        assert!(p.is_dirty(&()) && p.path().is_none());
    }

    #[test]
    fn a_panicking_section_does_not_lock_the_project_up() {
        let (p, _, _, _) = rig();
        struct Boom;
        impl Section<()> for Boom {
            fn key(&self) -> &'static str { "midi" }
            fn save(&self, _: &(), _: &Path) -> Result<Value, String> { panic!("boom") }
            fn read(&self, _: &(), _: Option<&Value>, _: &Path) -> Result<Apply<()>, String> { Ok(Box::new(|_| Ok(()))) }
            fn clear(&self, _: &()) -> Result<(), String> { Ok(()) }
            fn revision(&self, _: &()) -> u64 { 0 }
        }
        p.register(Boom);
        let dir = tempfile::tempdir().unwrap();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.save(&(), Some(dir.path().join("a")))));
        assert!(r.is_err(), "the section panicked while `busy` was held");
        assert!(p.open_text(&(), r#"{"format":"tunekore-project","version":2}"#, Path::new("/b.tunek"), None, false).is_ok(), "and the project still opens");
    }

    const V2: &str = r#"{
  "format": "tunekore-project",
  "version": 2,
  "transport": { "tempo": 97.0, "timeSignature": [3, 4] },
  "tracks": [{ "id": 1, "name": "Sampler", "mix": { "volume": 0.5, "pan": -0.25, "mute": false, "solo": true } }],
  "sampler": { "swing": 0.25, "pads": [{ "sample": "kick.wav" }] },
  "timeline": { "clips": [1, 2] },
  "plugins": null,
  "midi": null,
  "later": { "from": "a newer module" }
}"#;

    fn json(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn a_project_round_trips_and_keeps_sections_nobody_reads() {
        let (p, transport, state, _) = rig();
        let file = Path::new("/music/beat.tunek");
        p.open_text(&(), V2, file, Some(file.into()), false).unwrap();
        assert_eq!(transport.tempo(), 97.0);
        assert_eq!(state.lock().unwrap()["swing"], json!(0.25));
        let back = p.serialize(&(), file).unwrap();
        assert_eq!(json(&back), json(V2), "what was opened is what is written, unknown sections and all");
        assert!(!p.is_dirty(&()));
        assert_eq!(p.name(), "beat");
    }

    #[test]
    fn a_new_project_is_clean_from_launch_and_after_new_open_and_save() {
        let (p, _, _, rev) = rig();
        assert!(!p.is_dirty(&()), "nothing changed since launch");
        rev.fetch_add(1, Ordering::Relaxed);
        assert!(p.is_dirty(&()), "an edit");
        p.new_project(&()).unwrap();
        assert!(!p.is_dirty(&()) && p.path().is_none(), "new");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.tunek");
        std::fs::write(&file, V2).unwrap();
        p.open(&(), &file, Some(file.clone()), false).unwrap();
        assert!(!p.is_dirty(&()) && p.path() == Some(file.clone()), "open");
        rev.fetch_add(1, Ordering::Relaxed);
        let saved = p.save(&(), Some(dir.path().join("b"))).unwrap();
        assert_eq!(saved, dir.path().join("b.tunek"), "the extension is added");
        assert!(!p.is_dirty(&()), "saved");
        assert_eq!(json(&std::fs::read_to_string(&saved).unwrap())["version"], json!(2));
    }

    #[test]
    fn a_failed_apply_leaves_the_project_untitled_and_unsaved() {
        let (p, transport, _, _) = rig();
        let (mut fake, _, _) = Fake::new("timeline");
        fake.fail_apply = true;
        p.register(fake);
        let file = PathBuf::from("/music/beat.tunek");
        let err = p.open_text(&(), V2, &file, Some(file.clone()), false).unwrap_err();
        assert!(err.contains("opened only in part") && err.contains("the disk went away"), "{err}");
        assert_eq!(transport.tempo(), 97.0, "the other sections applied");
        assert!(p.path().is_none(), "untitled: Save can't write over beat.tunek");
        assert!(p.is_dirty(&()), "and unsaved");
    }

    #[test]
    fn a_file_that_fails_its_checks_changes_nothing() {
        let (p, transport, state, _) = rig();
        let many: Vec<Value> = (1..=70).map(|i| json!({ "id": i, "name": format!("t{i}") })).collect();
        let text = json!({ "format": FORMAT, "version": 2, "transport": { "tempo": 150.0 }, "tracks": many, "sampler": { "swing": 1 } }).to_string();
        let err = p.open_text(&(), &text, Path::new("/a.tunek"), None, false).unwrap_err();
        assert!(err.contains("at most 64"), "{err}");
        let twice = json!({ "format": FORMAT, "version": 2, "tracks": [{ "id": 3, "name": "a" }, { "id": 3, "name": "b" }] }).to_string();
        assert!(p.open_text(&(), &twice, Path::new("/a.tunek"), None, false).unwrap_err().contains("two tracks numbered 3"));
        assert_eq!(transport.tempo(), 120.0, "no section applied");
        assert_eq!(*state.lock().unwrap(), Value::Null);
    }

    #[test]
    fn track_kinds_save_when_they_differ_from_the_default() {
        let (p, _, _, _) = rig();
        let text = r#"{"format":"tunekore-project","version":2,"tracks":[{"id":1,"name":"Sampler","kind":"audio"},{"id":4,"name":"Drums"},{"id":7,"name":"Keys","kind":"instrument"},{"id":9,"name":"Odd","kind":"sampler"}]}"#;
        p.open_text(&(), text, Path::new("/a.tunek"), None, false).unwrap();
        let saved = json(&p.serialize(&(), Path::new("/a.tunek")).unwrap());
        let kinds: Vec<Value> = saved["tracks"].as_array().unwrap().iter().map(|t| t.get("kind").cloned().unwrap_or(Value::Null)).collect();
        // Track 1 is always the Sampler's, and only it: those and audio, the defaults, aren't written.
        assert_eq!(kinds, vec![Value::Null, Value::Null, json!("instrument"), Value::Null]);
    }

    /// A project with its own engine and the track sections, as a launch of the app has.
    fn launch() -> (Project<()>, &'static Mutex<Audio>) {
        let audio: &'static Mutex<Audio> = Box::leak(Box::new(Mutex::new(Audio::new(tunekore_engine::Engine::new()))));
        let p: Project<()> = Project::new();
        p.register_core(TransportSection(audio.lock().unwrap().engine.transport()));
        p.register_core(TracksSection::new(audio));
        p.register_core(TrackIdsSection(audio));
        (p, audio)
    }

    fn tracks_text(ids: &[u32]) -> String {
        let list: Vec<Value> = ids.iter().map(|&i| json!({ "id": i, "name": format!("t{i}") })).collect();
        json!({ "format": FORMAT, "version": 2, "tracks": list }).to_string()
    }

    #[test]
    fn a_deleted_tracks_id_isnt_given_to_a_new_track_after_reopening() {
        let (p, audio) = launch();
        p.open_text(&(), &tracks_text(&[1, 2, 3]), Path::new("/a.tunek"), None, false).unwrap();
        audio.lock().unwrap().engine.mixer().remove_track(TrackId(3)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = p.save(&(), Some(dir.path().join("a"))).unwrap();
        let saved = json(&std::fs::read_to_string(&file).unwrap());
        assert!(saved["tracks"].is_array(), "the list older builds read");
        assert_eq!(saved["trackIds"], json!({ "next": 4 }));
        // Relaunch, reopen, add a track.
        let (p, audio) = launch();
        p.open(&(), &file, Some(file.clone()), false).unwrap();
        assert_eq!(audio.lock().unwrap().engine.mixer().add_track("New").unwrap(), TrackId(4), "not 3, the deleted track's");
    }

    #[test]
    fn the_next_id_is_the_projects_not_the_sessions() {
        let (p, audio) = launch();
        p.open_text(&(), &tracks_text(&[1, 2, 3, 4]), Path::new("/a.tunek"), None, false).unwrap();
        // Another project with two tracks, in the same session: its own next id.
        p.open_text(&(), &tracks_text(&[1, 2]), Path::new("/b.tunek"), None, false).unwrap();
        let saved = json(&p.serialize(&(), Path::new("/b.tunek")).unwrap());
        assert!(saved["tracks"].is_array());
        assert_eq!(saved["trackIds"], json!({ "next": 3 }));
        p.new_project(&()).unwrap();
        assert_eq!(audio.lock().unwrap().engine.mixer().next_track_id(), TrackId(2), "a new project starts after the Sampler");
    }

    #[test]
    fn a_build_without_track_ids_keeps_the_section_as_it_found_it() {
        // rig() has no trackIds section, as 0.1.12 and 0.1.13 don't.
        let (p, _, _, _) = rig();
        let text = json!({ "format": FORMAT, "version": 2, "tracks": [{ "id": 1, "name": "Sampler" }, { "id": 2, "name": "Bass" }], "trackIds": { "next": 9 } }).to_string();
        p.open_text(&(), &text, Path::new("/a.tunek"), None, false).unwrap();
        assert_eq!(json(&p.serialize(&(), Path::new("/a.tunek")).unwrap())["trackIds"], json!({ "next": 9 }));
    }

    #[test]
    fn exclusive_work_holds_off_a_save() {
        let (p, _, _, _) = rig();
        let p = Arc::new(p);
        let dir = tempfile::tempdir().unwrap();
        let saved = Arc::new(AtomicBool::new(false));
        let t = p.exclusive(|| {
            let (p2, file, saved2) = (p.clone(), dir.path().join("a"), saved.clone());
            let t = std::thread::spawn(move || {
                p2.save(&(), Some(file)).unwrap();
                saved2.store(true, Ordering::Release);
            });
            std::thread::sleep(Duration::from_millis(50));
            assert!(!saved.load(Ordering::Acquire), "the save waits for the exclusive work");
            t
        });
        t.join().unwrap();
        assert!(saved.load(Ordering::Acquire));
    }

    #[test]
    fn a_bad_time_signature_falls_back_to_four_four() {
        let (p, transport, _, _) = rig();
        transport.set_time_signature(7, 8);
        let text = r#"{"format":"tunekore-project","version":2,"transport":{"tempo":120,"timeSignature":[5,3]}}"#;
        p.open_text(&(), text, Path::new("/a.tunek"), None, false).unwrap();
        let sig = transport.time_signature();
        assert_eq!((sig.numerator, sig.denominator), (4, 4), "not the last project's 7/8");
    }

    #[test]
    fn autosave_waits_for_an_answer_about_the_last_session() {
        let (p, _, _, rev) = rig();
        rev.fetch_add(1, Ordering::Relaxed);
        p.recovery_pending.store(true, Ordering::Release);
        assert!(p.autosave_text(&(), Path::new("/r.tunek"), None).is_none(), "the recovery file is not written over");
        p.recovery_pending.store(false, Ordering::Release);
        let (_, written) = p.autosave_text(&(), Path::new("/r.tunek"), None).expect("unsaved work is written");
        assert!(p.autosave_text(&(), Path::new("/r.tunek"), Some(&written)).is_none(), "and not again until it changes");
    }

    #[test]
    fn a_version_1_file_opens_as_one_sampler_track() {
        let old = r#"{"format":"tunekore-project","version":1,"tempo":97.5,"swing":0.25,"steps":12,"pattern_switch":"now","pads":[{"sample":"kick.wav"}]}"#;
        let v2 = parse(old).unwrap();
        assert_eq!(v2["transport"]["tempo"], json!(97.5));
        assert_eq!(v2["transport"]["timeSignature"], json!([4, 4]));
        let tracks: Vec<TrackFile> = serde_json::from_value(v2["tracks"].clone()).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!((tracks[0].id, tracks[0].name.as_str(), tracks[0].mix), (SAMPLER_TRACK, "Sampler", TrackMix::default()));
        let sampler = v2["sampler"].as_object().unwrap();
        assert_eq!(sampler["swing"], json!(0.25));
        assert_eq!(sampler["pattern_switch"], json!("now"));
        assert_eq!(sampler["pads"][0]["sample"], json!("kick.wav"));
        assert!(!sampler.contains_key("tempo"), "the tempo moved to the transport");
        assert!(!sampler.contains_key("format") && !sampler.contains_key("version"));
        let (p, transport, state, _) = rig();
        p.open_text(&(), old, Path::new("/old.tunek"), None, false).unwrap();
        assert_eq!(transport.tempo(), 97.5);
        assert_eq!(state.lock().unwrap()["steps"], json!(12));
    }

    #[test]
    fn newer_foreign_and_badly_versioned_files_are_refused() {
        assert!(parse(r#"{"format":"tunekore-project","version":3}"#).unwrap_err().contains("newer"));
        for bad in [
            r#"{"format":"tunekore-project","version":2.0}"#,
            r#"{"format":"tunekore-project","version":"2"}"#,
            r#"{"format":"tunekore-project","tempo":120}"#,
            r#"{"format":"tunekore-project","version":0}"#,
        ] {
            assert!(parse(bad).unwrap_err().contains("whole number"), "{bad}");
        }
        assert!(parse(r#"{"format":"something-else","version":1}"#).is_err());
        assert!(parse("[1, 2]").is_err());
        assert!(parse("not json").is_err());
    }

    #[test]
    fn the_hooks_are_free_for_their_modules() {
        for key in HOOKS {
            assert!(!["transport", "tracks", "sampler", "format", "version"].contains(&key));
        }
    }

    /// A section whose save is slow enough for an edit to land while it runs.
    struct EditedWhileSaving(Arc<AtomicU64>);

    impl Section<()> for EditedWhileSaving {
        fn key(&self) -> &'static str {
            "timeline"
        }
        fn save(&self, _: &(), _: &Path) -> Result<Value, String> {
            // The value written is the state before this edit.
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(Value::Null)
        }
        fn read(&self, _: &(), _: Option<&Value>, _: &Path) -> Result<Apply<()>, String> {
            Ok(Box::new(|_| Ok(())))
        }
        fn clear(&self, _: &()) -> Result<(), String> {
            Ok(())
        }
        fn revision(&self, _: &()) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn an_edit_that_lands_during_a_save_stays_unsaved() {
        let (p, _, _, _) = rig();
        p.register(EditedWhileSaving(Arc::default()));
        let dir = tempfile::tempdir().unwrap();
        p.save(&(), Some(dir.path().join("a"))).unwrap();
        assert!(p.is_dirty(&()), "the edit isn't in the file, so the project isn't saved");
    }

    #[test]
    fn resuming_without_an_answer_keeps_the_last_sessions_work() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("recovery.tunek"), "last session").unwrap();
        std::fs::write(dir.path().join("recovery.json"), "{}").unwrap();
        set_aside(dir.path(), 1_793_000_000);
        assert!(!dir.path().join("recovery.tunek").exists(), "autosave can write here now");
        assert_eq!(std::fs::read_to_string(dir.path().join("recovery-1793000000.tunek")).unwrap(), "last session");
        assert!(dir.path().join("recovery-1793000000.json").exists());
    }
}
