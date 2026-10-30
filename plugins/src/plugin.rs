//! The interface between the engine and a hosted plugin, whatever its format.
//!
//! A loaded plugin comes in two halves, because that is how plugin formats are threaded (VST3
//! and CLAP both) and because the engine cannot lock on its audio thread:
//!
//! * [`PluginController`]: the main-thread half. Loads state, lists and sets parameters, opens
//!   the editor, and *activates* the plugin, which hands back
//! * [`Plugin`]: the audio-thread half. It has one job, [`Plugin::process`], which never
//!   allocates, locks or logs.
//!
//! The engine keeps the controller in its document model and moves the `Plugin` into the audio
//! graph. Both halves share the underlying plugin instance. Drop the `Plugin` first (and not on
//! the audio thread), then the controller; the controller's drop shuts the instance down.
//!
//! Format-specific code lives behind these two traits (`crate::vst3`, and CLAP later), so the
//! engine never sees a format.
//!
//! Parameter values are always *normalized* (0..=1) at this interface. Controllers convert to
//! the plugin's own units for display ([`PluginController::param_text`]).

use serde::{Deserialize, Serialize};
use std::ffi::c_void;
use std::path::PathBuf;

/// A plugin parameter's id. Stable across sessions for a given plugin, so it is what projects
/// and automation store.
pub type ParamId = u32;

/// The plugin formats tuneKore hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Vst3,
    Clap,
}

/// What a plugin is for, from its format's category strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Instrument,
    Effect,
    Other,
}

/// Which plugin this is: enough to find and load it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub format: Format,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub category: Category,
    /// The format's own category words (`Instrument|Synth` is `["Instrument", "Synth"]`).
    pub sub_categories: Vec<String>,
    /// VST3: the component class id as 32 upper-case hex digits. CLAP: the plugin id.
    pub class_id: String,
    /// The `.vst3` bundle (or `.clap` file).
    pub path: PathBuf,
}

/// Whether an audio bus is the plugin's main input or output, or an extra one (a sidechain, or
/// one of Kontakt's other outputs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BusKind {
    Main,
    Aux,
}

/// One audio bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusInfo {
    pub name: String,
    pub channels: u32,
    pub kind: BusKind,
    /// Whether the bus is on: by default in a [`BusLayout`] reported before activation, as
    /// activated in one reported after it. Inactive buses get no buffers.
    pub active: bool,
}

/// A plugin's buses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusLayout {
    pub audio_inputs: Vec<BusInfo>,
    pub audio_outputs: Vec<BusInfo>,
    /// Note and MIDI inputs (an instrument has one).
    pub event_inputs: u32,
    /// Note and MIDI outputs (a sequencer plugin has one).
    pub event_outputs: u32,
}

impl BusLayout {
    /// Channels across the active input buses: how many slices [`ProcessBlock::inputs`] holds.
    pub fn input_channels(&self) -> usize {
        active_channels(&self.audio_inputs)
    }

    /// Channels across the active output buses: how many slices [`ProcessBlock::outputs`] holds.
    pub fn output_channels(&self) -> usize {
        active_channels(&self.audio_outputs)
    }

    /// The channels of output bus `bus` inside [`ProcessBlock::outputs`] (empty if inactive).
    pub fn output_range(&self, bus: usize) -> std::ops::Range<usize> {
        channel_range(&self.audio_outputs, bus)
    }

    /// The channels of input bus `bus` inside [`ProcessBlock::inputs`] (empty if inactive).
    pub fn input_range(&self, bus: usize) -> std::ops::Range<usize> {
        channel_range(&self.audio_inputs, bus)
    }
}

fn active_channels(buses: &[BusInfo]) -> usize {
    buses.iter().filter(|b| b.active).map(|b| b.channels as usize).sum()
}

fn channel_range(buses: &[BusInfo], bus: usize) -> std::ops::Range<usize> {
    let start: usize = buses.iter().take(bus).filter(|b| b.active).map(|b| b.channels as usize).sum();
    match buses.get(bus) {
        Some(b) if b.active => start..start + b.channels as usize,
        _ => start..start,
    }
}

/// Which of a direction's buses to switch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BusSelection {
    /// The main bus only: what an ordinary instrument or effect needs.
    #[default]
    Main,
    /// Every bus, including sidechains and extra outputs.
    All,
}

/// How the plugin will be run, set at activation.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessSetup {
    pub sample_rate: f64,
    /// The most frames in any block passed to [`Plugin::process`].
    pub max_block_size: u32,
    /// False for an offline render (a bounce or an export), which lets a plugin use its better,
    /// slower algorithms.
    pub realtime: bool,
    pub inputs: BusSelection,
    pub outputs: BusSelection,
    /// Channels to ask for on the main buses (2 is stereo). A plugin that refuses keeps its own.
    pub main_channels: u32,
}

impl Default for ProcessSetup {
    fn default() -> Self {
        ProcessSetup {
            sample_rate: 48_000.0,
            max_block_size: 512,
            realtime: true,
            inputs: BusSelection::Main,
            outputs: BusSelection::Main,
            main_channels: 2,
        }
    }
}

/// The host's transport at the start of a block, for tempo-synced plugins. Sent to the plugin
/// every block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transport {
    pub playing: bool,
    pub recording: bool,
    pub looping: bool,
    /// Quarter notes per minute.
    pub tempo: f64,
    pub time_sig_numerator: u32,
    pub time_sig_denominator: u32,
    /// Timeline position in samples. Jumps when the transport loops or is moved.
    pub project_samples: i64,
    /// Samples since the engine started; never jumps and keeps counting while stopped.
    pub continuous_samples: i64,
    /// Timeline position in quarter notes.
    pub project_beats: f64,
    /// Start of the current bar, in quarter notes.
    pub bar_start_beats: f64,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
}

impl Default for Transport {
    fn default() -> Self {
        Transport {
            playing: false,
            recording: false,
            looping: false,
            tempo: 120.0,
            time_sig_numerator: 4,
            time_sig_denominator: 4,
            project_samples: 0,
            continuous_samples: 0,
            project_beats: 0.0,
            bar_start_beats: 0.0,
            loop_start_beats: 0.0,
            loop_end_beats: 0.0,
        }
    }
}

/// A note or MIDI message for the plugin, at a sample offset inside the block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Event {
    /// Frames from the start of the block. Events must be sorted by offset.
    pub offset: u32,
    pub kind: EventKind,
}

/// What an [`Event`] says. Channels are 0 to 15, velocities and pressures 0.0 to 1.0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EventKind {
    /// `note_id` ties a later note-off or pressure to this note; use -1 if not tracking.
    NoteOn { channel: u8, key: u8, velocity: f32, note_id: i32 },
    NoteOff { channel: u8, key: u8, velocity: f32, note_id: i32 },
    PolyPressure { channel: u8, key: u8, pressure: f32, note_id: i32 },
    /// Channel aftertouch.
    ChannelPressure { channel: u8, pressure: f32 },
    /// A MIDI control change; `value` is 0.0 to 1.0 (a 7-bit value over 127).
    ControlChange { channel: u8, controller: u8, value: f32 },
    /// Pitch wheel, -1.0 to 1.0 with 0.0 at the centre.
    PitchBend { channel: u8, value: f32 },
}

/// A parameter value at a sample offset, normalized. Several for one parameter in a block make a
/// ramp; the plugin interpolates (or steps) between them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamChange {
    pub id: ParamId,
    /// Frames from the start of the block. Changes must be sorted by offset.
    pub offset: u32,
    pub value: f64,
}

/// A fixed-capacity list the plugin's outputs go into, so that `process` never allocates.
/// Created once, `clear`ed before each block, read after it. Items that do not fit are dropped
/// and counted.
#[derive(Debug, Clone)]
pub struct OutBuf<T> {
    items: Vec<T>,
    cap: usize,
    dropped: u32,
}

impl<T: Copy> OutBuf<T> {
    pub fn with_capacity(cap: usize) -> Self {
        OutBuf { items: Vec::with_capacity(cap), cap, dropped: 0 }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.dropped = 0;
    }

    /// Adds an item, or counts it as dropped when full. Never allocates.
    pub fn push(&mut self, item: T) -> bool {
        if self.items.len() < self.cap {
            self.items.push(item);
            true
        } else {
            self.dropped = self.dropped.saturating_add(1);
            false
        }
    }

    pub fn as_slice(&self) -> &[T] {
        &self.items
    }

    /// How many items did not fit since the last `clear`.
    pub fn dropped(&self) -> u32 {
        self.dropped
    }
}

/// Everything one call to [`Plugin::process`] reads and writes.
pub struct ProcessBlock<'a, 'b> {
    /// Frames in this block: at most [`ProcessSetup::max_block_size`].
    pub frames: usize,
    /// One slice per channel of every *active* input bus, in bus order (see
    /// [`BusLayout::input_range`]). Each is at least `frames` long.
    pub inputs: &'b [&'a [f32]],
    /// The same for the outputs. Every channel is written (silence is written as zeros).
    pub outputs: &'b mut [&'a mut [f32]],
    /// Notes and MIDI for this block, sorted by offset.
    pub events: &'b [Event],
    /// Parameter changes for this block (automation, knob moves), sorted by offset. Changes the
    /// user makes in the plugin's own editor reach the plugin by themselves.
    pub param_changes: &'b [ParamChange],
    pub transport: &'b Transport,
    /// Notes the plugin sends out (a sequencer or arpeggiator plugin). Cleared by the caller.
    pub midi_out: &'b mut OutBuf<Event>,
    /// Parameter changes the plugin makes itself (its own LFOs or macros). Cleared by the caller.
    pub param_out: &'b mut OutBuf<ParamChange>,
}

/// Why [`Plugin::process`] did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProcessError {
    /// The controller activated the plugin again, or shut it down, since this half was made.
    #[error("this processor is no longer the plugin's current one")]
    Stale,
    /// The plugin threw an exception or crashed earlier; it is not called again. The message is
    /// from [`PluginController::take_error`].
    #[error("the plugin failed earlier and is switched off")]
    Failed,
    /// The block does not match the layout: too many frames, or the wrong number of channels.
    #[error("the block does not match the plugin's layout")]
    BadBlock,
    /// The plugin returned an error from `process`.
    #[error("the plugin returned error {0} from process")]
    Plugin(i32),
}

/// The audio-thread half of a loaded plugin. See the module notes.
pub trait Plugin: Send {
    fn info(&self) -> &PluginInfo;

    /// The buses in force since activation.
    fn layout(&self) -> &BusLayout;

    /// Runs the plugin over one block. On the audio thread: no allocation, locks or logging.
    /// On an error the outputs are left as zeros.
    fn process(&mut self, block: &mut ProcessBlock<'_, '_>) -> Result<(), ProcessError>;
}

/// A parameter, as the plugin lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamInfo {
    pub id: ParamId,
    pub name: String,
    pub short_name: String,
    pub units: String,
    /// Normalized.
    pub default: f64,
    /// 0 for a continuous parameter; otherwise the value moves in this many steps (1 is a
    /// switch).
    pub step_count: u32,
    pub automatable: bool,
    pub hidden: bool,
    pub read_only: bool,
    pub is_bypass: bool,
}

/// Something that happened on the plugin's side, reported by [`PluginController::poll`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ControllerEvent {
    /// The user started moving a control in the plugin's editor (for automation recording).
    EditBegin { id: ParamId },
    /// The user moved a control in the plugin's editor. The plugin's processor already has the
    /// value; this is for the UI and for recording automation.
    ParamEdit { id: ParamId, value: f64 },
    EditEnd { id: ParamId },
    /// The plugin's latency changed: ask [`PluginController::latency_samples`] and re-plan delay
    /// compensation.
    LatencyChanged,
    /// Parameter values or names changed on the plugin's side (a preset was loaded in its own
    /// browser): read them again.
    ParamsChanged,
    /// The plugin wants to be reloaded (its buses or parameter list changed). Deactivate and
    /// activate it again, and read its parameters again.
    ReloadRequested,
}

/// A native window the editor goes into: an `NSView*` on macOS, an `HWND` on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeParent(pub *mut c_void);

// SAFETY: the pointer is only handed to the plugin on the main thread; this type just carries it.
unsafe impl Send for NativeParent {}

/// The editor's size in pixels (points on macOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorSize {
    pub width: u32,
    pub height: u32,
}

/// Called on the main thread when the plugin's editor asks for a different size. The host
/// resizes its window to match.
pub type ResizeCallback = Box<dyn FnMut(EditorSize) + Send>;

/// Why a plugin operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    #[error("cannot load {path}: {reason}")]
    Load { path: String, reason: String },
    #[error("{path} has no plugin with class id {class_id}")]
    NotFound { path: String, class_id: String },
    /// The plugin threw a C++ exception (caught by the shim) while we called `during`.
    #[error("the plugin threw an exception in {during}: {message}")]
    Exception { during: &'static str, message: String },
    /// The plugin refused or failed a call.
    #[error("{0}")]
    Failed(String),
    #[error("not supported: {0}")]
    Unsupported(String),
    /// The plugin failed earlier and is switched off.
    #[error("the plugin failed earlier: {0}")]
    Poisoned(String),
}

/// The main-thread half of a loaded plugin. See the module notes. Every method is for the main
/// (UI) thread, except where it says otherwise.
pub trait PluginController: Send {
    fn info(&self) -> &PluginInfo;

    /// The buses the plugin starts with, before activation chooses.
    fn default_layout(&self) -> &BusLayout;

    /// Sets the plugin up for processing and returns the audio-thread half. Call again to change
    /// the sample rate, block size or buses: the previous `Plugin` then returns
    /// [`ProcessError::Stale`]. The plugin is left running, so any sound it makes starts now.
    fn activate(&mut self, setup: &ProcessSetup) -> Result<Box<dyn Plugin>, PluginError>;

    /// Stops the plugin. The current `Plugin` returns [`ProcessError::Stale`] afterwards.
    fn deactivate(&mut self);

    /// Processing latency in samples, for delay compensation. Valid after `activate`.
    fn latency_samples(&mut self) -> u32;

    /// How long the plugin keeps sounding after its input stops, in samples (0 if none, and
    /// `u32::MAX` if it never stops).
    fn tail_samples(&mut self) -> u32;

    /// The plugin's parameters, hidden ones included.
    fn params(&self) -> &[ParamInfo];

    /// Reads the parameter list again (after [`ControllerEvent::ParamsChanged`] or a reload).
    fn refresh_params(&mut self);

    /// A parameter's current value, normalized.
    fn param_value(&mut self, id: ParamId) -> Option<f64>;

    /// Sets a parameter from the UI: the plugin's editor shows it and its processor gets it in
    /// the next block. Automation does not use this; it goes in [`ProcessBlock::param_changes`].
    fn set_param(&mut self, id: ParamId, normalized: f64);

    /// The plugin's own text for a value ("-6.0 dB", "Saw").
    fn param_text(&mut self, id: ParamId, normalized: f64) -> Option<String>;

    /// A normalized value in the parameter's own units (Hz, dB, ...), when the plugin has a
    /// mapping.
    fn param_plain(&mut self, id: ParamId, normalized: f64) -> Option<f64>;

    /// The plugin's whole state (sound, settings) as one opaque blob for the project file.
    fn save_state(&mut self) -> Result<Vec<u8>, PluginError>;

    /// Restores a blob from [`save_state`](Self::save_state). The data is untrusted. Call it
    /// while the processor is idle where you can.
    fn load_state(&mut self, data: &[u8]) -> Result<(), PluginError>;

    /// Things the plugin did since the last call: edits in its own editor, latency changes.
    fn poll(&mut self, out: &mut Vec<ControllerEvent>);

    /// The last exception or failure that switched the plugin off, once.
    fn take_error(&mut self) -> Option<String>;

    /// Whether the plugin has an editor of its own.
    fn has_editor(&mut self) -> bool;

    /// Opens the plugin's editor inside `parent` (an `NSView*`, in a window the host owns) and
    /// returns its size. `on_resize` is called when the editor wants a different size.
    fn open_editor(&mut self, parent: NativeParent, on_resize: ResizeCallback) -> Result<EditorSize, PluginError>;

    /// Closes the editor. The host window can go once this returns.
    fn close_editor(&mut self);

    /// Tells the editor its window was resized by the user. Returns the size the editor settled
    /// on (it may refuse or snap). `None` if no editor is open.
    fn resize_editor(&mut self, size: EditorSize) -> Option<EditorSize>;

    /// Whether the user may resize the editor window.
    fn editor_resizable(&mut self) -> bool;
}
