//! The project's `plugins` section: every device, its plugin and its state.
//!
//! ```json
//! "plugins": {
//!   "nextId": 4,
//!   "devices": [
//!     { "id": 3, "track": 1, "role": "pad", "pad": 5, … },
//!     { "id": 1, "track": 2, "role": "instrument", "bypass": false,
//!       "plugin": { "format": "vst3", "name": "Serum 2", "vendor": "Xfer Records",
//!                   "version": "2.1.5", "classId": "…", "path": "/Library/Audio/Plug-Ins/VST3/Serum2.vst3" },
//!       "state": "<base64 of the plugin's own state>" }
//!   ]
//! }
//! ```
//!
//! A track's effects are listed in chain order. A pad device plays a
//! Sampler pad (`pad`, 1 to 16) and sits on the Sampler's track, ahead of its effects. Ids are never reused within
//! a project (`nextId`), so a MIDI mapping to a removed plugin's parameter
//! can't land on a new one. A plugin that won't load when the project opens
//! stays listed as missing, with its state, and is written back as it was.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};
use tunekore_engine::plugins::{vst3, Category, Format, PluginInfo};
use tunekore_engine::{PluginRole, TrackId, TrackKind, PADS, SAMPLER_TRACK};

use super::{base64, kind_of, lock, make_device, mark_pad, placeholder, plugin_folders, publish, refresh_params, remove_at, resolve, state_of, with_doc, Doc, Loaded, Plugins, Role, MAX_STATE};
use crate::project::{Apply, Section};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginFile {
    pub format: Format,
    pub name: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub version: String,
    pub class_id: String,
    pub path: PathBuf,
}

impl PluginFile {
    fn of(info: &PluginInfo) -> Self {
        Self { format: info.format, name: info.name.clone(), vendor: info.vendor.clone(), version: info.version.clone(), class_id: info.class_id.clone(), path: info.path.clone() }
    }

    fn info(&self, role: Role) -> PluginInfo {
        PluginInfo {
            format: self.format,
            name: self.name.clone(),
            vendor: self.vendor.clone(),
            version: self.version.clone(),
            category: match role {
                Role::Effect => Category::Effect,
                Role::Instrument | Role::Pad => Category::Instrument,
            },
            sub_categories: Vec::new(),
            class_id: self.class_id.clone(),
            path: self.path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceFile {
    pub id: u64,
    pub track: TrackId,
    pub role: Role,
    /// The pad (1..=16, as the pads are labelled) a `pad` device plays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pad: Option<u8>,
    pub plugin: PluginFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default)]
    pub bypass: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginsFile {
    #[serde(default)]
    pub next_id: u64,
    #[serde(default)]
    pub devices: Vec<DeviceFile>,
}

/// A device read from a file, its state decoded and checked.
struct Read {
    file: DeviceFile,
    state: Option<Vec<u8>>,
}

/// Reads and checks the section; changes nothing.
fn read_file(value: Option<&Value>) -> Result<(u64, Vec<Read>), String> {
    let file: PluginsFile = match value {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the project's plugins are damaged ({e})"))?,
        None => PluginsFile::default(),
    };
    let mut ids: Vec<u64> = file.devices.iter().map(|d| d.id).collect();
    ids.sort_unstable();
    if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
        return Err(format!("the project has two plugins numbered {}", w[0]));
    }
    for t in file.devices.iter().filter(|d| d.role == Role::Instrument).map(|d| d.track) {
        if file.devices.iter().filter(|d| d.role == Role::Instrument && d.track == t).count() > 1 {
            return Err(format!("the project puts two instruments on track {}", t.0));
        }
    }
    for d in file.devices.iter().filter(|d| d.role == Role::Pad) {
        let ok = d.track == SAMPLER_TRACK && d.pad.is_some_and(|p| (1..=PADS as u8).contains(&p));
        if !ok {
            return Err(format!("the project's plugin {} names no Sampler pad", d.id));
        }
        if file.devices.iter().filter(|o| o.role == Role::Pad && o.pad == d.pad).count() > 1 {
            return Err(format!("the project puts two plugins on pad {}", d.pad.unwrap_or(0)));
        }
    }
    let next = file.next_id.max(ids.last().map_or(1, |i| i + 1));
    let mut out = Vec::with_capacity(file.devices.len());
    for d in file.devices {
        let state = match &d.state {
            Some(s) => {
                let bytes = base64::decode(s).ok_or_else(|| format!("{}'s saved sound is damaged", d.plugin.name))?;
                if bytes.len() > MAX_STATE {
                    return Err(format!("{}'s saved sound is too large", d.plugin.name));
                }
                Some(bytes)
            }
            None => None,
        };
        out.push(Read { file: d, state });
    }
    Ok((next, out))
}

/// Writes the document as the section, on the main thread. Devices whose
/// track is gone (deleted in the Timeline, maybe to come back with an undo)
/// are left out.
///
/// A plugin that won't give its sound keeps the sound it had in the project,
/// says so on its card, and is named once (`plugins:unsaved`) until a save
/// works again.
fn write(app: &AppHandle, doc: &mut Doc) -> PluginsFile {
    let tracks: Vec<TrackId> = lock(crate::audio(app)).engine.tracks().into_iter().map(|t| t.id).collect();
    let running = super::engine_rate(app) > 0;
    let mut newly = Vec::new();
    let mut changed = false;
    let devices = doc
        .slots
        .iter_mut()
        .filter(|s| tracks.contains(&s.track))
        .map(|s| {
            let before = s.save_error.is_some();
            let state = state_of(s, running).map(|b| base64::encode(&b));
            if before != s.save_error.is_some() {
                changed = true;
                if !before {
                    newly.push(s.info.name.clone());
                }
            }
            let pad = (s.role == Role::Pad).then_some(s.pad + 1);
            DeviceFile { id: s.id, track: s.track, role: s.role, pad, plugin: PluginFile::of(&s.info), state, bypass: s.bypass }
        })
        .collect();
    let file = PluginsFile { next_id: doc.next_id.max(1), devices };
    if changed {
        publish(app, doc);
    }
    if !newly.is_empty() {
        let _ = app.emit("plugins:unsaved", &newly);
    }
    file
}

/// Main thread: empties the document and the tracks of plugins.
fn clear_all(app: &AppHandle, doc: &mut Doc) {
    while !doc.slots.is_empty() {
        if remove_at(app, doc, 0).is_err() {
            doc.slots.remove(0);
        }
    }
}

/// Main thread: puts the file's devices on their tracks. A plugin that
/// won't load stays as a missing device. Returns the names that didn't load.
fn apply(app: &AppHandle, doc: &mut Doc, next: u64, devices: Vec<Read>) -> Vec<String> {
    clear_all(app, doc);
    doc.next_id = next;
    let tracks = lock(crate::audio(app)).engine.tracks();
    let rate = super::engine_rate(app);
    let catalog = lock(&app.state::<Plugins>().catalog).cache.clone();
    let folders = plugin_folders();
    let mut missing = Vec::new();
    for Read { file, state } in devices {
        let Some(track) = tracks.iter().find(|t| t.id == file.track) else {
            missing.push(format!("{} (its track is gone)", file.plugin.name));
            continue;
        };
        if file.role == Role::Instrument && track.kind != TrackKind::Instrument {
            missing.push(format!("{} (its track isn't an instrument track)", file.plugin.name));
            continue;
        }
        let named = file.plugin.info(file.role);
        let mut slot = placeholder(file.id, file.track, file.role, named.clone());
        slot.pad = file.pad.map_or(0, |p| p.saturating_sub(1));
        slot.bypass = file.bypass;
        slot.saved = state.clone();
        // Only from the scan's list or a plugin folder: a project file can't
        // point tuneKore at code anywhere else.
        let loaded = resolve(catalog.as_ref(), &named, &folders).and_then(|info| vst3::load(&info).map(|c| (info, c)).map_err(|e| e.to_string()));
        match loaded {
            Ok((info, mut controller)) => {
                slot.info = info;
                if let Some(s) = &state {
                    if let Err(e) = controller.load_state(s) {
                        // It plays its own sound; the saved one is kept and written back.
                        slot.sound_lost = Some(e.to_string());
                        missing.push(format!("{} (its saved sound didn't load)", file.plugin.name));
                    }
                }
                if file.role != Role::Effect && kind_of(controller.info()) != Role::Instrument {
                    slot.error = Some("it isn't an instrument any more".into());
                }
                let role = match file.role {
                    Role::Instrument => PluginRole::Instrument,
                    Role::Effect => PluginRole::Effect,
                    Role::Pad => PluginRole::Pad(slot.pad),
                };
                match make_device(app, file.id, role, &mut *controller, rate) {
                    Ok((device, link)) => {
                        link.set_bypass(file.bypass);
                        let index = doc.chain_index_at(file.track, doc.slots.len());
                        let put = slot.put(lock(crate::audio(app)).engine.mixer(), device, index);
                        match put {
                            Ok(()) => {
                                if slot.role == Role::Pad {
                                    mark_pad(app, slot.pad, true);
                                }
                                slot.loaded = Some(Loaded { controller, link });
                            }
                            Err(e) => {
                                slot.error = Some(e);
                                missing.push(file.plugin.name.clone());
                            }
                        }
                    }
                    Err(e) => {
                        slot.error = Some(e);
                        missing.push(file.plugin.name.clone());
                    }
                }
            }
            Err(e) => {
                slot.error = Some(e);
                missing.push(file.plugin.name.clone());
            }
        }
        doc.slots.push(slot);
        let last = doc.slots.len() - 1;
        refresh_params(app, &mut doc.slots[last]);
    }
    missing
}

/// The `plugins` section of the project.
pub struct PluginsSection {
    revision: Arc<AtomicU64>,
}

impl PluginsSection {
    pub fn new(revision: Arc<AtomicU64>) -> Self {
        Self { revision }
    }
}

impl Section<AppHandle> for PluginsSection {
    fn key(&self) -> &'static str {
        "plugins"
    }

    /// Asks every plugin for its state, on the main thread.
    fn save(&self, app: &AppHandle, _: &Path) -> Result<Value, String> {
        let file = with_doc(app, write)?;
        serde_json::to_value(file).map_err(|e| e.to_string())
    }

    fn read(&self, _: &AppHandle, value: Option<&Value>, _: &Path) -> Result<Apply<AppHandle>, String> {
        let (next, devices) = read_file(value)?;
        Ok(Box::new(move |app| {
            let missing = with_doc(app, move |app, d| {
                let missing = apply(app, d, next, devices);
                app.state::<Plugins>().live.set_focus(None);
                publish(app, d);
                missing
            })?;
            if !missing.is_empty() {
                let _ = app.emit("plugins:missing", &missing);
            }
            Ok(())
        }))
    }

    fn clear(&self, app: &AppHandle) -> Result<(), String> {
        with_doc(app, |app, d| {
            clear_all(app, d);
            d.next_id = 1;
            app.state::<Plugins>().live.set_focus(None);
            publish(app, d);
        })
    }

    fn revision(&self, _: &AppHandle) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn device(id: u64, track: u32, role: &str) -> Value {
        json!({ "id": id, "track": track, "role": role, "plugin": { "format": "vst3", "name": "Serum 2", "classId": "ABC", "path": "/Library/Audio/Plug-Ins/VST3/Serum2.vst3" }, "state": "Zm9v" })
    }

    #[test]
    fn reads_devices_and_their_state() {
        let (next, read) = read_file(Some(&json!({ "nextId": 2, "devices": [device(5, 2, "instrument"), device(6, 2, "effect")] }))).unwrap();
        assert_eq!(next, 7, "past the highest id");
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].state.as_deref(), Some(&b"foo"[..]));
        assert_eq!(read[1].file.role, Role::Effect);
        assert!(!read[0].file.bypass);
        assert_eq!(read_file(None).map(|(n, r)| (n, r.len())), Ok((1, 0)), "a project from before plugins");
    }

    #[test]
    fn a_pad_device_names_its_pad_on_the_samplers_track() {
        let mut pad = device(7, 1, "pad");
        pad["pad"] = json!(5);
        let (_, read) = read_file(Some(&json!({ "devices": [pad.clone()] }))).unwrap();
        assert_eq!((read[0].file.role, read[0].file.pad), (Role::Pad, Some(5)));
        let mut elsewhere = pad.clone();
        elsewhere["track"] = json!(2);
        assert!(read_file(Some(&json!({ "devices": [elsewhere] }))).is_err(), "only the Sampler's track has pads");
        let mut no_pad = pad.clone();
        no_pad["pad"] = json!(17);
        assert!(read_file(Some(&json!({ "devices": [no_pad] }))).is_err());
        let mut twice = pad.clone();
        twice["id"] = json!(8);
        assert!(read_file(Some(&json!({ "devices": [pad, twice] }))).is_err(), "two on one pad");
    }

    #[test]
    fn a_damaged_section_opens_nothing() {
        assert!(read_file(Some(&json!({ "devices": [device(1, 2, "instrument"), device(1, 3, "instrument")] }))).is_err(), "the same id twice");
        assert!(read_file(Some(&json!({ "devices": [device(1, 2, "instrument"), device(2, 2, "instrument")] }))).is_err(), "two instruments on one track");
        let mut bad = device(1, 2, "effect");
        bad["state"] = json!("not base64!");
        assert!(read_file(Some(&json!({ "devices": [bad] }))).is_err());
        assert!(read_file(Some(&json!({ "devices": [{ "id": 1 }] }))).is_err());
    }

    #[test]
    fn writes_back_what_it_reads() {
        let f = PluginsFile {
            next_id: 3,
            devices: vec![DeviceFile {
                id: 2,
                track: TrackId(4),
                role: Role::Effect,
                pad: None,
                plugin: PluginFile { format: Format::Vst3, name: "Guitar Rig 7".into(), vendor: "Native Instruments".into(), version: "7".into(), class_id: "X".into(), path: "/a.vst3".into() },
                state: Some(base64::encode(b"state")),
                bypass: true,
            }],
        };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["devices"][0]["plugin"]["classId"], "X");
        assert_eq!(serde_json::from_value::<PluginsFile>(v).unwrap(), f);
    }
}
