//! MIDI controllers: notes C1 to D#2 (36 to 51) play pads 1 to 16, and MIDI
//! learn maps any knob or fader to a control, saved with the project.
//!
//! A background thread connects to every MIDI input port and looks again
//! every two seconds, so plugging a controller in just works. The midir
//! callback runs on the driver's thread; it only parses the message, plays
//! pads through the Sampler's atomics, and hands control changes to the
//! mapping thread through a channel, so it never blocks.
//!
//! The mapping thread ([`spawn_mapper`]) learns and applies control
//! changes: a burst of them is taken together (the latest value of each
//! knob wins), mapped onto each control's range ([`map`]), and set through
//! the control's module ([`targets`]): Sampler controls as undoable edits,
//! one undo step per gesture, the master level and track mix straight to
//! the engine's atomics. The windows hear about it at most 30 times a second.
//!
//! The mappings are the project's `midi` section ([`MidiSection`]); a
//! project without one opens with none.

pub mod map;
pub mod targets;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use midir::{MidiInput, MidiInputConnection};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, State};
use tunekore_engine::{SamplerHandle, PADS};

use crate::project;
use map::{Cc, Mapper, Mapping, Move, Source, TargetInfo, Takeover};
use targets::{MasterTarget, Registry, SamplerTargets, TrackTargets};
pub use targets::Targets;

/// The note that plays pad 1.
pub const FIRST_NOTE: u8 = 36;
const SCAN_EVERY: Duration = Duration::from_secs(2);
/// The activity event goes out at most this often (about 30 a second).
const ACTIVITY_EVERY: Duration = Duration::from_millis(33);
const NO_PAD: i32 = -1;
/// After a control change arrives, the mapping thread gathers the ones
/// that follow for this long, so a fast knob sets each control once.
const GATHER: Duration = Duration::from_millis(2);
/// The windows hear about controls moved by a knob at most this often.
const TELL_EVERY: Duration = Duration::from_millis(33);

/// What a MIDI message does to a pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadEvent {
    /// A note on, with its velocity 1..=127.
    On { pad: usize, velocity: u8 },
    Off { pad: usize },
}

/// Reads one raw MIDI message. Only notes 36 to 51 on any channel count; a
/// note on with velocity 0 is a note off. Everything else is ignored.
pub fn parse(msg: &[u8]) -> Option<PadEvent> {
    let (&status, rest) = msg.split_first()?;
    let (&note, rest) = rest.split_first()?;
    let pad = note.checked_sub(FIRST_NOTE)? as usize;
    if pad >= PADS {
        return None;
    }
    match status & 0xF0 {
        0x90 => match rest.first().copied().unwrap_or(0) {
            0 => Some(PadEvent::Off { pad }),
            velocity => Some(PadEvent::On { pad, velocity }),
        },
        0x80 => Some(PadEvent::Off { pad }),
        _ => None,
    }
}

/// A MIDI input port, as the UI lists it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MidiInputInfo {
    pub name: String,
    pub connected: bool,
}

/// A control change and when it arrived.
type Arrived = (Instant, Cc);

pub struct MidiState {
    inputs: Mutex<Vec<MidiInputInfo>>,
    /// The last pad played since the activity thread looked, or `NO_PAD`.
    activity: AtomicI32,
    mapper: Arc<Mutex<Mapper>>,
    /// Every module's targets. Modules register theirs from their setup.
    targets: Mutex<Registry<AppHandle>>,
    /// Changes whenever the mappings do, for the project.
    revision: Arc<AtomicU64>,
    /// The pad selected in the Sampler: `sampler.pad.selected.*` moves it.
    focus: Arc<AtomicUsize>,
    /// Control changes, from the midir callbacks to the mapping thread.
    changes: Sender<Arrived>,
    receiver: Mutex<Option<Receiver<Arrived>>>,
}

impl MidiState {
    fn new() -> Self {
        let (changes, receiver) = mpsc::channel();
        Self {
            inputs: Mutex::default(),
            activity: AtomicI32::new(NO_PAD),
            mapper: Arc::default(),
            targets: Mutex::default(),
            revision: Arc::default(),
            focus: Arc::default(),
            changes,
            receiver: Mutex::new(Some(receiver)),
        }
    }

    fn registry(&self) -> Registry<AppHandle> {
        self.targets.lock().unwrap().clone()
    }
}

/// Plays and releases pads from a midir callback, and passes control
/// changes on. Touches only atomics and the channel.
fn handle(pads: &SamplerHandle, state: &MidiState, changes: &Sender<Arrived>, msg: &[u8]) {
    match parse(msg) {
        Some(PadEvent::On { pad, velocity }) => {
            pads.trigger_velocity(pad, velocity as f32 / 127.0);
            state.activity.store(pad as i32, Ordering::Relaxed);
        }
        Some(PadEvent::Off { pad }) => pads.release(pad),
        None => {
            if let Some(cc) = map::parse_cc(msg) {
                let _ = changes.send((Instant::now(), cc));
            }
        }
    }
}

/// The ports now present, as (id, name).
fn scan() -> Result<Vec<(String, String)>, String> {
    // A new client each time: CoreMIDI only reports a plugged-in device to a client made after it.
    let input = MidiInput::new("tuneKore scan").map_err(|e| e.to_string())?;
    Ok(input
        .ports()
        .iter()
        .filter_map(|p| Some((p.id(), input.port_name(p).ok()?)))
        .collect())
}

fn connect(id: &str, name: &str, pads: SamplerHandle, state: Arc<MidiState>) -> Result<MidiInputConnection<()>, String> {
    let input = MidiInput::new("tuneKore").map_err(|e| e.to_string())?;
    let port = input.find_port_by_id(id).ok_or("the port is gone")?;
    let changes = state.changes.clone();
    input
        .connect(&port, name, move |_stamp, msg, _| handle(&pads, &state, &changes, msg), ())
        .map_err(|e| e.to_string())
}

/// Connects to every input now and every two seconds after, and sends
/// `midi:activity` (the pad's index) at most 30 times a second.
pub fn spawn(app: AppHandle, pads: SamplerHandle, state: Arc<MidiState>) {
    let st = state.clone();
    std::thread::spawn(move || {
        let mut open: HashMap<String, MidiInputConnection<()>> = HashMap::new();
        loop {
            match scan() {
                Ok(ports) => {
                    // Drop the ones that went away, connect the new ones.
                    open.retain(|id, _| ports.iter().any(|(p, _)| p == id));
                    for (id, name) in &ports {
                        if !open.contains_key(id) {
                            if let Ok(conn) = connect(id, name, pads.clone(), st.clone()) {
                                open.insert(id.clone(), conn);
                            }
                        }
                    }
                    let list = ports
                        .iter()
                        .map(|(id, name)| MidiInputInfo { name: name.clone(), connected: open.contains_key(id) })
                        .collect();
                    *st.inputs.lock().unwrap() = list;
                }
                Err(e) => {
                    open.clear();
                    st.inputs.lock().unwrap().clear();
                    eprintln!("no MIDI: {e}");
                }
            }
            std::thread::sleep(SCAN_EVERY);
        }
    });
    std::thread::spawn(move || loop {
        std::thread::sleep(ACTIVITY_EVERY);
        let pad = state.activity.swap(NO_PAD, Ordering::Relaxed);
        if pad != NO_PAD {
            let _ = app.emit("midi:activity", pad);
        }
    });
}

pub fn state() -> Arc<MidiState> {
    Arc::new(MidiState::new())
}

// ---- The mapping thread ----

/// What one batch of control changes did.
#[derive(Debug, Default)]
struct Outcome {
    learned: Vec<Mapping>,
    /// The providers that moved something (by place in the registry).
    touched: Vec<usize>,
    moves: Vec<Move>,
    errors: Vec<String>,
}

/// Learns from and applies one batch of control changes. `now` completes a
/// learn that waited for a 14-bit partner. Generic so tests can drive it
/// with targets of their own.
fn run<C: 'static>(mapper: &mut Mapper, registry: &Registry<C>, ctx: &C, batch: &[Arrived], now: Instant) -> Outcome {
    let mut out = Outcome::default();
    for &(at, cc) in batch {
        out.learned.extend(mapper.feed(at, cc));
    }
    out.learned.extend(mapper.tick(now));
    let controls = registry.with(ctx);
    let moves = mapper.moves(&controls);
    if !moves.is_empty() {
        let (touched, errors) = registry.apply(ctx, &moves, &mapper.gesture_key());
        mapper.settle(&moves, &controls);
        out.touched = touched;
        out.errors = errors;
        out.moves = moves;
    }
    out
}

/// A control moved by a knob, for the windows (`midi:moved`).
#[derive(Debug, Clone, Serialize)]
pub struct Moved {
    target: String,
    value: f64,
}

/// Learns and applies control changes as they arrive; see the module docs.
fn spawn_mapper(app: AppHandle, state: Arc<MidiState>) {
    let Some(rx) = state.receiver.lock().unwrap().take() else { return };
    std::thread::spawn(move || {
        let mut told = Instant::now();
        let mut to_tell: Vec<usize> = Vec::new();
        let mut moved: Vec<Moved> = Vec::new();
        loop {
            // Wake for a control change, a learn's 14-bit wait, or the next time to tell the windows.
            let tell_at = (!to_tell.is_empty() || !moved.is_empty()).then(|| told + TELL_EVERY);
            let wake = [state.mapper.lock().unwrap().deadline(), tell_at].into_iter().flatten().min();
            let first = match wake {
                Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return,
                },
            };
            let mut batch: Vec<Arrived> = first.into_iter().collect();
            if !batch.is_empty() {
                let until = Instant::now() + GATHER;
                while let Ok(m) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                    batch.push(m);
                }
            }
            let registry = state.registry();
            let out = {
                let mut mapper = state.mapper.lock().unwrap();
                run(&mut mapper, &registry, &app, &batch, Instant::now())
            };
            for e in &out.errors {
                eprintln!("MIDI mapping: {e}");
            }
            if !out.learned.is_empty() {
                state.revision.fetch_add(1, Ordering::Relaxed);
                for m in &out.learned {
                    let _ = app.emit("midi:learned", mapping_view(&registry, &app, m));
                }
                tell_mappings(&app);
                // The project has unsaved changes now: the title says so.
                crate::sampler::publish_now(&app);
            }
            for t in out.touched {
                if !to_tell.contains(&t) {
                    to_tell.push(t);
                }
            }
            for m in out.moves {
                moved.retain(|x| x.target != m.target);
                moved.push(Moved { target: m.target, value: m.value });
            }
            if (!to_tell.is_empty() || !moved.is_empty()) && told.elapsed() >= TELL_EVERY {
                registry.changed(&app, &to_tell);
                let _ = app.emit("midi:moved", &moved);
                to_tell.clear();
                moved.clear();
                told = Instant::now();
            }
        }
    });
}

// ---- What the windows see ----

/// One mapping, as the UI lists it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MappingView {
    target: String,
    /// "Cutoff"; the target's id when it doesn't exist now.
    label: String,
    /// "Pad 3"; empty when it doesn't exist now.
    group: String,
    source: Source,
    /// "Ch 1 · CC 74".
    controller: String,
    /// Whether the target exists now (a removed plugin's doesn't).
    available: bool,
}

/// The mappings, and the control being learned.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MidiView {
    mappings: Vec<MappingView>,
    learning: Option<String>,
    takeover: Takeover,
}

fn mapping_view<C: 'static>(registry: &Registry<C>, ctx: &C, m: &Mapping) -> MappingView {
    let info = registry.info(ctx, &m.target);
    MappingView {
        target: m.target.clone(),
        label: info.as_ref().map_or_else(|| m.target.clone(), |i| i.label.clone()),
        group: info.as_ref().map_or_else(String::new, |i| i.group.clone()),
        source: m.source,
        controller: m.source.describe(),
        available: info.is_some(),
    }
}

fn view(app: &AppHandle) -> MidiView {
    let state = app.state::<Arc<MidiState>>();
    let registry = state.registry();
    let mapper = state.mapper.lock().unwrap();
    MidiView {
        mappings: mapper.mappings().iter().map(|m| mapping_view(&registry, app, m)).collect(),
        learning: mapper.learning().map(String::from),
        takeover: mapper.takeover,
    }
}

/// Sends the mappings to every window, and returns them.
fn tell_mappings(app: &AppHandle) -> MidiView {
    let v = view(app);
    let _ = app.emit("midi:mappings", &v);
    v
}

/// The mappings changed: the project has unsaved work, and the windows hear.
fn mappings_changed(app: &AppHandle) -> MidiView {
    app.state::<Arc<MidiState>>().revision.fetch_add(1, Ordering::Relaxed);
    let v = tell_mappings(app);
    crate::sampler::publish_now(app);
    v
}

// ---- Commands ----

/// The MIDI inputs found so far, and whether each is connected.
#[tauri::command]
pub fn midi_inputs(state: State<'_, Arc<MidiState>>) -> Vec<MidiInputInfo> {
    state.inputs.lock().unwrap().clone()
}

/// The mappings, and the control being learned.
#[tauri::command]
pub fn midi_mappings(app: AppHandle) -> MidiView {
    view(&app)
}

/// Every control a knob can learn now.
#[tauri::command]
pub fn midi_targets(app: AppHandle) -> Vec<TargetInfo> {
    app.state::<Arc<MidiState>>().registry().list(&app)
}

/// Learns `target`: the next knob or fader moved maps to it. `None` stops learning.
#[tauri::command]
pub fn midi_learn(target: Option<String>, app: AppHandle) -> Result<MidiView, String> {
    let state = app.state::<Arc<MidiState>>();
    if let Some(t) = &target {
        if state.registry().info(&app, t).is_none() {
            return Err(format!("there is no control called {t} to map"));
        }
    }
    state.mapper.lock().unwrap().learn(target);
    Ok(tell_mappings(&app))
}

/// Forgets the mapping on `target`.
#[tauri::command]
pub fn midi_forget(target: String, app: AppHandle) -> MidiView {
    let gone = app.state::<Arc<MidiState>>().mapper.lock().unwrap().forget(&target);
    if gone { mappings_changed(&app) } else { view(&app) }
}

/// Tells the mapping which pad is selected, for `sampler.pad.selected.*`.
#[tauri::command]
pub fn midi_focus(pad: usize, state: State<'_, Arc<MidiState>>) {
    state.focus.store(pad.min(PADS - 1), Ordering::Relaxed);
}

/// What a knob does when it and its control disagree. A preference of this
/// Mac, kept by the UI, not part of the project.
#[tauri::command]
pub fn midi_takeover(mode: Takeover, app: AppHandle) -> MidiView {
    app.state::<Arc<MidiState>>().mapper.lock().unwrap().takeover = mode;
    tell_mappings(&app)
}

/// The MIDI commands.
pub fn commands() -> crate::Commands {
    crate::commands![midi_inputs, midi_mappings, midi_targets, midi_learn, midi_forget, midi_focus, midi_takeover]
}

/// Adds a module's controls to what knobs can learn, under its id prefix
/// (`plugin.<id>.<param>` for #37). Call it once, from the module's setup.
#[allow(dead_code)] // for the modules still to come
pub fn register_targets(app: &AppHandle, targets: impl Targets<AppHandle>) {
    app.state::<Arc<MidiState>>().targets.lock().unwrap().register(Arc::new(targets));
}

// ---- The project's `midi` section ----

/// `midi`: the mappings, as `{ "mappings": [{ "target", "channel", "cc", "bits"? }] }`.
/// A project without it (older ones, or `null`) opens with none.
pub struct MidiSection<C> {
    mapper: Arc<Mutex<Mapper>>,
    revision: Arc<AtomicU64>,
    /// Tells the windows the mappings changed.
    notify: fn(&C),
}

impl<C: 'static> project::Section<C> for MidiSection<C> {
    fn key(&self) -> &'static str {
        "midi"
    }

    fn save(&self, _: &C, _: &Path) -> Result<Value, String> {
        Ok(self.mapper.lock().unwrap().to_file())
    }

    fn read(&self, _: &C, value: Option<&Value>, _: &Path) -> Result<project::Apply<C>, String> {
        let file = Mapper::read_file(value)?;
        let (mapper, revision, notify) = (self.mapper.clone(), self.revision.clone(), self.notify);
        Ok(Box::new(move |ctx| {
            mapper.lock().unwrap().load(file);
            revision.fetch_add(1, Ordering::Relaxed);
            notify(ctx);
            Ok(())
        }))
    }

    fn clear(&self, ctx: &C) -> Result<(), String> {
        self.mapper.lock().unwrap().clear();
        self.revision.fetch_add(1, Ordering::Relaxed);
        (self.notify)(ctx);
        Ok(())
    }

    fn revision(&self, _: &C) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }
}

fn notify_app(app: &AppHandle) {
    tell_mappings(app);
}

/// Watches for MIDI controllers, plays the pads from them, and maps their
/// knobs and faders to the controls they learned.
pub fn setup(app: &AppHandle) {
    let pads = app.state::<SamplerHandle>().inner().clone();
    let state = app.state::<Arc<MidiState>>().inner().clone();
    {
        let mut registry = state.targets.lock().unwrap();
        registry.register(Arc::new(SamplerTargets { focus: state.focus.clone() }));
        registry.register(Arc::new(MasterTarget));
        registry.register(Arc::new(TrackTargets));
    }
    project::register(app, MidiSection { mapper: state.mapper.clone(), revision: state.revision.clone(), notify: notify_app });
    spawn(app.clone(), pads, state.clone());
    spawn_mapper(app.clone(), state);
}

#[cfg(test)]
mod tests;
