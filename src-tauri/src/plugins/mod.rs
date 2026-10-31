//! Plugins in the app (#37): VST3 instruments and effects on tracks, their
//! editors in windows of their own, the plugin list, live notes from a MIDI
//! controller, and their state in the project's `plugins` section.
//!
//! **Threads.** A plugin's controller (state, parameters, editor) is used on
//! the main thread only, as VST3 asks, so the document here ([`Doc`]) is only
//! ever locked there. Tauri runs this module's synchronous commands on the
//! main thread; work that starts elsewhere (saving the project, the engine
//! asking for a plugin at a new rate, MIDI learn) reaches it through
//! [`with_doc`] (which waits) or [`later`] (which doesn't). A plugin can run the main thread's event loop
//! inside a call (a modal licence dialog), so code here never assumes the
//! document is free on the main thread: it tries the lock, and work that
//! finds it taken is queued again.
//!
//! **Locks.** The document, then the engine (`crate::audio`). Nothing that
//! holds the engine's lock waits for the main thread: the engine's devices
//! ask for activation through a callback that only queues ([`PluginLink`]).
//!
//! **The audio side** is `tunekore_engine::plugin_device`: an instrument in
//! a track's source slot, effects in its chain. Each device here has a
//! [`PluginLink`] that the main thread hands activated plugins and live notes.

mod base64;
#[cfg(debug_assertions)]
mod check;
mod live;
mod section;
mod targets;
mod window;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, TryLockError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tunekore_engine::graph::MAX_FRAMES;
use tunekore_engine::plugins::scan::{self, ScanCache, ScanOptions, ScanStatus};
use tunekore_engine::plugins::{vst3, BusSelection, Category, ChannelTable, ControllerEvent, EditorSize, OutBuf, PluginController, PluginInfo, ProcessBlock, ProcessSetup, Transport};
use tunekore_engine::{plugin_device, plugin_device_for_pad, Device, Mixer, PluginLink, PluginRole, SamplerHandle, TrackId, TrackInfo, TrackKind, PADS, SAMPLER_TRACK};

pub use live::{control_live, has_focus as live_has_focus, play_live};
use window::{EditorWindow, Place, WindowEvent};

/// How often the main thread looks at every plugin's news (editor edits, failures).
const POLL_EVERY: Duration = Duration::from_millis(100);
/// The rate plugins are activated at before an output opens.
const FALLBACK_RATE: u32 = 48_000;
/// How long saving waits for the audio thread to hand a plugin the values set from the UI.
const FLUSH_WAIT: Duration = Duration::from_millis(300);
/// How long work sent to the main thread may take before the caller gives up waiting.
const MAIN_WAIT: Duration = Duration::from_secs(60);
/// An editor's size before it says its own.
const FIRST_SIZE: EditorSize = EditorSize { width: 480, height: 320 };

// ---- The main thread ----

static MAIN: OnceLock<ThreadId> = OnceLock::new();

fn on_main_thread() -> bool {
    MAIN.get() == Some(&std::thread::current().id())
}

/// Runs `f` with the document on the main thread once it is free: queued
/// now, and queued again while a plugin call further down the main thread
/// holds it. Never waits.
fn later(app: &AppHandle, f: impl FnOnce(&AppHandle, &mut Doc) + Send + 'static) {
    let a = app.clone();
    let _ = app.run_on_main_thread(move || {
        let state = a.state::<Plugins>();
        let r = state.doc.try_lock();
        match r {
            Ok(mut doc) => f(&a, &mut doc),
            Err(TryLockError::Poisoned(p)) => f(&a, &mut p.into_inner()),
            Err(TryLockError::WouldBlock) => {
                drop(r);
                later(&a, f);
            }
        }
    });
}

/// Runs `f` with the document on the main thread and waits for what it
/// returns, queued again while a plugin call holds the document. Off the
/// main thread only (on it, the document may be held further down the
/// stack, and waiting would never end); the caller must hold no lock the
/// main thread may wait for.
fn with_doc<R: Send + 'static>(app: &AppHandle, f: impl FnOnce(&AppHandle, &mut Doc) -> R + Send + 'static) -> Result<R, String> {
    if on_main_thread() {
        let state = app.state::<Plugins>();
        let mut d = doc(&state)?;
        return Ok(f(app, &mut d));
    }
    let (tx, rx) = mpsc::sync_channel(1);
    later(app, move |app, d| {
        let _ = tx.send(f(app, d));
    });
    rx.recv_timeout(MAIN_WAIT).map_err(|_| "the main thread didn't answer (a plugin may be showing a dialog)".to_string())
}

/// The document, for a command on the main thread. A plugin call further
/// down this thread may hold it (a dialog it shows runs the event loop).
fn doc(state: &Plugins) -> Result<std::sync::MutexGuard<'_, Doc>, String> {
    match state.doc.try_lock() {
        Ok(d) => Ok(d),
        Err(TryLockError::Poisoned(p)) => Ok(p.into_inner()),
        Err(TryLockError::WouldBlock) => Err("a plugin is busy: close its dialog and try again".into()),
    }
}

// ---- The document ----

/// A plugin's place on its track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The instrument in an instrument track's source slot.
    Instrument,
    /// An effect in a track's chain.
    Effect,
    /// The instrument a Sampler pad plays ([`Slot::pad`]): in the Sampler
    /// track's chain, ahead of its effects.
    Pad,
}

/// A plugin that loaded: its controller and the link to its device.
struct Loaded {
    controller: Box<dyn PluginController>,
    link: Arc<PluginLink>,
}

/// One device on a track.
struct Slot {
    /// Stable within a project and saved: MIDI targets name it.
    id: u64,
    track: TrackId,
    role: Role,
    /// The pad (0..16) a `Pad` device plays.
    pad: u8,
    info: PluginInfo,
    /// `None` while the plugin is missing (it didn't load).
    loaded: Option<Loaded>,
    /// The state from the project, kept while the plugin is missing and
    /// written back unchanged; and what Reload starts from.
    saved: Option<Vec<u8>>,
    /// Why it is missing, or how it failed.
    error: Option<String>,
    bypass: bool,
    window: Option<EditorWindow>,
    /// Where its window was last, while the app runs.
    place: Option<Place>,
    /// The window's title, to see when the track's name changes.
    title: String,
}

impl Slot {
    /// Whether its device sits in the track's chain (an effect, or a pad's
    /// instrument) rather than its source slot.
    fn in_chain(&self) -> bool {
        self.role != Role::Instrument
    }

    fn engine_role(&self) -> PluginRole {
        match self.role {
            Role::Instrument => PluginRole::Instrument,
            Role::Effect => PluginRole::Effect,
            Role::Pad => PluginRole::Pad(self.pad),
        }
    }

    /// Puts `device` in its place on the track: the source slot, or `index` in the chain.
    fn put(&self, mixer: &mut Mixer, device: Box<dyn Device>, index: usize) -> Result<(), String> {
        match self.role {
            Role::Instrument => mixer.set_source(self.track, Some(device)),
            _ => mixer.insert(self.track, index, device),
        }
    }

    /// Takes its device off the track (it was at `index` in the chain).
    fn take(&self, mixer: &mut Mixer, index: usize) -> Result<(), String> {
        if mixer.track(self.track).is_none() {
            return Ok(());
        }
        match self.role {
            Role::Instrument => mixer.set_source(self.track, None),
            _ => mixer.remove(self.track, index),
        }
    }

    fn status(&self) -> &'static str {
        match &self.loaded {
            None => "missing",
            Some(l) if l.link.failed() => "failed",
            Some(_) => "ok",
        }
    }
}

#[derive(Default)]
pub struct Doc {
    /// Every device, in order: a track's effects run in the order they appear here.
    slots: Vec<Slot>,
    next_id: u64,
}

impl Doc {
    fn find(&self, id: u64) -> Result<usize, String> {
        self.slots.iter().position(|s| s.id == id).ok_or_else(|| format!("there is no plugin {id}"))
    }

    /// Where the effect `i` sits in its track's chain in the engine.
    fn chain_index(&self, i: usize) -> usize {
        self.chain_index_at(self.slots[i].track, i)
    }

    /// Where a device at document place `at` on `track` goes in the engine's
    /// chain: after the loaded chain devices before it (a missing one has no
    /// device there). On the Sampler's track the pads' instruments come
    /// first, then the effects.
    fn chain_index_at(&self, track: TrackId, at: usize) -> usize {
        self.slots[..at.min(self.slots.len())].iter().filter(|o| o.track == track && o.in_chain() && o.loaded.is_some()).count()
    }

    /// The document place for a new pad device: after the Sampler track's other pads, before its effects.
    fn pad_place(&self) -> usize {
        self.slots.iter().position(|s| s.track == SAMPLER_TRACK && s.role == Role::Effect).unwrap_or(self.slots.len())
    }

    fn pad_device(&self, pad: u8) -> Option<usize> {
        self.slots.iter().position(|s| s.role == Role::Pad && s.pad == pad)
    }

    /// The document places of `track`'s effects, in order.
    fn effects(&self, track: TrackId) -> Vec<usize> {
        self.slots.iter().enumerate().filter(|(_, s)| s.track == track && s.role == Role::Effect).map(|(i, _)| i).collect()
    }

    fn instrument_of(&self, track: TrackId) -> Option<usize> {
        self.slots.iter().position(|s| s.track == track && s.role == Role::Instrument)
    }

    fn take_id(&mut self) -> u64 {
        self.next_id = self.next_id.max(1);
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// The plugin list from the scanner.
#[derive(Default)]
struct Catalog {
    cache: Option<ScanCache>,
    scanning: bool,
    error: Option<String>,
}

/// The module's state in the app.
pub struct Plugins {
    doc: Mutex<Doc>,
    catalog: Mutex<Catalog>,
    /// Bumped by every change the project saves.
    revision: Arc<AtomicU64>,
    /// Parameters by device, for MIDI learn from any thread (see `targets`).
    params: Arc<targets::ParamCache>,
    live: Arc<live::LiveRoute>,
    /// Set once the app is quitting: no more work starts.
    quitting: AtomicBool,
}

impl Default for Plugins {
    fn default() -> Self {
        Self {
            doc: Mutex::default(),
            catalog: Mutex::default(),
            revision: Arc::default(),
            params: Arc::default(),
            live: live::route(),
            quitting: AtomicBool::new(false),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Plugins {
    fn edited(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
    }
}

// ---- What the UI sees ----

/// One device, for the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    id: u64,
    track: u32,
    role: Role,
    /// The pad a `pad` device plays, 0..16.
    pad: u8,
    name: String,
    vendor: String,
    /// `ok`, `missing` or `failed`.
    status: &'static str,
    error: Option<String>,
    bypass: bool,
    has_editor: bool,
    editor_open: bool,
    /// Frames, as the plugin reports it (delay compensation comes with the Mixer).
    latency: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginsView {
    devices: Vec<DeviceView>,
    /// The track whose instrument a MIDI controller's keys play.
    focus: Option<u32>,
}

fn view(state: &Plugins, doc: &mut Doc) -> PluginsView {
    let devices = doc
        .slots
        .iter_mut()
        .map(|s| {
            let status = s.status();
            let (has_editor, latency) = match &mut s.loaded {
                Some(l) => (l.controller.has_editor(), l.controller.latency_samples()),
                None => (false, 0),
            };
            DeviceView {
                id: s.id,
                track: s.track.0,
                role: s.role,
                pad: s.pad,
                name: s.info.name.clone(),
                vendor: s.info.vendor.clone(),
                status,
                error: s.error.clone(),
                bypass: s.bypass,
                has_editor,
                editor_open: s.window.is_some(),
                latency,
            }
        })
        .collect();
    PluginsView { devices, focus: state.live.focus().map(|t| t.0) }
}

fn publish(app: &AppHandle, doc: &mut Doc) -> PluginsView {
    let v = view(&app.state::<Plugins>(), doc);
    let _ = app.emit("plugins:state", &v);
    v
}

/// One plugin in the list to add from.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    path: String,
    class_id: String,
    name: String,
    vendor: String,
    version: String,
    kind: Category,
    sub_categories: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedEntry {
    name: String,
    path: String,
    status: ScanStatus,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogView {
    plugins: Vec<CatalogEntry>,
    failed: Vec<FailedEntry>,
    scanning: bool,
    error: Option<String>,
}

fn catalog_view(c: &Catalog) -> CatalogView {
    let (mut plugins, failed) = match &c.cache {
        Some(cache) => (
            cache
                .plugins()
                .filter(|e| vst3::is_hostable(e.info.category))
                .map(|e| CatalogEntry {
                    path: e.info.path.display().to_string(),
                    class_id: e.info.class_id.clone(),
                    name: e.info.name.clone(),
                    vendor: e.info.vendor.clone(),
                    version: e.info.version.clone(),
                    kind: e.info.category,
                    sub_categories: e.info.sub_categories.clone(),
                })
                .collect::<Vec<_>>(),
            cache
                .failures()
                .map(|e| FailedEntry { name: e.info.name.clone(), path: e.info.path.display().to_string(), status: e.status, error: e.error.clone() })
                .collect(),
        ),
        None => (Vec::new(), Vec::new()),
    };
    plugins.sort_by_key(|p| (p.name.to_lowercase(), p.vendor.to_lowercase()));
    CatalogView { plugins, failed, scanning: c.scanning, error: c.error.clone() }
}

// ---- Scanning ----

fn cache_path(app: &AppHandle) -> PathBuf {
    app.path().app_data_dir().map(|d| d.join("plugin-cache.json")).unwrap_or_else(|_| scan::default_cache_path())
}

/// How a scan treats what the cache already knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanMode {
    /// Probes only new or changed bundles.
    Update,
    /// Probes every bundle again.
    Rescan,
    /// Gives the plugins that failed another try.
    Retry,
}

/// Scans in the background (the scanner is a helper process) and tells the
/// windows when it is done. A scan already running is left to finish.
fn spawn_scan(app: &AppHandle, mode: ScanMode) {
    let state = app.state::<Plugins>();
    {
        let mut c = lock(&state.catalog);
        if c.scanning {
            return;
        }
        c.scanning = true;
        let _ = app.emit("plugins:catalog", catalog_view(&c));
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let opts = ScanOptions { cache_path: cache_path(&app), ..ScanOptions::default() };
        if let Some(dir) = opts.cache_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let r = match mode {
            ScanMode::Update => scan::scan(&opts),
            ScanMode::Rescan => scan::rescan(&opts),
            ScanMode::Retry => scan::retry_failed(&opts),
        };
        let state = app.state::<Plugins>();
        let mut c = lock(&state.catalog);
        c.scanning = false;
        match r {
            Ok(cache) => {
                c.cache = Some(cache);
                c.error = None;
            }
            Err(e) => {
                // Keep what an earlier scan found, if anything.
                if c.cache.is_none() {
                    c.cache = ScanCache::read(&opts.cache_path);
                }
                c.error = Some(e.to_string());
            }
        }
        let _ = app.emit("plugins:catalog", catalog_view(&c));
    });
}

/// The plugin `path` and `class_id` name, from the scan or, failing that,
/// from the bundle itself.
fn find_info(app: &AppHandle, path: &Path, class_id: &str) -> Result<PluginInfo, String> {
    let known = lock(&app.state::<Plugins>().catalog)
        .cache
        .as_ref()
        .and_then(|c| c.plugins().find(|e| e.info.path == path && e.info.class_id.eq_ignore_ascii_case(class_id)).map(|e| e.info.clone()));
    match known {
        Some(i) => Ok(i),
        None => vst3::classes(path)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|i| i.class_id.eq_ignore_ascii_case(class_id))
            .ok_or_else(|| format!("{} has no such plugin", path.display())),
    }
}

// ---- Activation ----

fn setup_at(rate: u32) -> ProcessSetup {
    ProcessSetup {
        sample_rate: f64::from(rate),
        max_block_size: MAX_FRAMES as u32,
        realtime: true,
        inputs: BusSelection::Main,
        outputs: BusSelection::Main,
        main_channels: 2,
    }
}

/// Main thread: activates slot `i`'s plugin at the rate its device runs at
/// and hands it over. Called when the device asks (a new output rate), and
/// after a latency change or a reload request.
fn activate(app: &AppHandle, doc: &mut Doc, i: usize, force: bool) {
    let s = &mut doc.slots[i];
    let Some(l) = &mut s.loaded else { return };
    let rate = l.link.rate();
    if rate == 0 || (l.link.is_ready() && !force) {
        return;
    }
    l.link.collect();
    match l.controller.activate(&setup_at(rate)) {
        Ok(p) => {
            l.link.give(p, rate);
            if s.error.take().is_some() {
                publish(app, doc);
            }
        }
        Err(e) => {
            s.error = Some(e.to_string());
            publish(app, doc);
        }
    }
}

/// The callback a device calls from `prepare`: queues its activation.
fn requester(app: &AppHandle, id: u64) -> Box<dyn Fn() + Send + Sync> {
    let app = app.clone();
    Box::new(move || {
        later(&app, move |app, doc| {
            if let Ok(i) = doc.find(id) {
                activate(app, doc, i, false);
            }
        });
    })
}

/// Makes a device in `role` for `controller`, activated at `rate` (0: not
/// yet; the device asks once it is prepared). Main thread.
fn make_device(app: &AppHandle, id: u64, role: PluginRole, controller: &mut dyn PluginController, rate: u32) -> Result<(Box<dyn Device>, Arc<PluginLink>), String> {
    let (mut device, link) = match role {
        PluginRole::Pad(pad) => {
            let pads = app.state::<SamplerHandle>().inner().clone();
            plugin_device_for_pad(pad, Box::new(move || pads.pad_gains(usize::from(pad))), requester(app, id))
        }
        _ => plugin_device(role, requester(app, id)),
    };
    if rate > 0 {
        let p = controller.activate(&setup_at(rate)).map_err(|e| e.to_string())?;
        device.install(p, rate);
    }
    Ok((Box::new(device), link))
}

/// Main thread: makes sure parameter values set from the UI or the editor
/// have reached the plugin's processor, so its saved state has them.
fn flush(l: &mut Loaded) {
    if !l.controller.has_pending_params() {
        return;
    }
    if l.link.is_ready() {
        // A running device delivers them on its next block.
        let until = Instant::now() + FLUSH_WAIT;
        while l.controller.has_pending_params() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !l.controller.has_pending_params() {
            return;
        }
    }
    // No block is coming (no output): a fresh processor takes them in a zero-frame block.
    let rate = if l.link.rate() > 0 { l.link.rate() } else { FALLBACK_RATE };
    let Ok(mut p) = l.controller.activate(&setup_at(rate)) else { return };
    let layout = p.layout().clone();
    let (ins, mut outs) = (vec![Vec::<f32>::new(); layout.input_channels()], vec![Vec::<f32>::new(); layout.output_channels()]);
    let mut table = ChannelTable::default();
    let (inputs, outputs) = table.audio(&ins, &mut outs);
    let (mut midi, mut params) = (OutBuf::with_capacity(1), OutBuf::with_capacity(1));
    let t = Transport::default();
    let mut block = ProcessBlock { frames: 0, inputs, outputs, events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    let _ = p.process(&mut block);
    if l.link.rate() == rate {
        l.link.give(p, rate);
    }
}

/// Main thread: the plugin's state now, or what was kept for a missing one.
fn state_of(s: &mut Slot) -> Option<Vec<u8>> {
    match &mut s.loaded {
        Some(l) => {
            flush(l);
            l.controller.save_state().ok().or_else(|| s.saved.clone())
        }
        None => s.saved.clone(),
    }
}

// ---- Editors ----

fn track_name(app: &AppHandle, track: TrackId) -> String {
    lock(crate::audio(app)).engine.tracks().into_iter().find(|t| t.id == track).map(|t| t.name).unwrap_or_default()
}

fn title(track: &str, plugin: &str) -> String {
    if track.is_empty() { plugin.to_string() } else { format!("{track} · {plugin}") }
}

fn open_editor(app: &AppHandle, doc: &mut Doc, i: usize) -> Result<(), String> {
    let track = track_name(app, doc.slots[i].track);
    let s = &mut doc.slots[i];
    match &s.window {
        Some(w) if !w.is_closed() => {
            w.raise();
            return Ok(());
        }
        // Closed by the user a moment ago, before the module heard: done with it first.
        Some(_) => close_editor(s),
        None => {}
    }
    let Some(l) = &mut s.loaded else { return Err(format!("{} isn't loaded", s.info.name)) };
    if !l.controller.has_editor() {
        return Err(format!("{} has no window of its own", s.info.name));
    }
    let id = s.id;
    let a = app.clone();
    let on_event = Box::new(move |e: WindowEvent| {
        later(&a, move |app, doc| window_event(app, doc, id, e));
    });
    s.title = title(&track, &s.info.name);
    let w = EditorWindow::new(&s.title, FIRST_SIZE, s.place, on_event)?;
    let handle = w.handle();
    match l.controller.open_editor(w.parent(), Box::new(move |size| handle.resize(size))) {
        Ok(size) => {
            w.resize(size);
            w.set_resizable(l.controller.editor_resizable());
            w.raise();
            s.window = Some(w);
            Ok(())
        }
        Err(e) => {
            w.close();
            Err(e.to_string())
        }
    }
}

fn close_editor(s: &mut Slot) {
    if let Some(w) = s.window.take() {
        if let Some(l) = &mut s.loaded {
            l.controller.close_editor();
        }
        s.place = Some(w.place());
        w.close();
    }
}

fn window_event(app: &AppHandle, doc: &mut Doc, id: u64, e: WindowEvent) {
    let Ok(i) = doc.find(id) else { return };
    let s = &mut doc.slots[i];
    match e {
        WindowEvent::Closed => {
            // Heard late: a window opened since is not the one that closed.
            if !s.window.as_ref().is_some_and(EditorWindow::is_closed) {
                return;
            }
            // The window is closing itself: the editor goes first, then our hold on it.
            if let Some(l) = &mut s.loaded {
                l.controller.close_editor();
            }
            if let Some(w) = s.window.take() {
                s.place = Some(w.place());
                drop(w);
            }
            publish(app, doc);
        }
        WindowEvent::Resized(size) => {
            let (Some(l), Some(w)) = (&mut s.loaded, &s.window) else { return };
            if let Some(took) = l.controller.resize_editor(size) {
                if took != size {
                    w.resize(took);
                }
            }
        }
    }
}

// ---- Edits ----

/// What to add: a plugin from the list.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginRef {
    pub path: String,
    pub class_id: String,
}

/// Marks pad `pad` as playing a plugin (its hits become notes) or its sample.
fn mark_pad(app: &AppHandle, pad: u8, on: bool) {
    app.state::<SamplerHandle>().set_plugin_pad(usize::from(pad), on);
}

fn kind_of(info: &PluginInfo) -> Role {
    match info.category {
        Category::Instrument => Role::Instrument,
        _ => Role::Effect,
    }
}

/// Puts the devices back on the tracks after the engine lost them (a
/// stream that died without handing its devices home).
fn restore_lost(app: &AppHandle, doc: &mut Doc, tracks: &[TrackInfo]) {
    let lost: Vec<usize> = doc
        .slots
        .iter()
        .enumerate()
        .filter(|(_, s)| s.loaded.is_some())
        .filter(|(i, s)| {
            let Some(t) = tracks.iter().find(|t| t.id == s.track) else { return false };
            if s.in_chain() { doc.chain_index(*i) >= t.chain } else { !t.has_source }
        })
        .map(|(i, _)| i)
        .collect();
    if lost.is_empty() {
        return;
    }
    for i in lost {
        let _ = reinstall(app, doc, i);
    }
}

/// Main thread: makes slot `i`'s device again (its controller stays) and
/// puts it in its place on the track.
fn reinstall(app: &AppHandle, doc: &mut Doc, i: usize) -> Result<(), String> {
    let rate = engine_rate(app);
    let index = doc.chain_index(i);
    let s = &mut doc.slots[i];
    let role = s.engine_role();
    let Some(l) = &mut s.loaded else { return Ok(()) };
    let (device, link) = make_device(app, s.id, role, &mut *l.controller, rate)?;
    link.set_bypass(s.bypass);
    app.state::<Plugins>().live.retarget(s.track, &l.link, &link);
    l.link = link;
    let mut a = lock(crate::audio(app));
    let mixer = a.engine.mixer();
    match s.role {
        Role::Instrument => mixer.restore_source(s.track, device)?,
        _ => mixer.insert(s.track, index, device)?,
    }
    Ok(())
}

/// The output's rate, or 0 while none is open.
fn engine_rate(app: &AppHandle) -> u32 {
    let a = lock(crate::audio(app));
    if a.engine.is_open() { a.engine.output_rate() } else { 0 }
}

/// Where a plugin goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// An instrument, on a new instrument track named after it.
    NewTrack,
    /// An instrument track's instrument (replacing the one there), or an
    /// effect in a track's chain at a place (the end by default).
    Track(TrackId, Option<usize>),
    /// The instrument a Sampler pad plays (replacing the one there).
    Pad(u8),
}

/// Main thread: loads `info` and puts it where `target` says.
fn add(app: &AppHandle, doc: &mut Doc, info: PluginInfo, target: Target) -> Result<u64, String> {
    let kind = kind_of(&info);
    let tracks = lock(crate::audio(app)).engine.tracks();
    let (role, track, pad) = match target {
        Target::Pad(p) => {
            if usize::from(p) >= PADS {
                return Err(format!("there is no pad {}", p + 1));
            }
            if kind != Role::Instrument {
                return Err("a pad plays an instrument; put effects on the Sampler's track".into());
            }
            (Role::Pad, Some(SAMPLER_TRACK), p)
        }
        Target::NewTrack if kind != Role::Instrument => return Err("choose a track for the effect".into()),
        Target::NewTrack => {
            if tracks.len() >= tunekore_engine::track::TRACK_SLOTS {
                return Err(format!("tuneKore runs at most {} tracks", tunekore_engine::track::TRACK_SLOTS));
            }
            (Role::Instrument, None, 0)
        }
        Target::Track(t, _) => {
            let Some(found) = tracks.iter().find(|x| x.id == t) else { return Err(format!("there is no track {}", t.0)) };
            if kind == Role::Instrument && found.kind != TrackKind::Instrument {
                return Err(match found.kind {
                    TrackKind::Sampler => "an instrument goes on an instrument track or a pad, not on the Sampler's track".into(),
                    _ => "an instrument goes on an instrument track; audio tracks play clips".into(),
                });
            }
            (kind, Some(t), 0)
        }
    };
    let mut controller = vst3::load(&info).map_err(|e| e.to_string())?;
    // A pad's instrument goes in place of the one it had.
    if role == Role::Pad {
        if let Some(old) = doc.pad_device(pad) {
            remove_at(app, doc, old)?;
        }
    }
    let id = doc.take_id();
    let engine_role = match role {
        Role::Instrument => PluginRole::Instrument,
        Role::Effect => PluginRole::Effect,
        Role::Pad => PluginRole::Pad(pad),
    };
    let rate = engine_rate(app);
    let (device, link) = make_device(app, id, engine_role, &mut *controller, rate)?;

    // Its place in the document: an effect at `index` among its track's
    // effects, a pad's instrument after the other pads', an instrument last.
    let at = match (role, track, target) {
        (Role::Effect, Some(t), Target::Track(_, index)) => {
            let effects = doc.effects(t);
            match index {
                Some(n) if n < effects.len() => effects[n],
                _ => effects.last().map_or(doc.slots.len(), |&i| i + 1),
            }
        }
        (Role::Pad, _, _) => doc.pad_place(),
        _ => doc.slots.len(),
    };
    let mut replaced = None;
    let track = {
        let mut a = lock(crate::audio(app));
        let mixer = a.engine.mixer();
        match (role, track) {
            (Role::Instrument, None) => {
                let t = mixer.add_track(&info.name)?;
                mixer.set_kind(t, TrackKind::Instrument)?;
                mixer.set_source(t, Some(device))?;
                t
            }
            (Role::Instrument, Some(t)) => {
                replaced = doc.instrument_of(t);
                mixer.set_source(t, Some(device))?;
                t
            }
            (_, Some(t)) => {
                mixer.insert(t, doc.chain_index_at(t, at), device)?;
                t
            }
            (_, None) => return Err("choose a track for the effect".into()),
        }
    };
    let state = app.state::<Plugins>();
    let mut at = at;
    if let Some(old) = replaced {
        // Its device is gone from the track already (replaced in the source slot).
        let mut s = doc.slots.remove(old);
        if old < at {
            at -= 1;
        }
        close_editor(&mut s);
        state.params.forget(s.id);
        if let Some(l) = &s.loaded {
            state.live.retarget(track, &l.link, &link);
        }
        drop(s);
    }
    if role == Role::Pad {
        mark_pad(app, pad, true);
    }
    let at = at.min(doc.slots.len());
    let slot = Slot { pad, loaded: Some(Loaded { controller, link }), ..placeholder(id, track, role, info) };
    doc.slots.insert(at, slot);
    let i = doc.find(id)?;
    refresh_params(app, &mut doc.slots[i]);
    state.edited();
    if target == Target::NewTrack {
        crate::timeline::tracks_changed(app);
    }
    Ok(id)
}

fn placeholder(id: u64, track: TrackId, role: Role, info: PluginInfo) -> Slot {
    Slot { id, track, role, pad: 0, info, loaded: None, saved: None, error: None, bypass: false, window: None, place: None, title: String::new() }
}

/// Main thread: takes slot `i` off its track and drops its plugin here.
fn remove_at(app: &AppHandle, doc: &mut Doc, i: usize) -> Result<(), String> {
    if doc.slots[i].loaded.is_some() {
        let index = doc.chain_index(i);
        let s = &doc.slots[i];
        s.take(lock(crate::audio(app)).engine.mixer(), index)?;
    }
    let mut s = doc.slots.remove(i);
    if s.role == Role::Pad {
        mark_pad(app, s.pad, false);
    }
    close_editor(&mut s);
    let state = app.state::<Plugins>();
    state.params.forget(s.id);
    if let Some(l) = &s.loaded {
        state.live.forget(&l.link);
    }
    // The controller goes here, on the main thread; its processor may still
    // be in the engine for a block, where it is now stale and silent.
    drop(s);
    state.edited();
    Ok(())
}

fn refresh_params(app: &AppHandle, s: &mut Slot) {
    let state = app.state::<Plugins>();
    match &mut s.loaded {
        Some(l) => state.params.store(s.id, &s.info.name, s.track, &mut *l.controller),
        None => state.params.forget(s.id),
    }
}

/// Main thread, every [`POLL_EVERY`]: what each plugin did (edits in its
/// window, latency changes, failures), and window titles that follow renames.
fn poll(app: &AppHandle, doc: &mut Doc) {
    let state = app.state::<Plugins>();
    let tracks = lock(crate::audio(app)).engine.tracks();
    let names: HashMap<TrackId, &str> = tracks.iter().map(|t| (t.id, t.name.as_str())).collect();
    state.params.track_names(&names);
    let mut changed = false;
    let mut events = Vec::new();
    for i in 0..doc.slots.len() {
        let s = &mut doc.slots[i];
        // Its track was deleted (in the Timeline): its window goes; the plugin
        // waits, unsaved, in case an undo brings the track back.
        if !names.contains_key(&s.track) {
            if s.window.is_some() {
                close_editor(s);
                changed = true;
            }
            continue;
        }
        let Some(l) = &mut s.loaded else { continue };
        l.link.collect();
        events.clear();
        l.controller.poll(&mut events);
        let mut reactivate = false;
        for e in &events {
            match e {
                ControllerEvent::ParamEdit { id, value } => {
                    state.params.set_value(s.id, *id, *value);
                    state.edited();
                }
                ControllerEvent::EditEnd { .. } => state.edited(),
                ControllerEvent::ParamsChanged => {
                    state.params.store(s.id, &s.info.name, s.track, &mut *l.controller);
                    state.edited();
                }
                ControllerEvent::LatencyChanged | ControllerEvent::ReloadRequested => reactivate = true,
                ControllerEvent::Failed => {
                    s.error = l.controller.take_error().or_else(|| Some("the plugin failed".into()));
                    changed = true;
                }
                ControllerEvent::EditBegin { .. } => {}
            }
        }
        if reactivate {
            if events.contains(&ControllerEvent::ReloadRequested) {
                l.controller.refresh_params();
                state.params.store(s.id, &s.info.name, s.track, &mut *l.controller);
            }
            activate(app, doc, i, true);
            changed = true;
        }
        let s = &mut doc.slots[i];
        if let (Some(w), Some(name)) = (&s.window, names.get(&s.track)) {
            let t = title(name, &s.info.name);
            if t != s.title {
                w.set_title(&t);
                s.title = t;
            }
        }
    }
    restore_lost(app, doc, &tracks);
    if changed {
        publish(app, doc);
    }
}

fn spawn_poll(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(POLL_EVERY);
        let state = app.state::<Plugins>();
        if state.quitting.load(Ordering::Relaxed) {
            break;
        }
        let a = app.clone();
        let _ = app.run_on_main_thread(move || {
            let state = a.state::<Plugins>();
            // Skipped, not queued again, if a plugin call below holds the document.
            let held = state.doc.try_lock();
            if let Ok(mut doc) = held {
                poll(&a, &mut doc);
            }
        });
    });
}

// ---- Commands (synchronous ones run on the main thread) ----

fn track_id(id: u32) -> TrackId {
    TrackId(id)
}

#[tauri::command]
pub fn plugins_state(app: AppHandle) -> Result<PluginsView, String> {
    let state = app.state::<Plugins>();
    let mut d = doc(&state)?;
    Ok(view(&state, &mut d))
}

#[tauri::command]
pub fn plugins_catalog(app: AppHandle) -> CatalogView {
    catalog_view(&lock(&app.state::<Plugins>().catalog))
}

/// Scans for plugins again in the background; `plugins:catalog` says when it is done.
#[tauri::command]
pub fn plugins_scan(mode: ScanMode, app: AppHandle) {
    spawn_scan(&app, mode);
}

/// Adds a plugin from the list: on `pad` (0..16) when given, else on
/// `track` (an effect at `index`), else on a new instrument track. See [`add`].
#[tauri::command]
pub fn plugins_add(plugin: PluginRef, track: Option<u32>, index: Option<usize>, pad: Option<u8>, app: AppHandle) -> Result<PluginsView, String> {
    let info = find_info(&app, Path::new(&plugin.path), &plugin.class_id)?;
    let state = app.state::<Plugins>();
    let mut d = doc(&state)?;
    let target = match (pad, track) {
        (Some(p), _) => Target::Pad(p),
        (None, Some(t)) => Target::Track(track_id(t), index),
        (None, None) => Target::NewTrack,
    };
    let r = add(&app, &mut d, info, target);
    let v = publish(&app, &mut d);
    r.map(|_| v)
}

#[tauri::command]
pub fn plugins_remove(id: u64, app: AppHandle) -> Result<PluginsView, String> {
    let state = app.state::<Plugins>();
    let mut d = doc(&state)?;
    let i = d.find(id)?;
    remove_at(&app, &mut d, i)?;
    Ok(publish(&app, &mut d))
}

/// Removes an instrument track with its instrument and effects.
#[tauri::command]
pub fn plugins_remove_track(track: u32, app: AppHandle) -> Result<PluginsView, String> {
    let track = track_id(track);
    let state = app.state::<Plugins>();
    let mut d = doc(&state)?;
    let kind = lock(crate::audio(&app)).engine.tracks().into_iter().find(|t| t.id == track).map(|t| t.kind);
    if kind != Some(TrackKind::Instrument) {
        return Err("only an instrument track is removed here".into());
    }
    while let Some(i) = d.slots.iter().position(|s| s.track == track) {
        remove_at(&app, &mut d, i)?;
    }
    lock(crate::audio(&app)).engine.mixer().remove_track(track)?;
    state.live.unfocus(track);
    crate::timeline::tracks_changed(&app);
    Ok(publish(&app, &mut d))
}

#[tauri::command]
pub fn plugins_bypass(id: u64, on: bool, app: AppHandle) -> Result<PluginsView, String> {
    let state = app.state::<Plugins>();
    let mut d = doc(&state)?;
    let i = d.find(id)?;
    let s = &mut d.slots[i];
    if s.bypass != on {
        s.bypass = on;
        if let Some(l) = &s.loaded {
            l.link.set_bypass(on);
        }
        state.edited();
    }
    Ok(publish(&app, &mut d))
}

/// Moves an effect to place `to` in its track's chain.
#[tauri::command]
pub fn plugins_move(id: u64, to: usize, app: AppHandle) -> Result<PluginsView, String> {
    let state = app.state::<Plugins>();
    let mut d = doc(&state)?;
    let i = d.find(id)?;
    if d.slots[i].role != Role::Effect {
        return Err("only effects move along the chain".into());
    }
    let track = d.slots[i].track;
    let effects = d.effects(track);
    let from = effects.iter().position(|&n| n == i).unwrap_or(0);
    let to = to.min(effects.len() - 1);
    if from == to {
        return Ok(publish(&app, &mut d));
    }
    let engine_from = d.chain_index(i);
    let s = d.slots.remove(i);
    let loaded = s.loaded.is_some();
    // Its new place in the document: before the effect now at `to`, or after the last.
    let rest = d.effects(track);
    let at = rest.get(to).copied().unwrap_or_else(|| rest.last().map_or(d.slots.len(), |&n| n + 1));
    d.slots.insert(at, s);
    if loaded {
        let engine_to = d.chain_index(at);
        lock(crate::audio(&app)).engine.mixer().move_in_chain(track, engine_from, engine_to)?;
    }
    state.edited();
    Ok(publish(&app, &mut d))
}

/// Opens (raises) or closes a plugin's window.
#[tauri::command]
pub fn plugins_editor(id: u64, open: bool, app: AppHandle) -> Result<PluginsView, String> {
    let state = app.state::<Plugins>();
    let mut d = doc(&state)?;
    let i = d.find(id)?;
    let r = if open {
        open_editor(&app, &mut d, i)
    } else {
        close_editor(&mut d.slots[i]);
        Ok(())
    };
    let v = publish(&app, &mut d);
    r.map(|()| v)
}

/// Loads a missing or failed plugin again, from the state last saved or opened.
#[tauri::command]
pub fn plugins_reload(id: u64, app: AppHandle) -> Result<PluginsView, String> {
    let state = app.state::<Plugins>();
    let mut d = doc(&state)?;
    let i = d.find(id)?;
    let r = reload(&app, &mut d, i);
    let v = publish(&app, &mut d);
    r.map(|()| v)
}

/// Main thread: loads slot `i`'s plugin afresh with its kept state and puts it on its track.
fn reload(app: &AppHandle, doc: &mut Doc, i: usize) -> Result<(), String> {
    // Off the track first, whatever it was.
    if doc.slots[i].loaded.is_some() {
        let index = doc.chain_index(i);
        let s = &mut doc.slots[i];
        close_editor(s);
        s.take(lock(crate::audio(app)).engine.mixer(), index)?;
        if let Some(l) = s.loaded.take() {
            app.state::<Plugins>().live.forget(&l.link);
        }
    }
    let s = &mut doc.slots[i];
    let mut controller = vst3::load(&s.info).map_err(|e| {
        s.error = Some(e.to_string());
        e.to_string()
    })?;
    if let Some(state) = &s.saved {
        if let Err(e) = controller.load_state(state) {
            s.error = Some(format!("its saved sound didn't load: {e}"));
        }
    }
    let rate = engine_rate(app);
    let (device, link) = make_device(app, s.id, s.engine_role(), &mut *controller, rate)?;
    link.set_bypass(s.bypass);
    if s.role == Role::Instrument {
        app.state::<Plugins>().live.focus_if(s.track, &link);
    }
    s.loaded = Some(Loaded { controller, link });
    s.error = None;
    let index = doc.chain_index(i);
    let s = &mut doc.slots[i];
    s.put(lock(crate::audio(app)).engine.mixer(), device, index)?;
    if s.role == Role::Pad {
        mark_pad(app, s.pad, true);
    }
    refresh_params(app, s);
    Ok(())
}

/// The track whose instrument a MIDI controller's keys play, or none (the pads).
#[tauri::command]
pub fn plugins_focus(track: Option<u32>, app: AppHandle) -> Result<PluginsView, String> {
    let state = app.state::<Plugins>();
    let mut d = doc(&state)?;
    let link = track.map(track_id).and_then(|t| d.instrument_of(t).and_then(|i| d.slots[i].loaded.as_ref().map(|l| (t, l.link.clone()))));
    state.live.set_focus(link);
    Ok(publish(&app, &mut d))
}

pub fn commands() -> crate::Commands {
    crate::commands![
        plugins_state,
        plugins_catalog,
        plugins_scan,
        plugins_add,
        plugins_remove,
        plugins_remove_track,
        plugins_bypass,
        plugins_move,
        plugins_editor,
        plugins_reload,
        plugins_focus,
    ]
}

/// The plugins' part of the app: its state, its project section, its MIDI
/// targets, the first scan and the poll. Runs on the main thread.
pub fn setup(app: &AppHandle) {
    let _ = MAIN.set(std::thread::current().id());
    app.manage(Plugins::default());
    let state = app.state::<Plugins>();
    // What the last scan found, at once; then look for new or changed plugins.
    lock(&state.catalog).cache = ScanCache::read(&cache_path(app));
    spawn_scan(app, ScanMode::Update);
    project::register(app, section::PluginsSection::new(state.revision.clone()));
    crate::midi::register_targets(app, targets::PluginTargets::new(state.params.clone()));
    spawn_poll(app);
    #[cfg(debug_assertions)]
    check::start(app);
}

/// Main thread, as the app quits: closes every editor and takes every
/// plugin off its track and drops it, closes the output so the engine lets
/// go of their processors, then lets the plugin modules run their exit code
/// (`vst3::shutdown`; Serum 2 crashes at exit without it).
pub fn quit(app: &AppHandle) {
    let Some(state) = app.try_state::<Plugins>() else { return };
    state.quitting.store(true, Ordering::Relaxed);
    state.live.set_focus(None);
    {
        let mut d = lock(&state.doc);
        while !d.slots.is_empty() {
            if remove_at(app, &mut d, 0).is_err() {
                d.slots.remove(0);
            }
        }
    }
    {
        let mut a = lock(crate::audio(app));
        a.engine.close_output();
        a.engine.mixer().collect();
    }
    vst3::shutdown();
}

use crate::project;
