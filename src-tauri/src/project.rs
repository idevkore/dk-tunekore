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
//! Version 1 files (the Sampler's fields at the top level) open as one
//! Sampler track.
//!
//! Locks: this file's mutexes are leaves. They are never held while a
//! section runs, so a section may lock its own state and the engine.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tauri::{AppHandle, Manager};
use tunekore_engine::{TrackId, TrackInfo, TrackMix, TransportShared, SAMPLER_TRACK};

/// Project files end in `.tunek`.
pub const PROJECT_EXT: &str = "tunek";
const FORMAT: &str = "tunekore-project";
/// The format this build writes. It opens every version up to this one.
pub const VERSION: u32 = 2;
/// Sections kept for the modules still to come. Each registers its own.
pub const HOOKS: [&str; 3] = ["timeline", "plugins", "midi"];
/// How often unsaved work is written to the recovery file.
const AUTOSAVE_EVERY: Duration = Duration::from_secs(20);

/// Puts a module's state into the project file and back.
pub trait Section: Send + Sync + 'static {
    /// The section's key in the file.
    fn key(&self) -> &'static str;

    /// This module's part of a file being written to `file` (so paths can be
    /// made relative to its folder).
    fn save(&self, app: &AppHandle, file: &Path) -> Result<Value, String>;

    /// Reads this module's part of `file`, or `None` when the file has none
    /// (it is older than the module). Do the slow work here (decoding,
    /// loading) without changing anything; the returned step makes the
    /// change. Every section reads before any applies, so a bad file
    /// changes nothing.
    fn read(&self, app: &AppHandle, value: Option<&Value>, file: &Path) -> Result<Apply, String>;

    /// Empties this module for a new project.
    fn clear(&self, app: &AppHandle) -> Result<(), String>;

    /// Changes whenever this module's saved state changes, so the project
    /// knows it has unsaved work and autosave knows when to write. Must not
    /// wait on a lock: it is asked while other locks are held.
    fn revision(&self, app: &AppHandle) -> u64;
}

/// The second half of [`Section::read`]: applies what was read.
pub type Apply = Box<dyn FnOnce(&AppHandle) -> Result<(), String> + Send>;

/// The open project: where it lives, its sections, and what was last saved.
pub struct Project {
    path: Mutex<Option<PathBuf>>,
    sections: Mutex<Vec<Arc<dyn Section>>>,
    /// Sections from the file that no module here reads, written back as they were.
    kept: Mutex<Map<String, Value>>,
    /// Every section's revision at the last save or open; `None` until the
    /// work is saved (a recovered autosave).
    saved: Mutex<Option<Vec<u64>>>,
}

impl Project {
    /// A project with the two core sections: `transport` and `tracks`.
    pub fn new(transport: Arc<TransportShared>) -> Self {
        let sections: Vec<Arc<dyn Section>> = vec![Arc::new(TransportSection(transport)), Arc::new(TracksSection::default())];
        Self { path: Mutex::default(), sections: Mutex::new(sections), kept: Mutex::default(), saved: Mutex::new(Some(Vec::new())) }
    }

    fn sections(&self) -> Vec<Arc<dyn Section>> {
        self.sections.lock().unwrap().clone()
    }
}

fn project(app: &AppHandle) -> tauri::State<'_, Project> {
    app.state::<Project>()
}

/// Adds a module's section. Call it once, from the module's setup.
pub fn register(app: &AppHandle, section: impl Section) {
    let p = project(app);
    let mut list = p.sections.lock().unwrap();
    assert!(list.iter().all(|s| s.key() != section.key()), "two project sections are called {}", section.key());
    list.push(Arc::new(section));
}

/// The project's file, or `None` before it is first saved.
pub fn path(app: &AppHandle) -> Option<PathBuf> {
    project(app).path.lock().unwrap().clone()
}

/// The project's name: its file's name, or "Untitled".
pub fn name(app: &AppHandle) -> String {
    path(app).as_deref().map(file_stem).unwrap_or_else(|| "Untitled".into())
}

fn revisions(app: &AppHandle) -> Vec<u64> {
    project(app).sections().iter().map(|s| s.revision(app)).collect()
}

/// Whether anything changed since the project was last saved or opened.
pub fn is_dirty(app: &AppHandle) -> bool {
    let now = revisions(app);
    project(app).saved.lock().unwrap().as_ref() != Some(&now)
}

fn mark_saved(app: &AppHandle) {
    let now = revisions(app);
    *project(app).saved.lock().unwrap() = Some(now);
}

/// The whole project as file text, for `file`.
fn serialize(app: &AppHandle, file: &Path) -> Result<String, String> {
    let p = project(app);
    let mut obj = p.kept.lock().unwrap().clone();
    obj.insert("format".into(), FORMAT.into());
    obj.insert("version".into(), VERSION.into());
    for key in HOOKS {
        obj.entry(key).or_insert(Value::Null);
    }
    for s in p.sections() {
        obj.insert(s.key().into(), s.save(app, file)?);
    }
    serde_json::to_string_pretty(&Value::Object(obj)).map(|t| t + "\n").map_err(|e| e.to_string())
}

/// Reads file text into its sections, as the current format: checks the
/// format and version, and turns a version 1 file into version 2.
fn parse(text: &str) -> Result<Map<String, Value>, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not a tuneKore project ({e})"))?;
    let Value::Object(mut obj) = value else { return Err("not a tuneKore project".into()) };
    if obj.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err("not a tuneKore project".into());
    }
    let version = obj.get("version").and_then(Value::as_u64).unwrap_or(1);
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
    v2.insert("tracks".into(), json!([TrackFile { id: SAMPLER_TRACK, name: "Sampler".into(), mix: TrackMix::default() }]));
    v2.insert("sampler".into(), Value::Object(obj));
    v2
}

/// Opens `file`. `belongs_to` is the project it belongs to (the file
/// itself, or the original project for a recovered autosave); `dirty`
/// marks the work unsaved. Blocking: call it off the UI thread.
pub fn open(app: &AppHandle, file: &Path, belongs_to: Option<PathBuf>, dirty: bool) -> Result<(), String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("can't read {}: {e}", file.display()))?;
    let mut obj = parse(&text)?;
    let sections = project(app).sections();
    let mut steps = Vec::with_capacity(sections.len());
    for s in &sections {
        // A hook written as null is a section with nothing in it yet.
        steps.push(s.read(app, obj.get(s.key()).filter(|v| !v.is_null()), file)?);
    }
    for step in steps {
        step(app)?;
    }
    for s in &sections {
        obj.remove(s.key());
    }
    obj.retain(|_, v| !v.is_null());
    let p = project(app);
    *p.kept.lock().unwrap() = obj;
    *p.path.lock().unwrap() = belongs_to;
    if dirty {
        *p.saved.lock().unwrap() = None;
    } else {
        mark_saved(app);
    }
    Ok(())
}

/// Empties every module for a new, untitled project.
pub fn new_project(app: &AppHandle) -> Result<(), String> {
    for s in project(app).sections() {
        s.clear(app)?;
    }
    let p = project(app);
    p.kept.lock().unwrap().clear();
    *p.path.lock().unwrap() = None;
    mark_saved(app);
    clear_autosave(app);
    Ok(())
}

/// Saves to `to` (given the `.tunek` extension), or to the project's own
/// file when `to` is `None`. Returns where it saved.
pub fn save(app: &AppHandle, to: Option<PathBuf>) -> Result<PathBuf, String> {
    let mut file = to.or_else(|| path(app)).ok_or("choose where to save the project")?;
    if file.extension().and_then(|e| e.to_str()) != Some(PROJECT_EXT) {
        file.set_extension(PROJECT_EXT);
    }
    let text = serialize(app, &file)?;
    let tmp = file.with_extension(format!("{PROJECT_EXT}.part"));
    std::fs::write(&tmp, text).map_err(|e| format!("can't write {}: {e}", file.display()))?;
    std::fs::rename(&tmp, &file).map_err(|e| e.to_string())?;
    *project(app).path.lock().unwrap() = Some(file.clone());
    mark_saved(app);
    clear_autosave(app);
    Ok(file)
}

// ---- Autosave and recovery ----

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
        let mut written: Option<Vec<u64>> = None;
        loop {
            std::thread::sleep(AUTOSAVE_EVERY);
            let Some(dir) = autosave_dir(&app) else { continue };
            let now = revisions(&app);
            if !is_dirty(&app) || written.as_ref() == Some(&now) {
                continue;
            }
            let file = dir.join("recovery.tunek");
            let meta = Recovery {
                project_name: name(&app),
                project_path: path(&app).map(|p| p.display().to_string()),
                saved_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
            };
            let (Ok(text), Ok(meta)) = (serialize(&app, &file), serde_json::to_string(&meta)) else { continue };
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
    open(app, &file, meta.project_path.map(PathBuf::from), true)
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

/// This file's commands. The Sampler answers new, open, save and recover,
/// since the UI wants its view back from them.
pub fn commands() -> crate::Commands {
    crate::commands![autosave_check, autosave_discard]
}

/// Starts the autosave thread.
pub fn setup(app: &AppHandle) {
    spawn_autosave(app.clone());
}

pub fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Untitled".into())
}

// ---- The core sections ----

/// `transport`: tempo and time signature.
struct TransportSection(Arc<TransportShared>);

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

impl Section for TransportSection {
    fn key(&self) -> &'static str {
        "transport"
    }

    fn save(&self, _: &AppHandle, _: &Path) -> Result<Value, String> {
        let sig = self.0.time_signature();
        serde_json::to_value(TransportFile { tempo: self.0.tempo(), time_signature: [sig.numerator, sig.denominator] }).map_err(|e| e.to_string())
    }

    fn read(&self, _: &AppHandle, value: Option<&Value>, _: &Path) -> Result<Apply, String> {
        let file: TransportFile = match value {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the project's transport is damaged ({e})"))?,
            None => TransportFile::default(),
        };
        let t = self.0.clone();
        Ok(Box::new(move |_| {
            t.stop();
            // The Sampler's tempo box goes from 40 to 300.
            t.set_tempo(file.tempo.clamp(40.0, 300.0));
            t.set_time_signature(file.time_signature[0], file.time_signature[1]);
            t.seek(0.0);
            Ok(())
        }))
    }

    fn clear(&self, _: &AppHandle) -> Result<(), String> {
        self.0.stop();
        self.0.set_tempo(120.0);
        self.0.set_time_signature(4, 4);
        self.0.seek(0.0);
        Ok(())
    }

    fn revision(&self, _: &AppHandle) -> u64 {
        let sig = self.0.time_signature();
        self.0.tempo().to_bits() ^ (u64::from(sig.numerator) << 40) ^ (u64::from(sig.denominator) << 48)
    }
}

/// `tracks`: the track list and each track's mix. A module puts its devices
/// on its tracks from its own section, by track id.
#[derive(Default)]
struct TracksSection {
    /// The mixer's revision when last read, for when the engine is busy.
    last: AtomicU64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackFile {
    id: TrackId,
    name: String,
    #[serde(default)]
    mix: TrackMix,
}

impl TracksSection {
    /// Sets the engine's track list to `tracks`: the Sampler's track always
    /// stays, others are added or removed to match.
    fn apply(app: &AppHandle, tracks: &[TrackFile]) -> Result<(), String> {
        let mut audio = crate::audio(app).lock().unwrap();
        let mixer = audio.engine.mixer();
        let now: Vec<TrackInfo> = mixer.tracks();
        for t in now.iter().filter(|t| t.id != SAMPLER_TRACK && tracks.iter().all(|f| f.id != t.id)) {
            mixer.remove_track(t.id)?;
        }
        if tracks.iter().all(|f| f.id != SAMPLER_TRACK) {
            mixer.set_mix(SAMPLER_TRACK, TrackMix::default())?;
        }
        for f in tracks {
            if mixer.track(f.id).is_none() {
                mixer.add_track_as(f.id, &f.name)?;
            }
            mixer.rename(f.id, &f.name)?;
            mixer.set_mix(f.id, f.mix)?;
        }
        Ok(())
    }
}

impl Section for TracksSection {
    fn key(&self) -> &'static str {
        "tracks"
    }

    fn save(&self, app: &AppHandle, _: &Path) -> Result<Value, String> {
        let tracks: Vec<TrackFile> =
            crate::audio(app).lock().unwrap().engine.tracks().into_iter().map(|t| TrackFile { id: t.id, name: t.name, mix: t.mix }).collect();
        serde_json::to_value(tracks).map_err(|e| e.to_string())
    }

    fn read(&self, _: &AppHandle, value: Option<&Value>, _: &Path) -> Result<Apply, String> {
        let tracks: Vec<TrackFile> = match value {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the project's tracks are damaged ({e})"))?,
            None => Vec::new(),
        };
        Ok(Box::new(move |app| Self::apply(app, &tracks)))
    }

    fn clear(&self, app: &AppHandle) -> Result<(), String> {
        Self::apply(app, &[TrackFile { id: SAMPLER_TRACK, name: "Sampler".into(), mix: TrackMix::default() }])
    }

    fn revision(&self, app: &AppHandle) -> u64 {
        // Never wait for the engine: someone up the stack may hold it.
        if let Ok(mut audio) = crate::audio(app).try_lock() {
            self.last.store(audio.engine.mixer().revision(), Ordering::Relaxed);
        }
        self.last.load(Ordering::Relaxed)
    }
}

/// [`parse`], for other modules' tests.
#[cfg(test)]
pub fn tests_parse(text: &str) -> Map<String, Value> {
    parse(text).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn a_version_2_file_keeps_its_sections_and_newer_or_foreign_files_are_refused() {
        let text = r#"{"format":"tunekore-project","version":2,"transport":{"tempo":140},"tracks":[],"sampler":{},"timeline":{"clips":[1,2]}}"#;
        let obj = parse(text).unwrap();
        assert_eq!(obj["timeline"], json!({"clips": [1, 2]}), "a section nobody here reads survives");
        assert!(!obj.contains_key("format") && !obj.contains_key("version"));
        let file: TransportFile = serde_json::from_value(obj["transport"].clone()).unwrap();
        assert_eq!((file.tempo, file.time_signature), (140.0, [4, 4]), "a missing time signature is 4/4");

        let newer = r#"{"format":"tunekore-project","version":3}"#;
        assert!(parse(newer).unwrap_err().contains("newer"));
        assert!(parse(r#"{"format":"something-else","version":1}"#).is_err());
        assert!(parse("[1, 2]").is_err());
        assert!(parse("not json").is_err());
    }

    #[test]
    fn a_version_1_file_without_a_version_number_still_opens() {
        let obj = parse(r#"{"format":"tunekore-project","tempo":120,"swing":0,"steps":16,"pads":[]}"#).unwrap();
        assert!(obj.contains_key("sampler") && obj.contains_key("tracks"));
    }

    #[test]
    fn the_hooks_are_free_for_their_modules() {
        for key in HOOKS {
            assert!(!["transport", "tracks", "sampler", "format", "version"].contains(&key));
        }
    }
}
