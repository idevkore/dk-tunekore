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
use map::{Cc, Looked, Mapper, Mapping, Move, Source, TargetInfo, Takeover, GESTURE_GAP};
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
/// changes on. While a track's instrument has the keys (`plugins_focus`),
/// notes, pitch bend and pressure go to it instead, and so do control
/// changes MIDI learn neither maps nor is learning.
fn handle(pads: &SamplerHandle, state: &MidiState, changes: &Sender<Arrived>, msg: &[u8]) {
    if crate::plugins::play_live(msg) {
        return;
    }
    match parse(msg) {
        Some(PadEvent::On { pad, velocity }) => {
            pads.trigger_velocity(pad, velocity as f32 / 127.0);
            state.activity.store(pad as i32, Ordering::Relaxed);
        }
        Some(PadEvent::Off { pad }) => pads.release(pad),
        None => {
            if let Some(cc) = map::parse_cc(msg) {
                if crate::plugins::live_has_focus() {
                    let free = {
                        let m = state.mapper.lock().unwrap_or_else(|e| e.into_inner());
                        m.learning().is_none() && !m.mappings().iter().any(|x| x.source.listens(cc.channel, cc.cc))
                    };
                    if free {
                        crate::plugins::control_live(msg);
                    }
                }
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

/// Waits `d` between scans. On macOS it runs this thread's run loop
/// meanwhile: CoreMIDI updates a process's list of devices only through the
/// run loop of the thread that made the process's first client (this one,
/// the scan's), so with a plain sleep a controller plugged in after launch
/// never shows up, however many new clients look.
#[cfg(target_os = "macos")]
fn wait_for_devices(d: Duration) {
    use std::ffi::c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source_handled: u8) -> i32;
    }
    /// What `CFRunLoopRunInMode` returns when the run loop has nothing to run.
    const FINISHED: i32 = 1;
    let end = Instant::now() + d;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        // SAFETY: runs this thread's own run loop in the default mode, a
        // constant CoreFoundation exports; nothing is passed in or kept.
        if unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, left.as_secs_f64(), 0) } == FINISHED {
            std::thread::sleep(left);
            break;
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn wait_for_devices(d: Duration) {
    std::thread::sleep(d);
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
            wait_for_devices(SCAN_EVERY);
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
/// learn that waited for a 14-bit partner. The mapper's lock is held only
/// for its own quick steps, never while a control is looked up or set: a
/// busy Sampler or engine (a project opening, the output moving) then holds
/// up the knobs, not the MIDI commands. Generic so tests can drive it with
/// targets of their own.
fn run<C: 'static>(mapper: &Mutex<Mapper>, registry: &Registry<C>, ctx: &C, batch: &[Arrived], now: Instant) -> Outcome {
    let mut out = Outcome::default();
    let (pending, key) = {
        let mut m = mapper.lock().unwrap();
        for &(at, cc) in batch {
            out.learned.extend(m.feed(at, cc));
        }
        out.learned.extend(m.tick(now));
        (m.pending(), m.gesture_key())
    };
    if pending.is_empty() {
        return out;
    }
    let looked: Vec<Looked> = pending
        .into_iter()
        .map(|p| {
            let (info, value) = (registry.info(ctx, &p.target), registry.get(ctx, &p.target));
            (p, info, value)
        })
        .collect();
    let moves = mapper.lock().unwrap().decide(looked);
    if moves.is_empty() {
        return out;
    }
    let (touched, errors) = registry.apply(ctx, &moves, &key);
    let read: Vec<(String, Option<f64>)> = moves.iter().map(|m| (m.target.clone(), registry.get(ctx, &m.target))).collect();
    mapper.lock().unwrap().settle_values(&read);
    out.touched = touched;
    out.errors = errors;
    out.moves = moves;
    out
}

/// A control moved by a knob, for the windows (`midi:moved`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Moved {
    target: String,
    value: f64,
}

/// When the windows hear about knob moves: each provider's small update
/// (`changed`) at most every [`TELL_EVERY`], with `midi:moved`, and its
/// full state (`settled`) once a gesture has had no move for [`GESTURE_GAP`].
#[derive(Debug, Default)]
struct Teller {
    /// Providers that moved something since the windows last heard, and the moves.
    to_tell: Vec<usize>,
    moved: Vec<Moved>,
    told: Option<Instant>,
    /// Providers that moved something in the gesture now going on.
    in_gesture: Vec<usize>,
    last_move: Option<Instant>,
}

/// What a [`Teller`] says is due now.
#[derive(Debug, Default, PartialEq)]
struct Due {
    changed: Vec<usize>,
    moved: Vec<Moved>,
    settled: Vec<usize>,
}

impl Teller {
    /// Notes what a batch moved, at `now`.
    fn note(&mut self, out: &Outcome, now: Instant) {
        for &t in &out.touched {
            for list in [&mut self.to_tell, &mut self.in_gesture] {
                if !list.contains(&t) {
                    list.push(t);
                }
            }
        }
        for m in &out.moves {
            self.moved.retain(|x| x.target != m.target);
            self.moved.push(Moved { target: m.target.clone(), value: m.value });
        }
        if !out.moves.is_empty() {
            self.last_move = Some(now);
        }
    }

    /// When the thread should wake for the next of these, if anything waits.
    fn wake_at(&self) -> Option<Instant> {
        let tell = (!self.to_tell.is_empty() || !self.moved.is_empty()).then(|| self.told.map_or(self.last_move, |t| Some(t + TELL_EVERY))).flatten();
        let settle = (!self.in_gesture.is_empty()).then(|| self.last_move.map(|t| t + GESTURE_GAP)).flatten();
        [tell, settle].into_iter().flatten().min()
    }

    /// Takes what is due at `now`.
    fn take(&mut self, now: Instant) -> Due {
        let mut due = Due::default();
        let ended = !self.in_gesture.is_empty() && self.last_move.is_some_and(|t| now >= t + GESTURE_GAP);
        let waiting = !self.to_tell.is_empty() || !self.moved.is_empty();
        if waiting && (ended || self.told.is_none_or(|t| now >= t + TELL_EVERY)) {
            due.changed = std::mem::take(&mut self.to_tell);
            due.moved = std::mem::take(&mut self.moved);
            self.told = Some(now);
        }
        if ended {
            due.settled = std::mem::take(&mut self.in_gesture);
        }
        due
    }
}

/// Learns and applies control changes as they arrive; see the module docs.
fn spawn_mapper(app: AppHandle, state: Arc<MidiState>) {
    let Some(rx) = state.receiver.lock().unwrap().take() else { return };
    std::thread::spawn(move || {
        let mut teller = Teller::default();
        loop {
            // Wake for a control change, a learn's 14-bit wait, the next time to
            // tell the windows, or the end of a gesture.
            let wake = [state.mapper.lock().unwrap().deadline(), teller.wake_at()].into_iter().flatten().min();
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
            let out = run(&state.mapper, &registry, &app, &batch, Instant::now());
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
            teller.note(&out, Instant::now());
            let due = teller.take(Instant::now());
            if !due.changed.is_empty() || !due.moved.is_empty() {
                registry.changed(&app, &due.changed);
                let _ = app.emit("midi:moved", &due.moved);
            }
            registry.settled(&app, &due.settled);
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
    // Copied out first: naming the controls may wait on the Sampler or the engine.
    let (mappings, learning, takeover) = {
        let mapper = state.mapper.lock().unwrap();
        (mapper.mappings().to_vec(), mapper.learning().map(String::from), mapper.takeover)
    };
    MidiView { mappings: mappings.iter().map(|m| mapping_view(&registry, app, m)).collect(), learning, takeover }
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
