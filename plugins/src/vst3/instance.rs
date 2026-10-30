//! A live VST3 plugin: the shared instance, and its two halves.
//!
//! Derived from soundCraft's `vst3-host` (`ffi.rs`), MIT OR Apache-2.0; see
//! `third_party/soundcraft/`. What changed: the instance is split into [`Vst3Controller`]
//! (main thread) and [`Vst3Processor`] (audio thread) behind the `Plugin` traits; `process` takes
//! multiple buses, many events and automation points, and a full transport, and allocates and
//! locks nothing; every call goes through the exception-catching shim, and a plugin that throws
//! is switched off instead of being called again.

use super::editor::Editor;
use super::events::{self, EventList, ParamChanges, AFTERTOUCH, PITCH_BEND};
use super::host::{HostContext, MemoryStream, MAX_STATE_BYTES};
use super::module::Bundle;
use super::ptr::PlugPtr;
use super::shim::Exception;
use super::util::{parse_hex, read_c16};
use crate::plugin::*;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use vst3::Steinberg::{self as sb, Vst as sv};
use vst3::ComWrapper;

/// Most audio buses per direction we provide buffers for.
const MAX_BUSES: i32 = 16;
/// Most channels per audio bus.
const MAX_BUS_CHANNELS: u32 = 64;
/// Most parameters read from one controller.
const MAX_PARAMS: i32 = 4096;
/// Capacity of the per-block event lists (preallocated: `process` never allocates).
const EVENT_CAP: usize = 1024;
/// Parameter queues per block going in (one per parameter touched).
const IN_PARAM_QUEUES: usize = 256;
/// Parameter queues a plugin may write per block.
const OUT_PARAM_QUEUES: usize = 64;
/// Edits from a plugin's editor buffered for the processor between two blocks.
const EDIT_QUEUE: usize = 1024;
/// Magic of our state container: `TKV3` + u32 LE component length + component state + u32 LE
/// controller length + controller state.
const STATE_MAGIC: &[u8; 4] = b"TKV3";
/// How long shutting down waits for a `process` call that is still running.
const SHUTDOWN_WAIT: Duration = Duration::from_millis(500);

fn ok(r: i32) -> bool {
    r == sb::kResultOk
}

/// What a guarded call that threw becomes.
fn thrown(during: &'static str, e: Exception) -> PluginError {
    PluginError::Exception { during, message: e.0 }
}

/// The component's and the controller's connection points, linked to each other.
type ConnectionPair = (ManuallyDrop<PlugPtr<sv::IConnectionPoint>>, ManuallyDrop<PlugPtr<sv::IConnectionPoint>>);

/// The state both halves share.
pub(crate) struct Instance {
    pub info: PluginInfo,
    // The plugin's objects are released in `Drop` (or leaked if the plugin failed); they are
    // `ManuallyDrop` for that.
    component: ManuallyDrop<PlugPtr<sv::IComponent>>,
    processor: ManuallyDrop<PlugPtr<sv::IAudioProcessor>>,
    controller: Option<ManuallyDrop<PlugPtr<sv::IEditController>>>,
    /// The controller is the component itself (single-component plugin).
    single: bool,
    controller_initialized: bool,
    connections: Option<ConnectionPair>,
    pub host: ComWrapper<HostContext>,
    /// The plugin threw or returned a fatal error; it is never called again from `process`, and
    /// at drop its objects are leaked rather than touched.
    failed: AtomicBool,
    error: Mutex<Option<String>>,
    /// Shut down: `process` refuses to run.
    dead: AtomicBool,
    in_process: AtomicBool,
    /// Bumped at every activation; a processor from an earlier one is stale.
    generation: AtomicU64,
    active: AtomicBool,
    processing: AtomicBool,
    // Last: the bundle keeps the plugin's code mapped until everything above is gone.
    bundle: Arc<Bundle>,
}

// SAFETY: the COM pointers are called under the VST3 threading model, decided by the two halves:
// the controller (one thread at a time, the main thread by contract) and the processor (the audio
// thread). The host context holds only atomics and mutexes, and COM reference counts are atomic.
unsafe impl Send for Instance {}
// SAFETY: as above; all shared mutation goes through atomics and mutexes.
unsafe impl Sync for Instance {}

impl Instance {
    /// Creates, initialises and wires up one plugin instance.
    pub fn create(bundle: &Arc<Bundle>, info: &PluginInfo) -> Result<Arc<Instance>, PluginError> {
        let cid = parse_hex(&info.class_id).ok_or_else(|| PluginError::NotFound { path: info.path.display().to_string(), class_id: info.class_id.clone() })?;
        let host = ComWrapper::new(HostContext::new());
        let host_unknown = host
            .as_com_ref::<sv::IHostApplication>()
            .ok_or_else(|| PluginError::Failed("no host context".into()))?
            .as_ptr()
            .cast::<sb::FUnknown>();
        let component = bundle.create::<sv::IComponent>(&cid)?;
        // SAFETY: fresh component; the host context outlives it (owned by the instance).
        match unsafe { component.initialize(host_unknown) } {
            Ok(r) if ok(r) => {}
            Ok(r) => return Err(PluginError::Failed(format!("IComponent::initialize returned {r}"))),
            Err(e) => return Err(thrown("IComponent::initialize", e)),
        }
        let Some(processor) = component.query::<sv::IAudioProcessor>() else {
            // SAFETY: initialised above, terminated once.
            let _ = unsafe { component.terminate() };
            return Err(PluginError::Failed("the component has no IAudioProcessor".into()));
        };
        let mut inst = Instance {
            info: info.clone(),
            component: ManuallyDrop::new(component),
            processor: ManuallyDrop::new(processor),
            controller: None,
            single: false,
            controller_initialized: false,
            connections: None,
            host,
            failed: AtomicBool::new(false),
            error: Mutex::new(None),
            dead: AtomicBool::new(false),
            in_process: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            active: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            bundle: bundle.clone(),
        };
        // From here on a failure just returns: dropping `inst` terminates what was initialised.
        inst.find_controller(bundle, &cid, host_unknown);
        if let Some(c) = &inst.controller {
            if !inst.single
                && let (Some(a), Some(b)) = (inst.component.query::<sv::IConnectionPoint>(), c.query::<sv::IConnectionPoint>())
            {
                // SAFETY: both connection points are valid for as long as the instance lives; they
                // are disconnected in `shutdown` before either side terminates.
                unsafe {
                    let _ = a.connect(b.as_ptr());
                    let _ = b.connect(a.as_ptr());
                }
                inst.connections = Some((ManuallyDrop::new(a), ManuallyDrop::new(b)));
            }
            inst.sync_controller_state();
            if let Some(h) = inst.host.as_com_ref::<sv::IComponentHandler>() {
                // SAFETY: valid controller; the handler (host context) outlives it.
                let _ = unsafe { c.set_component_handler(h.as_ptr()) };
            }
        }
        Ok(Arc::new(inst))
    }

    /// The edit controller: the component itself, or a separate class it names.
    fn find_controller(&mut self, bundle: &Arc<Bundle>, _cid: &[u8; 16], host_unknown: *mut sb::FUnknown) {
        if let Some(c) = self.component.query::<sv::IEditController>() {
            self.controller = Some(ManuallyDrop::new(c));
            self.single = true;
            return;
        }
        let mut ccid: sb::TUID = [0; 16];
        // SAFETY: valid component; `ccid` is a writable 16-byte TUID.
        if !matches!(unsafe { self.component.get_controller_class_id(&mut ccid) }, Ok(sb::kResultOk)) {
            return;
        }
        let Ok(c) = bundle.create::<sv::IEditController>(&super::util::tuid_bytes(&ccid)) else {
            log::warn!("vst3 {}: cannot create the edit controller; parameters unavailable", self.info.name);
            return;
        };
        // SAFETY: fresh controller; the host context outlives it.
        match unsafe { c.initialize(host_unknown) } {
            Ok(r) if ok(r) => {
                self.controller = Some(ManuallyDrop::new(c));
                self.controller_initialized = true;
            }
            _ => log::warn!("vst3 {}: controller initialize failed; parameters unavailable", self.info.name),
        }
    }

    fn controller(&self) -> Option<&PlugPtr<sv::IEditController>> {
        self.controller.as_deref()
    }

    /// Switches the plugin off after it threw: records why, and `process` stops calling it.
    pub fn fail(&self, during: &'static str, message: &str) {
        log::error!("vst3 {}: {during}: {message}", self.info.name);
        self.error.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(|| format!("{during}: {message}"));
        self.failed.store(true, Ordering::Release);
    }

    /// Plugin objects were leaked on purpose: its module must never be exited.
    pub fn mark_leaked(&self) {
        self.bundle.mark_leaked();
    }

    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Runs a guarded call that sets the plugin up or runs it; an exception becomes an error and
    /// switches the plugin off, because its state is no longer known.
    fn guard<T>(&self, during: &'static str, r: Result<T, Exception>) -> Result<T, PluginError> {
        r.map_err(|e| {
            self.fail(during, &e.0);
            thrown(during, e)
        })
    }

    /// Runs a guarded call that only asks something (a parameter's text, its state): an
    /// exception becomes an error and is remembered for `take_error`, but the plugin keeps
    /// running.
    pub fn soft<T>(&self, during: &'static str, r: Result<T, Exception>) -> Result<T, PluginError> {
        r.map_err(|e| {
            self.note_exception(during, &e.0);
            thrown(during, e)
        })
    }

    /// Records that a call threw, without switching the plugin off.
    pub fn note_exception(&self, during: &'static str, message: &str) {
        log::warn!("vst3 {}: {during}: {message}", self.info.name);
        self.error.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(|| format!("{during}: {message}"));
    }

    /// Copies the component's state into the controller (`getState` → `setComponentState`).
    fn sync_controller_state(&self) {
        let Some(c) = self.controller() else { return };
        let stream = MemoryStream::new(Vec::new());
        let Some(s) = stream.as_com_ref::<sb::IBStream>() else { return };
        // SAFETY: valid component; the stream lives on this frame for the whole call.
        if matches!(self.soft("IComponent::getState", unsafe { self.component.get_state(s.as_ptr()) }), Ok(sb::kResultOk)) {
            stream.rewind();
            // SAFETY: valid controller; same stream, rewound.
            let _ = self.soft("IEditController::setComponentState", unsafe { c.set_component_state(s.as_ptr()) });
        }
    }

    // ---- buses ----------------------------------------------------------------------------

    fn bus_count(&self, media: sv::MediaTypes, dir: sv::BusDirections) -> i32 {
        // SAFETY: valid component.
        unsafe { self.component.get_bus_count(media as i32, dir as i32) }.unwrap_or(0).clamp(0, MAX_BUSES)
    }

    fn arrangement(&self, dir: sv::BusDirections, index: i32) -> Option<sv::SpeakerArrangement> {
        let mut arr: sv::SpeakerArrangement = 0;
        // SAFETY: valid processor; `arr` is writable.
        matches!(unsafe { self.processor.get_bus_arrangement(dir as i32, index, &mut arr) }, Ok(sb::kResultOk)).then_some(arr)
    }

    fn audio_buses(&self, dir: sv::BusDirections) -> Vec<BusInfo> {
        (0..self.bus_count(sv::MediaTypes_::kAudio, dir))
            .filter_map(|i| {
                // SAFETY: plain data; all-zero is valid.
                let mut b: sv::BusInfo = unsafe { std::mem::zeroed() };
                // SAFETY: valid component; index below the bus count; `b` is writable.
                if !matches!(unsafe { self.component.get_bus_info(sv::MediaTypes_::kAudio as i32, dir as i32, i, &mut b) }, Ok(sb::kResultOk)) {
                    return None;
                }
                // The arrangement, when the processor reports one, is the truer channel count.
                let channels = self.arrangement(dir, i).map_or(b.channelCount.max(0) as u32, |a| a.count_ones()).min(MAX_BUS_CHANNELS);
                Some(BusInfo {
                    name: read_c16(&b.name),
                    channels,
                    kind: if b.busType == sv::BusTypes_::kMain as i32 { BusKind::Main } else { BusKind::Aux },
                    active: b.flags & sv::BusInfo_::BusFlags_::kDefaultActive as u32 != 0,
                })
            })
            .collect()
    }

    /// The buses as the plugin reports them now.
    pub fn read_layout(&self) -> BusLayout {
        BusLayout {
            audio_inputs: self.audio_buses(sv::BusDirections_::kInput),
            audio_outputs: self.audio_buses(sv::BusDirections_::kOutput),
            event_inputs: self.bus_count(sv::MediaTypes_::kEvent, sv::BusDirections_::kInput) as u32,
            event_outputs: self.bus_count(sv::MediaTypes_::kEvent, sv::BusDirections_::kOutput) as u32,
        }
    }

    // ---- activation -----------------------------------------------------------------------

    /// Makes the current processor stale and waits (briefly) for a `process` call it may be in
    /// the middle of, so the plugin is not stopped or reconfigured under it.
    fn retire_processor(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.wait_for_process();
    }

    fn wait_for_process(&self) {
        let t = Instant::now();
        while self.in_process.load(Ordering::SeqCst) && t.elapsed() < SHUTDOWN_WAIT {
            std::thread::yield_now();
        }
    }

    /// Retires the current processor, then stops processing and deactivates (when needed).
    pub fn stop(&self) {
        self.retire_processor();
        if self.processing.swap(false, Ordering::AcqRel) {
            // SAFETY: valid processor in the processing state.
            let _ = unsafe { self.processor.set_processing(0) };
        }
        if self.active.swap(false, Ordering::AcqRel) {
            // SAFETY: valid, active component.
            let _ = unsafe { self.component.set_active(0) };
        }
    }

    /// Negotiates the bus layout (main buses with `setup.main_channels` channels when the plugin
    /// agrees), switches the chosen buses on, sets up 32-bit processing, activates the component
    /// and starts processing. Returns the layout in force and the new generation number.
    pub fn start(&self, setup: &ProcessSetup) -> Result<(BusLayout, u64), PluginError> {
        if self.is_failed() {
            return Err(PluginError::Poisoned(self.take_error().unwrap_or_default()));
        }
        self.stop();
        let audio = sv::MediaTypes_::kAudio as i32;
        let event = sv::MediaTypes_::kEvent as i32;
        let (din, dout) = (sv::BusDirections_::kInput, sv::BusDirections_::kOutput);
        let (nin, nout) = (self.bus_count(sv::MediaTypes_::kAudio, din), self.bus_count(sv::MediaTypes_::kAudio, dout));
        let before = self.read_layout();
        let main_index = |buses: &[BusInfo]| buses.iter().position(|b| b.kind == BusKind::Main).unwrap_or(0);
        let want = match setup.main_channels {
            1 => sv::SpeakerArr::kMono,
            _ => sv::SpeakerArr::kStereo,
        };
        // First try the main buses at the wanted width; if the plugin says no, leave its own.
        let current = |dir: sv::BusDirections, n: i32| -> Vec<sv::SpeakerArrangement> {
            (0..n).map(|i| self.arrangement(dir, i).unwrap_or(sv::SpeakerArr::kStereo)).collect()
        };
        let (mut ins, mut outs) = (current(din, nin), current(dout, nout));
        if let Some(a) = ins.get_mut(main_index(&before.audio_inputs)) {
            *a = want;
        }
        if let Some(a) = outs.get_mut(main_index(&before.audio_outputs)) {
            *a = want;
        }
        // SAFETY: valid processor; the arrays hold exactly `nin`/`nout` arrangements and outlive the call.
        let r = unsafe { self.processor.set_bus_arrangements(ins.as_mut_ptr(), nin, outs.as_mut_ptr(), nout) };
        if !self.guard("IAudioProcessor::setBusArrangements", r)?.eq(&sb::kResultOk) {
            log::debug!("vst3 {}: kept its own bus arrangements", self.info.name);
        }
        let mut layout = self.read_layout();
        for (dir, buses, sel) in [(din, &mut layout.audio_inputs, setup.inputs), (dout, &mut layout.audio_outputs, setup.outputs)] {
            let main = main_index(buses);
            for (i, b) in buses.iter_mut().enumerate() {
                b.active = match sel {
                    BusSelection::All => true,
                    BusSelection::Main => i == main,
                };
                // SAFETY: valid component; index below the bus count.
                let r = unsafe { self.component.activate_bus(audio, dir as i32, i as i32, u8::from(b.active)) };
                self.guard("IComponent::activateBus", r)?;
            }
        }
        for (dir, n) in [(din, layout.event_inputs), (dout, layout.event_outputs)] {
            for i in 0..n as i32 {
                // SAFETY: valid component; event bus index below the count.
                let r = unsafe { self.component.activate_bus(event, dir as i32, i, 1) };
                self.guard("IComponent::activateBus", r)?;
            }
        }
        let sample32 = sv::SymbolicSampleSizes_::kSample32 as i32;
        // SAFETY: valid processor.
        if !ok(self.guard("IAudioProcessor::canProcessSampleSize", unsafe { self.processor.can_process_sample_size(sample32) })?) {
            return Err(PluginError::Failed("the plugin cannot process 32-bit float audio".into()));
        }
        let mut s = sv::ProcessSetup {
            processMode: if setup.realtime { sv::ProcessModes_::kRealtime } else { sv::ProcessModes_::kOffline } as i32,
            symbolicSampleSize: sample32,
            maxSamplesPerBlock: i32::try_from(setup.max_block_size).unwrap_or(i32::MAX),
            sampleRate: setup.sample_rate,
        };
        // SAFETY: valid processor, inactive; `s` is a valid struct for the call.
        if !ok(self.guard("IAudioProcessor::setupProcessing", unsafe { self.processor.setup_processing(&mut s) })?) {
            return Err(PluginError::Failed("setupProcessing failed".into()));
        }
        // SAFETY: valid component, configured above.
        if !ok(self.guard("IComponent::setActive", unsafe { self.component.set_active(1) })?) {
            return Err(PluginError::Failed("setActive failed".into()));
        }
        self.active.store(true, Ordering::Release);
        // Many plugins return kNotImplemented here; that is not an error.
        // SAFETY: valid, active processor.
        self.guard("IAudioProcessor::setProcessing", unsafe { self.processor.set_processing(1) })?;
        self.processing.store(true, Ordering::Release);
        // `stop` above retired the previous processor; the new one belongs to this generation.
        Ok((layout, self.generation.load(Ordering::SeqCst)))
    }

    /// Which parameter each MIDI controller drives (`IMidiMapping`): `[channel][controller]`,
    /// controllers 0 to 127 are CCs, then aftertouch and pitch bend. `u32::MAX` for none.
    fn read_midi_mapping(&self) -> Box<[u32]> {
        let mut map = vec![u32::MAX; 16 * MIDI_CONTROLLERS].into_boxed_slice();
        let Some(m) = self.controller().and_then(|c| c.query::<sv::IMidiMapping>()) else { return map };
        for ch in 0..16usize {
            for n in 0..MIDI_CONTROLLERS {
                let mut id: sv::ParamID = 0;
                // SAFETY: valid mapping interface; `id` is a local.
                if let Ok(sb::kResultOk) = unsafe { m.get_midi_controller_assignment(0, ch as i16, n as i16, &mut id) } {
                    map[ch * MIDI_CONTROLLERS + n] = id;
                }
            }
        }
        map
    }

    // ---- shutdown -------------------------------------------------------------------------

    /// Stops the plugin for good: waits (briefly) for a running `process`, then deactivates,
    /// disconnects and terminates. Safe to call twice.
    pub fn shutdown(&self) {
        if self.dead.swap(true, Ordering::SeqCst) {
            return;
        }
        self.wait_for_process();
        if self.is_failed() {
            return; // Never call a plugin that threw again; `Drop` leaks its objects.
        }
        self.stop();
        if let Some((a, b)) = &self.connections {
            // SAFETY: the pair was connected in `create`; disconnect before terminating.
            unsafe {
                let _ = a.disconnect(b.as_ptr());
                let _ = b.disconnect(a.as_ptr());
            }
        }
        if let Some(c) = self.controller() {
            // SAFETY: valid controller; detach our handler before it goes away.
            let _ = unsafe { c.set_component_handler(std::ptr::null_mut()) };
            if self.controller_initialized {
                // SAFETY: a separately created controller, initialised once; terminated once.
                let _ = unsafe { c.terminate() };
            }
        }
        // SAFETY: valid component, inactive; terminated exactly once (`dead` guards this).
        let _ = unsafe { self.component.terminate() };
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.shutdown();
        if self.is_failed() {
            // The plugin threw or crashed: touching it again (even a release) may do it again.
            // Leak its objects, and keep its module loaded for good.
            self.bundle.mark_leaked();
            return;
        }
        // SAFETY: each is dropped once, here, in the order that releases the connection points and
        // controller before the component; none is used afterwards.
        unsafe {
            if let Some((a, b)) = self.connections.as_mut() {
                ManuallyDrop::drop(a);
                ManuallyDrop::drop(b);
            }
            if let Some(c) = self.controller.as_mut() {
                ManuallyDrop::drop(c);
            }
            ManuallyDrop::drop(&mut self.processor);
            ManuallyDrop::drop(&mut self.component);
        }
    }
}

/// The MIDI controllers `IMidiMapping` is asked about: CCs 0-127, aftertouch, pitch bend.
const MIDI_CONTROLLERS: usize = PITCH_BEND + 1;

// ---- the controller (main thread) ----------------------------------------------------------

/// The main-thread half of a hosted VST3 plugin.
pub struct Vst3Controller {
    inst: Arc<Instance>,
    info: PluginInfo,
    default_layout: BusLayout,
    params: Vec<ParamInfo>,
    editor: Option<Editor>,
}

impl Vst3Controller {
    pub(crate) fn new(inst: Arc<Instance>) -> Vst3Controller {
        let mut c = Vst3Controller { info: inst.info.clone(), default_layout: inst.read_layout(), params: Vec::new(), editor: None, inst };
        c.refresh_params();
        c
    }

    fn check_alive(&self) -> Result<(), PluginError> {
        if self.inst.is_failed() {
            Err(PluginError::Poisoned(self.inst.error.lock().unwrap_or_else(PoisonError::into_inner).clone().unwrap_or_default()))
        } else {
            Ok(())
        }
    }

    /// Writes one state into a fresh memory stream.
    fn read_state(&self, during: &'static str, f: impl FnOnce(*mut sb::IBStream) -> Result<i32, Exception>) -> Result<Vec<u8>, PluginError> {
        let stream = MemoryStream::new(Vec::new());
        let s = stream.as_com_ref::<sb::IBStream>().ok_or_else(|| PluginError::Failed("no stream".into()))?;
        let r = self.inst.soft(during, f(s.as_ptr()))?;
        if ok(r) { Ok(stream.take()) } else { Err(PluginError::Failed(format!("{during} returned {r}"))) }
    }
}

impl PluginController for Vst3Controller {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn default_layout(&self) -> &BusLayout {
        &self.default_layout
    }

    fn activate(&mut self, setup: &ProcessSetup) -> Result<Box<dyn Plugin>, PluginError> {
        if setup.max_block_size == 0 || !(setup.sample_rate.is_finite() && setup.sample_rate > 0.0) {
            return Err(PluginError::Failed("the setup needs a sample rate and a block size".into()));
        }
        let (layout, generation) = self.inst.start(setup)?;
        let (tx, rx) = rtrb::RingBuffer::new(EDIT_QUEUE);
        *self.inst.host.to_processor.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
        let mapping = self.inst.read_midi_mapping();
        Ok(Box::new(Vst3Processor::new(self.inst.clone(), generation, layout, setup, rx, mapping)))
    }

    fn deactivate(&mut self) {
        // The running processor goes stale (and any block in progress finishes) first.
        self.inst.stop();
        *self.inst.host.to_processor.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn latency_samples(&mut self) -> u32 {
        // SAFETY: valid processor.
        self.inst.soft("IAudioProcessor::getLatencySamples", unsafe { self.inst.processor.get_latency_samples() }).unwrap_or(0)
    }

    fn tail_samples(&mut self) -> u32 {
        // SAFETY: valid processor.
        self.inst.soft("IAudioProcessor::getTailSamples", unsafe { self.inst.processor.get_tail_samples() }).unwrap_or(0)
    }

    fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    fn refresh_params(&mut self) {
        self.params.clear();
        let Some(c) = self.inst.controller() else { return };
        // SAFETY: valid controller.
        let n = self.inst.soft("IEditController::getParameterCount", unsafe { c.get_parameter_count() }).unwrap_or(0).clamp(0, MAX_PARAMS);
        for i in 0..n {
            // SAFETY: plain data; all-zero is valid.
            let mut info: sv::ParameterInfo = unsafe { std::mem::zeroed() };
            // SAFETY: index below the count; `info` is writable.
            if !matches!(unsafe { c.get_parameter_info(i, &mut info) }, Ok(sb::kResultOk)) {
                continue;
            }
            use sv::ParameterInfo_::ParameterFlags_ as f;
            let flags = info.flags;
            let d = info.defaultNormalizedValue;
            self.params.push(ParamInfo {
                id: info.id,
                name: read_c16(&info.title),
                short_name: read_c16(&info.shortTitle),
                units: read_c16(&info.units),
                default: if d.is_finite() { d.clamp(0.0, 1.0) } else { 0.0 },
                step_count: info.stepCount.max(0) as u32,
                automatable: flags & f::kCanAutomate != 0,
                hidden: flags & f::kIsHidden != 0,
                read_only: flags & f::kIsReadOnly != 0,
                is_bypass: flags & f::kIsBypass != 0,
            });
        }
    }

    fn param_value(&mut self, id: ParamId) -> Option<f64> {
        let c = self.inst.controller()?;
        // SAFETY: valid controller.
        let v = self.inst.soft("IEditController::getParamNormalized", unsafe { c.get_param_normalized(id) }).ok()?;
        v.is_finite().then(|| v.clamp(0.0, 1.0))
    }

    fn set_param(&mut self, id: ParamId, normalized: f64) {
        if !normalized.is_finite() {
            return;
        }
        let v = normalized.clamp(0.0, 1.0);
        if let Some(c) = self.inst.controller() {
            // SAFETY: valid controller.
            let _ = self.inst.soft("IEditController::setParamNormalized", unsafe { c.set_param_normalized(id, v) });
        }
        self.inst.host.send_to_processor(id, v);
    }

    fn param_text(&mut self, id: ParamId, normalized: f64) -> Option<String> {
        let c = self.inst.controller()?;
        let mut s: sv::String128 = [0; 128];
        // SAFETY: valid controller; `s` is a writable String128.
        let r = self.inst.soft("IEditController::getParamStringByValue", unsafe { c.get_param_string_by_value(id, normalized.clamp(0.0, 1.0), &mut s) }).ok()?;
        ok(r).then(|| read_c16(&s)).filter(|t| !t.is_empty())
    }

    fn param_plain(&mut self, id: ParamId, normalized: f64) -> Option<f64> {
        let c = self.inst.controller()?;
        // SAFETY: valid controller.
        let v = self.inst.soft("IEditController::normalizedParamToPlain", unsafe { c.normalized_to_plain(id, normalized.clamp(0.0, 1.0)) }).ok()?;
        v.is_finite().then_some(v)
    }

    fn save_state(&mut self) -> Result<Vec<u8>, PluginError> {
        self.check_alive()?;
        // SAFETY: valid component; the stream lives in `read_state` for the whole call.
        let comp = self.read_state("IComponent::getState", |s| unsafe { self.inst.component.get_state(s) })?;
        let ctrl = match self.inst.controller() {
            // SAFETY: valid controller; as above. A controller without state is fine.
            Some(c) => self.read_state("IEditController::getState", |s| unsafe { c.get_state(s) }).unwrap_or_default(),
            None => Vec::new(),
        };
        let too_big = || PluginError::Failed("the plugin's state is too large".into());
        let mut out = Vec::with_capacity(12 + comp.len() + ctrl.len());
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&u32::try_from(comp.len()).map_err(|_| too_big())?.to_le_bytes());
        out.extend_from_slice(&comp);
        out.extend_from_slice(&u32::try_from(ctrl.len()).map_err(|_| too_big())?.to_le_bytes());
        out.extend_from_slice(&ctrl);
        Ok(out)
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), PluginError> {
        self.check_alive()?;
        let (comp, ctrl) = split_state(data).ok_or_else(|| PluginError::Failed("not a plugin state".into()))?;
        let stream = MemoryStream::new(comp.to_vec());
        let s = stream.as_com_ref::<sb::IBStream>().ok_or_else(|| PluginError::Failed("no stream".into()))?;
        // SAFETY: valid component; the stream lives on this frame for the call.
        let r = self.inst.soft("IComponent::setState", unsafe { self.inst.component.set_state(s.as_ptr()) })?;
        if !ok(r) {
            return Err(PluginError::Failed(format!("the plugin refused the state (result {r})")));
        }
        if let Some(c) = self.inst.controller() {
            stream.rewind();
            // SAFETY: valid controller; the same stream, rewound.
            let _ = self.inst.soft("IEditController::setComponentState", unsafe { c.set_component_state(s.as_ptr()) });
            if !ctrl.is_empty() {
                let cs = MemoryStream::new(ctrl.to_vec());
                if let Some(r) = cs.as_com_ref::<sb::IBStream>() {
                    // SAFETY: valid controller; the stream lives on this frame for the call.
                    let _ = self.inst.soft("IEditController::setState", unsafe { c.set_state(r.as_ptr()) });
                }
            }
        }
        Ok(())
    }

    fn poll(&mut self, out: &mut Vec<ControllerEvent>) {
        self.inst.host.take_events(out);
    }

    fn take_error(&mut self) -> Option<String> {
        self.inst.take_error()
    }

    fn has_editor(&mut self) -> bool {
        self.inst.controller().is_some_and(Editor::supported)
    }

    fn open_editor(&mut self, parent: NativeParent, on_resize: ResizeCallback) -> Result<EditorSize, PluginError> {
        self.check_alive()?;
        if self.editor.is_some() {
            self.close_editor();
        }
        let c = self.inst.controller().ok_or_else(|| PluginError::Unsupported("this plugin has no editor".into()))?;
        let (editor, size) = Editor::open(&self.inst, c, parent, on_resize)?;
        self.editor = Some(editor);
        Ok(size)
    }

    fn close_editor(&mut self) {
        if let Some(e) = self.editor.take() {
            e.close(&self.inst);
        }
    }

    fn resize_editor(&mut self, size: EditorSize) -> Option<EditorSize> {
        self.editor.as_ref().map(|e| e.resize(&self.inst, size))
    }

    fn editor_resizable(&mut self) -> bool {
        self.editor.as_ref().is_some_and(|e| e.resizable(&self.inst))
    }
}

impl Drop for Vst3Controller {
    fn drop(&mut self) {
        // The editor goes before the controller it belongs to.
        self.close_editor();
        self.inst.shutdown();
    }
}

/// Splits a state blob into (component, controller) parts. A blob without our container header
/// is taken as a bare component state.
fn split_state(data: &[u8]) -> Option<(&[u8], &[u8])> {
    if data.len() > MAX_STATE_BYTES.saturating_mul(2) {
        return None;
    }
    let Some(rest) = data.strip_prefix(STATE_MAGIC) else { return (!data.is_empty()).then_some((data, &[][..])) };
    let take = |b: &[u8]| -> Option<(usize, usize)> {
        let n = u32::from_le_bytes(b.get(..4)?.try_into().ok()?) as usize;
        (n <= MAX_STATE_BYTES && b.len() >= 4 + n).then_some((4, 4 + n))
    };
    let (a, b) = take(rest)?;
    let comp = rest.get(a..b)?;
    let rest = rest.get(b..)?;
    let (c, d) = take(rest)?;
    let ctrl = rest.get(c..d)?;
    (d == rest.len()).then_some((comp, ctrl))
}

// ---- the processor (audio thread) ----------------------------------------------------------

/// Where one bus's channels sit in the flat channel table.
struct BusSlot {
    first: usize,
    channels: usize,
}

/// The audio-thread half of a hosted VST3 plugin.
///
/// Everything `process` touches is allocated in `new`: the bus tables, the event and parameter
/// lists, the process context.
pub struct Vst3Processor {
    inst: Arc<Instance>,
    generation: u64,
    layout: BusLayout,
    max_block: usize,
    /// `kRealtime` or `kOffline`, as activated.
    process_mode: i32,
    /// Edits from the plugin's editor, waiting to go into the next block.
    edits: rtrb::Consumer<ParamChange>,
    midi_map: Box<[u32]>,
    in_events: ComWrapper<EventList>,
    out_events: ComWrapper<EventList>,
    in_params: ComWrapper<ParamChanges>,
    out_params: ComWrapper<ParamChanges>,
    context: sv::ProcessContext,
    // Pointer tables VST3 reads. `in_ptrs`/`out_ptrs` are rewritten every block; the bus tables
    // point into them and are fixed at construction.
    in_ptrs: Vec<*mut f32>,
    out_ptrs: Vec<*mut f32>,
    in_buses: Vec<sv::AudioBusBuffers>,
    out_buses: Vec<sv::AudioBusBuffers>,
    out_slots: Vec<BusSlot>,
    // The COM interface pointers handed to the plugin, fixed for the processor's life.
    p_in_events: *mut sv::IEventList,
    p_out_events: *mut sv::IEventList,
    p_in_params: *mut sv::IParameterChanges,
    p_out_params: *mut sv::IParameterChanges,
}

// SAFETY: the raw pointers point into this struct's own heap tables or at COM objects it owns; it
// moves to the audio thread as a whole and is used by one thread at a time.
unsafe impl Send for Vst3Processor {}

impl Vst3Processor {
    fn new(inst: Arc<Instance>, generation: u64, layout: BusLayout, setup: &ProcessSetup, edits: rtrb::Consumer<ParamChange>, midi_map: Box<[u32]>) -> Vst3Processor {
        let in_events = ComWrapper::new(EventList::new(EVENT_CAP));
        let out_events = ComWrapper::new(EventList::new(EVENT_CAP));
        let in_params = ComWrapper::new(ParamChanges::new(IN_PARAM_QUEUES));
        let out_params = ComWrapper::new(ParamChanges::new(OUT_PARAM_QUEUES));
        let ptr_of = |r: Option<vst3::ComRef<'_, sv::IEventList>>| r.map_or(std::ptr::null_mut(), |r| r.as_ptr());
        let p_in_events = ptr_of(in_events.as_com_ref::<sv::IEventList>());
        let p_out_events = ptr_of(out_events.as_com_ref::<sv::IEventList>());
        let params_ptr = |w: &ComWrapper<ParamChanges>| w.as_com_ref::<sv::IParameterChanges>().map_or(std::ptr::null_mut(), |r| r.as_ptr());
        let (p_in_params, p_out_params) = (params_ptr(&in_params), params_ptr(&out_params));
        // SAFETY: `ProcessContext` is plain data (numbers and small structs); all-zero is valid.
        let mut context: sv::ProcessContext = unsafe { std::mem::zeroed() };
        context.sampleRate = setup.sample_rate;
        let mut p = Vst3Processor {
            inst,
            generation,
            max_block: setup.max_block_size as usize,
            process_mode: if setup.realtime { sv::ProcessModes_::kRealtime } else { sv::ProcessModes_::kOffline } as i32,
            edits,
            midi_map,
            in_events,
            out_events,
            in_params,
            out_params,
            context,
            in_ptrs: vec![std::ptr::null_mut(); layout.input_channels()],
            out_ptrs: vec![std::ptr::null_mut(); layout.output_channels()],
            in_buses: Vec::new(),
            out_buses: Vec::new(),
            out_slots: Vec::new(),
            p_in_events,
            p_out_events,
            p_in_params,
            p_out_params,
            layout,
        };
        let (ins, _) = bus_table(&p.layout.audio_inputs, &mut p.in_ptrs);
        let (outs, slots) = bus_table(&p.layout.audio_outputs, &mut p.out_ptrs);
        p.in_buses = ins;
        p.out_buses = outs;
        p.out_slots = slots;
        p
    }

    /// Fills the outputs with silence.
    fn silence(block: &mut ProcessBlock<'_, '_>) {
        for ch in block.outputs.iter_mut() {
            let n = block.frames.min(ch.len());
            ch[..n].fill(0.0);
        }
    }

    fn run(&mut self, block: &mut ProcessBlock<'_, '_>) -> Result<(), ProcessError> {
        let frames = block.frames;
        if frames > self.max_block
            || block.inputs.len() != self.in_ptrs.len()
            || block.outputs.len() != self.out_ptrs.len()
            || block.inputs.iter().any(|c| c.len() < frames)
            || block.outputs.iter().any(|c| c.len() < frames)
        {
            Self::silence(block);
            return Err(ProcessError::BadBlock);
        }
        if frames == 0 {
            return Ok(());
        }

        // Audio: point VST3's tables at the caller's buffers (the plugin must not write inputs).
        for (p, c) in self.in_ptrs.iter_mut().zip(block.inputs) {
            *p = c.as_ptr().cast_mut();
        }
        for (p, c) in self.out_ptrs.iter_mut().zip(block.outputs.iter_mut()) {
            *p = c.as_mut_ptr();
        }
        for b in self.in_buses.iter_mut().chain(self.out_buses.iter_mut()) {
            b.silenceFlags = 0;
        }

        // Notes and MIDI. Notes become events; controllers become parameter changes through the
        // plugin's MIDI mapping.
        self.in_events.clear();
        self.in_params.clear();
        self.out_events.clear();
        self.out_params.clear();
        while let Ok(edit) = self.edits.pop() {
            self.in_params.add(edit.id, 0, edit.value);
        }
        // Offsets past the block would send the plugin outside its buffers: they land on the last
        // frame instead.
        let last = (frames - 1) as u32;
        for e in block.events {
            let e = Event { offset: e.offset.min(last), ..*e };
            if let Some(v) = events::to_vst3_event(&e) {
                self.in_events.push(v);
            } else if let Some((ch, ctrl, value)) = midi_as_param(&e.kind) {
                let id = self.midi_map[ch * MIDI_CONTROLLERS + ctrl];
                if id != u32::MAX {
                    self.in_params.add(id, e.offset, value);
                }
            }
        }
        for c in block.param_changes {
            self.in_params.add(c.id, c.offset.min(last), c.value.clamp(0.0, 1.0));
        }

        apply_transport(&mut self.context, block.transport);

        let mut data = sv::ProcessData {
            processMode: self.process_mode,
            symbolicSampleSize: sv::SymbolicSampleSizes_::kSample32 as i32,
            numSamples: frames as i32,
            numInputs: self.in_buses.len() as i32,
            numOutputs: self.out_buses.len() as i32,
            inputs: if self.in_buses.is_empty() { std::ptr::null_mut() } else { self.in_buses.as_mut_ptr() },
            outputs: if self.out_buses.is_empty() { std::ptr::null_mut() } else { self.out_buses.as_mut_ptr() },
            inputParameterChanges: self.p_in_params,
            outputParameterChanges: self.p_out_params,
            inputEvents: self.p_in_events,
            outputEvents: self.p_out_events,
            processContext: &mut self.context,
        };
        // SAFETY: valid, active processor. Every channel pointer refers to `frames` (or more) valid
        // f32s borrowed from `block` for the whole call; the bus tables, the parameter/event
        // objects and the context are owned by `self` and untouched until the call returns.
        let r = unsafe { self.inst.processor.process(&mut data) };
        match r {
            Ok(code) if ok(code) => {}
            Ok(code) => {
                Self::silence(block);
                return Err(ProcessError::Plugin(code));
            }
            Err(e) => {
                // The plugin threw: it is switched off and the block is silent.
                self.inst.fail("IAudioProcessor::process", &e.0);
                Self::silence(block);
                return Err(ProcessError::Failed);
            }
        }

        // Channels the plugin marked silent need not have been written.
        for (bus, slot) in self.out_buses.iter().zip(&self.out_slots) {
            if bus.silenceFlags != 0 {
                for ch in 0..slot.channels.min(64) {
                    if bus.silenceFlags & (1u64 << ch) != 0
                        && let Some(c) = block.outputs.get_mut(slot.first + ch)
                    {
                        c[..frames].fill(0.0);
                    }
                }
            }
        }
        for i in 0..self.out_events.len() {
            if let Some(e) = self.out_events.get(i).and_then(|e| events::from_vst3_event(&e)) {
                block.midi_out.push(e);
            }
        }
        let out = &mut *block.param_out;
        self.out_params.for_each(|id, offset, value| {
            out.push(ParamChange { id, offset: offset.max(0) as u32, value });
        });
        Ok(())
    }

}

impl Plugin for Vst3Processor {
    fn info(&self) -> &PluginInfo {
        &self.inst.info
    }

    fn layout(&self) -> &BusLayout {
        &self.layout
    }

    fn process(&mut self, block: &mut ProcessBlock<'_, '_>) -> Result<(), ProcessError> {
        let inst = &*self.inst;
        if inst.dead.load(Ordering::Acquire) || inst.generation.load(Ordering::Acquire) != self.generation {
            Self::silence(block);
            return Err(ProcessError::Stale);
        }
        if inst.is_failed() {
            Self::silence(block);
            return Err(ProcessError::Failed);
        }
        // Say a call is running, then look again: either `stop`/`shutdown` sees us and waits, or
        // we see what it changed and do not call the plugin.
        inst.in_process.store(true, Ordering::SeqCst);
        if inst.dead.load(Ordering::SeqCst) || inst.generation.load(Ordering::SeqCst) != self.generation {
            inst.in_process.store(false, Ordering::Release);
            Self::silence(block);
            return Err(ProcessError::Stale);
        }
        let r = self.run(block);
        self.inst.in_process.store(false, Ordering::Release);
        r
    }
}

/// The bus tables for one direction: one `AudioBusBuffers` per bus (inactive buses get none),
/// pointing into `ptrs`, which holds the channels of the active buses one after another.
fn bus_table(buses: &[BusInfo], ptrs: &mut [*mut f32]) -> (Vec<sv::AudioBusBuffers>, Vec<BusSlot>) {
    let mut table = Vec::with_capacity(buses.len());
    let mut slots = Vec::with_capacity(buses.len());
    let mut next = 0usize;
    for b in buses {
        let channels = if b.active { b.channels as usize } else { 0 };
        let base = if channels > 0 { ptrs[next..].as_mut_ptr() } else { std::ptr::null_mut() };
        table.push(sv::AudioBusBuffers { numChannels: channels as i32, silenceFlags: 0, __field0: sv::AudioBusBuffers__type0 { channelBuffers32: base } });
        slots.push(BusSlot { first: next, channels });
        next += channels;
    }
    (table, slots)
}

/// The (channel, controller index, normalized value) a MIDI message becomes as a parameter.
fn midi_as_param(kind: &EventKind) -> Option<(usize, usize, f64)> {
    match *kind {
        EventKind::ControlChange { channel, controller, value } if controller < 128 => Some((usize::from(channel & 15), usize::from(controller), f64::from(value))),
        EventKind::PitchBend { channel, value } => Some((usize::from(channel & 15), PITCH_BEND, (f64::from(value) + 1.0) * 0.5)),
        EventKind::ChannelPressure { channel, pressure } => Some((usize::from(channel & 15), AFTERTOUCH, f64::from(pressure))),
        _ => None,
    }
}

fn apply_transport(c: &mut sv::ProcessContext, t: &Transport) {
    use sv::ProcessContext_::StatesAndFlags_::*;
    let mut state = kTempoValid | kTimeSigValid | kProjectTimeMusicValid | kBarPositionValid | kContTimeValid;
    if t.playing {
        state |= kPlaying;
    }
    if t.recording {
        state |= kRecording;
    }
    if t.looping {
        state |= kCycleActive | kCycleValid;
    }
    // The flag constants are the C default enum type: i32 on Windows, u32 elsewhere.
    #[allow(clippy::unnecessary_cast)]
    let state = state as u32;
    c.state = state;
    c.tempo = t.tempo;
    c.timeSigNumerator = i32::try_from(t.time_sig_numerator).unwrap_or(4);
    c.timeSigDenominator = i32::try_from(t.time_sig_denominator).unwrap_or(4);
    c.projectTimeSamples = t.project_samples;
    c.continousTimeSamples = t.continuous_samples;
    c.projectTimeMusic = t.project_beats;
    c.barPositionMusic = t.bar_start_beats;
    c.cycleStartMusic = t.loop_start_beats;
    c.cycleEndMusic = t.loop_end_beats;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transport_fills_the_process_context() {
        use sv::ProcessContext_::StatesAndFlags_::*;
        // SAFETY: `ProcessContext` is plain data (numbers and small structs); all-zero is valid.
        let mut c: sv::ProcessContext = unsafe { std::mem::zeroed() };
        let t = Transport {
            playing: true,
            tempo: 90.0,
            time_sig_numerator: 3,
            time_sig_denominator: 4,
            project_samples: 96_000,
            continuous_samples: 100_000,
            project_beats: 3.5,
            bar_start_beats: 3.0,
            ..Transport::default()
        };
        apply_transport(&mut c, &t);
        let playing = kPlaying as u32;
        assert_eq!(c.state & playing, playing);
        assert_eq!(c.state & kRecording as u32, 0);
        assert_eq!(c.state & kCycleActive as u32, 0);
        assert_eq!((c.tempo, c.timeSigNumerator, c.timeSigDenominator), (90.0, 3, 4));
        assert_eq!((c.projectTimeSamples, c.continousTimeSamples), (96_000, 100_000));
        assert_eq!((c.projectTimeMusic, c.barPositionMusic), (3.5, 3.0));
        apply_transport(&mut c, &Transport { playing: false, looping: true, loop_start_beats: 4.0, loop_end_beats: 8.0, ..t });
        assert_eq!(c.state & playing, 0);
        assert_eq!(c.state & kCycleActive as u32, kCycleActive as u32);
        assert_eq!((c.cycleStartMusic, c.cycleEndMusic), (4.0, 8.0));
    }

    #[test]
    fn state_container_parses_strictly() {
        let mut blob = STATE_MAGIC.to_vec();
        blob.extend_from_slice(&3u32.to_le_bytes());
        blob.extend_from_slice(b"abc");
        blob.extend_from_slice(&2u32.to_le_bytes());
        blob.extend_from_slice(b"xy");
        assert_eq!(split_state(&blob), Some((&b"abc"[..], &b"xy"[..])));
        assert_eq!(split_state(b"raw component"), Some((&b"raw component"[..], &[][..])), "bare component state");
        assert_eq!(split_state(b""), None);
        let mut trailing = blob.clone();
        trailing.push(0);
        assert_eq!(split_state(&trailing), None);
        assert_eq!(split_state(&blob[..blob.len() - 1]), None);
        let mut huge = STATE_MAGIC.to_vec();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(split_state(&huge), None);
    }

    #[test]
    fn midi_becomes_parameters() {
        let cc = EventKind::ControlChange { channel: 3, controller: 7, value: 0.5 };
        assert_eq!(midi_as_param(&cc), Some((3, 7, 0.5)));
        assert_eq!(midi_as_param(&EventKind::PitchBend { channel: 0, value: -1.0 }), Some((0, PITCH_BEND, 0.0)));
        assert_eq!(midi_as_param(&EventKind::PitchBend { channel: 0, value: 1.0 }), Some((0, PITCH_BEND, 1.0)));
        assert_eq!(midi_as_param(&EventKind::ChannelPressure { channel: 1, pressure: 0.25 }), Some((1, AFTERTOUCH, 0.25)));
        assert_eq!(midi_as_param(&EventKind::NoteOn { channel: 0, key: 60, velocity: 1.0, note_id: -1 }), None);
    }

    #[test]
    fn bus_tables_skip_inactive_buses() {
        let buses = vec![
            BusInfo { name: "Main".into(), channels: 2, kind: BusKind::Main, active: true },
            BusInfo { name: "Aux".into(), channels: 2, kind: BusKind::Aux, active: false },
            BusInfo { name: "Aux 2".into(), channels: 1, kind: BusKind::Aux, active: true },
        ];
        let mut ptrs = vec![std::ptr::null_mut(); 3];
        let (table, slots) = bus_table(&buses, &mut ptrs);
        assert_eq!(table.iter().map(|b| b.numChannels).collect::<Vec<_>>(), vec![2, 0, 1]);
        assert_eq!(slots.iter().map(|s| (s.first, s.channels)).collect::<Vec<_>>(), vec![(0, 2), (2, 0), (2, 1)]);
    }
}

