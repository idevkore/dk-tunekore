//! A live VST3 plugin: the shared instance, and its two halves.
//!
//! Derived from soundCraft's `vst3-host` (`ffi.rs`), MIT OR Apache-2.0; see
//! `third_party/soundcraft/`. What changed: the instance is split into [`Vst3Controller`]
//! (main thread) and [`Vst3Processor`] (audio thread) behind the `Plugin` traits; `process` takes
//! multiple buses, many events and automation points, and a full transport, and allocates,
//! locks and logs nothing, even when the plugin throws; every call goes through the
//! exception-catching shim; a plugin that throws or hangs is switched off and its objects leaked
//! instead of being called again; and the plugin's objects are released on the controller's
//! (main) thread, never on the audio thread.

use super::editor::Editor;
use super::events::{self, EventList, ParamChanges, AFTERTOUCH, PITCH_BEND};
use super::host::{HostContext, MemoryStream, MAX_STATE_BYTES};
use super::module::Bundle;
use super::ptr::PlugPtr;
use super::shim::{self, Exception};
use super::util::{parse_hex, read_c16, tuid_bytes};
use crate::plugin::*;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
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
/// Magic of our state container: `TKV3` + u32 LE component length + component state + u32 LE
/// controller length + controller state.
const STATE_MAGIC: &[u8; 4] = b"TKV3";
/// How long stopping or shutting down waits for a block still running on the audio thread.
/// After that the plugin is taken to be hung: it is switched off and leaked, never stopped or
/// terminated under a running `process`. Long, because an offline render can legitimately spend
/// seconds in one block (Kontakt loads samples there).
const PROCESS_WAIT: Duration = Duration::from_secs(10);
/// The MIDI controllers `IMidiMapping` is asked about: CCs 0-127, aftertouch, pitch bend.
const MIDI_CONTROLLERS: usize = PITCH_BEND + 1;
/// Bytes kept of a failure message written on the audio thread.
const RT_MESSAGE: usize = 512;

fn ok(r: i32) -> bool {
    r == sb::kResultOk
}

/// What a guarded call that threw becomes.
fn thrown(during: &'static str, e: Exception) -> PluginError {
    PluginError::Exception { during, message: e.0 }
}

/// The plugin's COM objects. Released together, on the controller's thread (see
/// [`Instance::release_objects`]), or leaked together if the plugin failed.
struct Objects {
    component: PlugPtr<sv::IComponent>,
    processor: PlugPtr<sv::IAudioProcessor>,
    controller: Option<PlugPtr<sv::IEditController>>,
    /// The component's and the controller's connection points, linked to each other.
    connections: Option<(PlugPtr<sv::IConnectionPoint>, PlugPtr<sv::IConnectionPoint>)>,
}

/// A failure message written on the audio thread without allocating, read on the main thread.
struct RtMessage {
    /// 0 empty, 1 being written, 2 written, 3 taken.
    state: AtomicU8,
    len: AtomicUsize,
    buf: UnsafeCell<[u8; RT_MESSAGE]>,
}

impl RtMessage {
    fn new() -> RtMessage {
        RtMessage { state: AtomicU8::new(0), len: AtomicUsize::new(0), buf: UnsafeCell::new([0; RT_MESSAGE]) }
    }

    /// Records `during` and this thread's last exception, the first time only. No allocation.
    fn write(&self, during: &'static str) {
        if self.state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        // SAFETY: the compare-exchange above makes this the only writer, and readers wait for 2.
        let buf = unsafe { &mut *self.buf.get() };
        let mut n = 0;
        for part in [during.as_bytes(), b": "] {
            let k = part.len().min(RT_MESSAGE - n);
            buf[n..n + k].copy_from_slice(&part[..k]);
            n += k;
        }
        n += shim::copy_last_exception(&mut buf[n..]);
        self.len.store(n, Ordering::Relaxed);
        self.state.store(2, Ordering::Release);
    }

    /// The message, once. Allocates: main thread.
    fn take(&self) -> Option<String> {
        if self.state.compare_exchange(2, 3, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return None;
        }
        // SAFETY: state 2 means the writer is done; nobody writes again.
        let buf = unsafe { &*self.buf.get() };
        let raw = String::from_utf8_lossy(&buf[..self.len.load(Ordering::Relaxed)]).into_owned();
        Some(match raw.split_once(": ") {
            Some((during, rest)) => format!("{during}: {}", shim::readable(rest)),
            None => raw,
        })
    }
}

/// The state both halves share.
pub(crate) struct Instance {
    pub info: PluginInfo,
    /// `None` once released or leaked. Only the controller's thread replaces it, and only when no
    /// `process` call can be running (see `release_objects`).
    objects: UnsafeCell<Option<Objects>>,
    /// The controller is the component itself (single-component plugin).
    single: bool,
    controller_initialized: bool,
    pub host: ComWrapper<HostContext>,
    /// The plugin threw, hung, or could not be closed properly: it is never called again, and its
    /// objects are leaked rather than released.
    failed: AtomicBool,
    /// Why, for failures seen on the main thread.
    error: Mutex<Option<String>>,
    /// Why, for a failure in `process`.
    rt_error: RtMessage,
    /// Shut down: `process` refuses to run.
    dead: AtomicBool,
    /// `process` calls running right now (processors of several activations may overlap).
    in_process: AtomicU32,
    /// Bumped at every activation and deactivation; a processor from an earlier one is stale.
    generation: AtomicU64,
    active: AtomicBool,
    processing: AtomicBool,
    /// Which parameter each MIDI controller drives, `[channel * MIDI_CONTROLLERS + controller]`,
    /// `u32::MAX` for none. Rewritten on the main thread when the plugin changes it.
    midi_map: Box<[AtomicU32]>,
    // Last: the bundle keeps the plugin's code mapped until everything above is gone.
    bundle: Arc<Bundle>,
}

// SAFETY: the COM pointers are called under the VST3 threading model, decided by the two halves:
// the controller (one thread at a time, the main thread by contract) and the processor (the audio
// thread). `objects` is only replaced by the controller's thread after the processor is shut out
// (`dead` plus the `in_process` handshake). Everything else shared is atomics, mutexes or the
// host context, whose state is atomics and lock-free queues. COM reference counts are atomic.
unsafe impl Send for Instance {}
// SAFETY: as above.
unsafe impl Sync for Instance {}

impl Instance {
    /// Creates, initialises and wires up one plugin instance.
    pub fn create(bundle: &Arc<Bundle>, info: &PluginInfo) -> Result<Arc<Instance>, PluginError> {
        let cid = parse_hex(&info.class_id).ok_or_else(|| PluginError::NotFound { path: info.path.display().to_string(), class_id: info.class_id.clone() })?;
        let host = ComWrapper::new(HostContext::new(MAX_PARAMS as usize));
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
            Err(e) => {
                // It threw: never touch it again, not even to release it.
                std::mem::forget(component);
                bundle.mark_leaked();
                return Err(thrown("IComponent::initialize", e));
            }
        }
        let Some(processor) = component.query::<sv::IAudioProcessor>() else {
            // SAFETY: initialised above, terminated once.
            let _ = unsafe { component.terminate() };
            return Err(PluginError::Failed("the component has no IAudioProcessor".into()));
        };
        let mut inst = Instance {
            info: info.clone(),
            objects: UnsafeCell::new(Some(Objects { component, processor, controller: None, connections: None })),
            single: false,
            controller_initialized: false,
            host,
            failed: AtomicBool::new(false),
            error: Mutex::new(None),
            rt_error: RtMessage::new(),
            dead: AtomicBool::new(false),
            in_process: AtomicU32::new(0),
            generation: AtomicU64::new(0),
            active: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            midi_map: (0..16 * MIDI_CONTROLLERS).map(|_| AtomicU32::new(u32::MAX)).collect(),
            bundle: bundle.clone(),
        };
        // From here on a failure just returns: dropping `inst` terminates what was initialised.
        inst.find_controller(bundle, host_unknown);
        let handler = inst.host.as_com_ref::<sv::IComponentHandler>().map(|h| h.as_ptr());
        let single = inst.single;
        if let Some(o) = inst.objects.get_mut().as_mut()
            && let Some(c) = &o.controller
        {
            if !single
                && let (Some(a), Some(b)) = (o.component.query::<sv::IConnectionPoint>(), c.query::<sv::IConnectionPoint>())
            {
                // SAFETY: both connection points are valid for as long as the instance lives; they
                // are disconnected in `shutdown` before either side terminates.
                unsafe {
                    let _ = a.connect(b.as_ptr());
                    let _ = b.connect(a.as_ptr());
                }
                o.connections = Some((a, b));
            }
            if let Some(h) = handler {
                // SAFETY: valid controller; the handler (host context) outlives it.
                let _ = unsafe { c.set_component_handler(h) };
            }
        }
        inst.sync_controller_state();
        inst.refresh_midi_mapping();
        Ok(Arc::new(inst))
    }

    /// The edit controller: the component itself, or a separate class it names.
    fn find_controller(&mut self, bundle: &Arc<Bundle>, host_unknown: *mut sb::FUnknown) {
        let name = self.info.name.clone();
        let Some(o) = self.objects.get_mut().as_mut() else { return };
        if let Some(c) = o.component.query::<sv::IEditController>() {
            o.controller = Some(c);
            self.single = true;
            return;
        }
        let mut ccid: sb::TUID = [0; 16];
        // SAFETY: valid component; `ccid` is a writable 16-byte TUID.
        if !matches!(unsafe { o.component.get_controller_class_id(&mut ccid) }, Ok(sb::kResultOk)) {
            return;
        }
        let c = match bundle.create::<sv::IEditController>(&tuid_bytes(&ccid)) {
            Ok(c) => c,
            Err(e) => return log::warn!("vst3 {name}: cannot create the edit controller ({e}); parameters unavailable"),
        };
        // SAFETY: fresh controller; the host context outlives it.
        match unsafe { c.initialize(host_unknown) } {
            Ok(r) if ok(r) => {
                o.controller = Some(c);
                self.controller_initialized = true;
            }
            Ok(_) => log::warn!("vst3 {name}: controller initialize failed; parameters unavailable"),
            Err(e) => {
                log::warn!("vst3 {name}: controller initialize threw ({e}); parameters unavailable");
                std::mem::forget(c);
                bundle.mark_leaked();
            }
        }
    }

    /// The plugin's objects, while they exist.
    fn objs(&self) -> Option<&Objects> {
        // SAFETY: only `release_objects`/`leak_objects` replace the cell, from the controller's
        // thread, when no other reference to its contents can be in use (see `Instance`).
        unsafe { (*self.objects.get()).as_ref() }
    }

    fn controller(&self) -> Option<&PlugPtr<sv::IEditController>> {
        self.objs()?.controller.as_ref()
    }

    /// Switches the plugin off after it threw while being set up: records why. Main thread.
    fn fail(&self, during: &'static str, message: &str) {
        log::error!("vst3 {}: {during}: {message}", self.info.name);
        self.error.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(|| format!("{during}: {message}"));
        self.failed.store(true, Ordering::Release);
    }

    /// Switches the plugin off after it threw in `process`. Audio thread: no allocation, lock or
    /// log; the message waits for `take_error`.
    fn fail_rt(&self, during: &'static str) {
        self.rt_error.write(during);
        self.failed.store(true, Ordering::Release);
    }

    /// Gives up on the plugin without calling it again (it hung, or its editor could not be
    /// closed properly): it is switched off and its objects and module are leaked. Main thread.
    pub fn abandon(&self, why: &str) {
        log::error!("vst3 {}: {why}; leaking it", self.info.name);
        self.error.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(|| why.to_string());
        self.failed.store(true, Ordering::Release);
        self.bundle.mark_leaked();
    }

    /// Plugin objects were leaked on purpose: its module must never be exited.
    pub fn mark_leaked(&self) {
        self.bundle.mark_leaked();
    }

    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn take_error(&self) -> Option<String> {
        if let Some(m) = self.rt_error.take() {
            log::error!("vst3 {}: {m}", self.info.name);
            return Some(m);
        }
        self.error.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Runs a guarded call that sets the plugin up; an exception becomes an error and switches the
    /// plugin off, because its state is no longer known. Main thread.
    fn guard<T>(&self, during: &'static str, r: Result<T, Exception>) -> Result<T, PluginError> {
        r.map_err(|e| {
            self.fail(during, &e.0);
            thrown(during, e)
        })
    }

    /// Runs a guarded call that only asks something (a parameter's text, its state): an
    /// exception becomes an error and is remembered for `take_error`, but the plugin keeps
    /// running. Main thread.
    pub fn soft<T>(&self, during: &'static str, r: Result<T, Exception>) -> Result<T, PluginError> {
        r.map_err(|e| {
            self.note_exception(during, &e.0);
            thrown(during, e)
        })
    }

    /// Records that a call threw, without switching the plugin off. Main thread.
    pub fn note_exception(&self, during: &'static str, message: &str) {
        log::warn!("vst3 {}: {during}: {message}", self.info.name);
        self.error.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(|| format!("{during}: {message}"));
    }

    /// Copies the component's state into the controller (`getState` → `setComponentState`).
    fn sync_controller_state(&self) {
        let Some(o) = self.objs() else { return };
        let Some(c) = &o.controller else { return };
        let stream = MemoryStream::new(Vec::new());
        let Some(s) = stream.as_com_ref::<sb::IBStream>() else { return };
        // SAFETY: valid component; the stream lives on this frame for the whole call.
        if matches!(self.soft("IComponent::getState", unsafe { o.component.get_state(s.as_ptr()) }), Ok(sb::kResultOk)) {
            stream.rewind();
            // SAFETY: valid controller; same stream, rewound.
            let _ = self.soft("IEditController::setComponentState", unsafe { c.set_component_state(s.as_ptr()) });
        }
    }

    // ---- buses ----------------------------------------------------------------------------

    fn bus_count(&self, media: sv::MediaTypes, dir: sv::BusDirections) -> i32 {
        let Some(o) = self.objs() else { return 0 };
        // SAFETY: valid component.
        unsafe { o.component.get_bus_count(media as i32, dir as i32) }.unwrap_or(0).clamp(0, MAX_BUSES)
    }

    fn arrangement(&self, dir: sv::BusDirections, index: i32) -> Option<sv::SpeakerArrangement> {
        let o = self.objs()?;
        let mut arr: sv::SpeakerArrangement = 0;
        // SAFETY: valid processor; `arr` is writable.
        matches!(unsafe { o.processor.get_bus_arrangement(dir as i32, index, &mut arr) }, Ok(sb::kResultOk)).then_some(arr)
    }

    fn audio_buses(&self, dir: sv::BusDirections) -> Vec<BusInfo> {
        let Some(o) = self.objs() else { return Vec::new() };
        (0..self.bus_count(sv::MediaTypes_::kAudio, dir))
            .filter_map(|i| {
                // SAFETY: plain data; all-zero is valid.
                let mut b: sv::BusInfo = unsafe { std::mem::zeroed() };
                // SAFETY: valid component; index below the bus count; `b` is writable.
                if !matches!(unsafe { o.component.get_bus_info(sv::MediaTypes_::kAudio as i32, dir as i32, i, &mut b) }, Ok(sb::kResultOk)) {
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

    /// Waits for every running `process` call to return. `false` if one is still running after
    /// [`PROCESS_WAIT`].
    fn wait_for_process(&self) -> bool {
        let t = Instant::now();
        while self.in_process.load(Ordering::SeqCst) != 0 {
            if t.elapsed() >= PROCESS_WAIT {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    /// Makes the current processor stale and waits for a block it may be running. Then stops
    /// processing and deactivates (when needed). A block that never ends leaves the plugin
    /// switched off and leaked: it is not stopped under a running `process`.
    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if !self.wait_for_process() {
            self.abandon("a process call did not return within 10 s");
            return;
        }
        let Some(o) = self.objs() else { return };
        let (was_processing, was_active) = (self.processing.swap(false, Ordering::AcqRel), self.active.swap(false, Ordering::AcqRel));
        if self.is_failed() {
            return;
        }
        if was_processing {
            // SAFETY: valid processor in the processing state; no `process` call is running.
            let _ = unsafe { o.processor.set_processing(0) };
        }
        if was_active {
            // SAFETY: valid, active component.
            let _ = unsafe { o.component.set_active(0) };
        }
    }

    /// Negotiates the bus layout (main buses with `setup.main_channels` channels when the plugin
    /// agrees), switches the chosen buses on, sets up 32-bit processing, activates the component
    /// and starts processing. Returns the layout in force, the new generation number and the
    /// latency.
    pub fn start(&self, setup: &ProcessSetup) -> Result<(BusLayout, u64, u32), PluginError> {
        if self.is_failed() {
            return Err(PluginError::Poisoned(self.take_error().unwrap_or_default()));
        }
        self.stop();
        if self.is_failed() {
            return Err(PluginError::Poisoned(self.take_error().unwrap_or_default()));
        }
        let o = self.objs().ok_or_else(|| PluginError::Failed("the plugin is gone".into()))?;
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
        let r = unsafe { o.processor.set_bus_arrangements(ins.as_mut_ptr(), nin, outs.as_mut_ptr(), nout) };
        if !self.guard("IAudioProcessor::setBusArrangements", r)?.eq(&sb::kResultOk) {
            log::debug!("vst3 {}: kept its own bus arrangements", self.info.name);
        }
        let mut layout = self.read_layout();
        for (dir, buses, sel) in [(din, &mut layout.audio_inputs, &setup.inputs), (dout, &mut layout.audio_outputs, &setup.outputs)] {
            let main = main_index(buses);
            for (i, b) in buses.iter_mut().enumerate() {
                b.active = sel.includes(i, main);
                // SAFETY: valid component; index below the bus count.
                let r = unsafe { o.component.activate_bus(audio, dir as i32, i as i32, u8::from(b.active)) };
                self.guard("IComponent::activateBus", r)?;
            }
        }
        for (dir, n) in [(din, layout.event_inputs), (dout, layout.event_outputs)] {
            for i in 0..n as i32 {
                // SAFETY: valid component; event bus index below the count.
                let r = unsafe { o.component.activate_bus(event, dir as i32, i, 1) };
                self.guard("IComponent::activateBus", r)?;
            }
        }
        let sample32 = sv::SymbolicSampleSizes_::kSample32 as i32;
        // SAFETY: valid processor.
        if !ok(self.guard("IAudioProcessor::canProcessSampleSize", unsafe { o.processor.can_process_sample_size(sample32) })?) {
            return Err(PluginError::Failed("the plugin cannot process 32-bit float audio".into()));
        }
        let mut s = sv::ProcessSetup {
            processMode: if setup.realtime { sv::ProcessModes_::kRealtime } else { sv::ProcessModes_::kOffline } as i32,
            symbolicSampleSize: sample32,
            maxSamplesPerBlock: i32::try_from(setup.max_block_size).unwrap_or(i32::MAX),
            sampleRate: setup.sample_rate,
        };
        // SAFETY: valid processor, inactive; `s` is a valid struct for the call.
        if !ok(self.guard("IAudioProcessor::setupProcessing", unsafe { o.processor.setup_processing(&mut s) })?) {
            return Err(PluginError::Failed("setupProcessing failed".into()));
        }
        // SAFETY: valid component, configured above.
        if !ok(self.guard("IComponent::setActive", unsafe { o.component.set_active(1) })?) {
            return Err(PluginError::Failed("setActive failed".into()));
        }
        self.active.store(true, Ordering::Release);
        // Many plugins return kNotImplemented here; that is not an error.
        // SAFETY: valid, active processor.
        self.guard("IAudioProcessor::setProcessing", unsafe { o.processor.set_processing(1) })?;
        self.processing.store(true, Ordering::Release);
        // SAFETY: valid processor.
        let latency = self.soft("IAudioProcessor::getLatencySamples", unsafe { o.processor.get_latency_samples() }).unwrap_or(0);
        // `stop` above retired the previous processor; the new one belongs to this generation.
        Ok((layout, self.generation.load(Ordering::SeqCst), latency))
    }

    /// Reads which parameter each MIDI controller drives (`IMidiMapping`) into the shared table
    /// the processor reads. Main thread.
    fn refresh_midi_mapping(&self) {
        let m = self.controller().and_then(|c| c.query::<sv::IMidiMapping>());
        for ch in 0..16usize {
            for n in 0..MIDI_CONTROLLERS {
                let mut id: sv::ParamID = 0;
                let found = match &m {
                    // SAFETY: valid mapping interface; `id` is a local.
                    Some(m) => matches!(unsafe { m.get_midi_controller_assignment(0, ch as i16, n as i16, &mut id) }, Ok(sb::kResultOk)),
                    None => false,
                };
                self.midi_map[ch * MIDI_CONTROLLERS + n].store(if found { id } else { u32::MAX }, Ordering::Relaxed);
            }
        }
    }

    // ---- shutdown -------------------------------------------------------------------------

    /// Stops the plugin for good: waits for a running `process`, then deactivates, disconnects
    /// and terminates. A plugin that failed, or whose block never returns, is not called. Safe to
    /// call twice.
    pub fn shutdown(&self) {
        if self.dead.swap(true, Ordering::SeqCst) {
            return;
        }
        if !self.wait_for_process() {
            self.abandon("a process call did not return within 10 s");
            return;
        }
        if self.is_failed() {
            return;
        }
        self.stop();
        let Some(o) = self.objs() else { return };
        if self.is_failed() {
            return;
        }
        if let Some((a, b)) = &o.connections {
            // SAFETY: the pair was connected in `create`; disconnect before terminating.
            unsafe {
                let _ = a.disconnect(b.as_ptr());
                let _ = b.disconnect(a.as_ptr());
            }
        }
        if let Some(c) = &o.controller {
            // SAFETY: valid controller; detach our handler before it goes away.
            let _ = unsafe { c.set_component_handler(std::ptr::null_mut()) };
            if self.controller_initialized {
                // SAFETY: a separately created controller, initialised once; terminated once.
                let _ = unsafe { c.terminate() };
            }
        }
        // SAFETY: valid component, inactive; terminated exactly once (`dead` guards this).
        let _ = unsafe { o.component.terminate() };
    }

    /// Releases the plugin's objects (or leaks them, if it failed), running its destructors on
    /// the calling thread. Only after `shutdown`, which guarantees no `process` call is running
    /// or can start; the controller's drop calls it, so destructors run on the main thread even
    /// when the processor outlives the controller.
    fn release_objects(&self) {
        debug_assert!(self.dead.load(Ordering::SeqCst));
        if self.in_process.load(Ordering::SeqCst) != 0 {
            return; // `shutdown` gave up on a hung block and leaked the plugin.
        }
        // SAFETY: shut down with no `process` running, and `process` checks `dead` before it
        // touches the objects; the controller (the only other user) is the caller.
        let objects = unsafe { (*self.objects.get()).take() };
        let Some(o) = objects else { return };
        if self.is_failed() {
            std::mem::forget(o);
            self.bundle.mark_leaked();
            return;
        }
        // The connection points and the controller go before the component.
        let Objects { component, processor, controller, connections } = o;
        drop(connections);
        drop(controller);
        drop(processor);
        drop(component);
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Normally the controller already did both.
        self.shutdown();
        self.release_objects();
    }
}

// ---- the controller (main thread) ----------------------------------------------------------

/// The main-thread half of a hosted VST3 plugin.
pub struct Vst3Controller {
    inst: Arc<Instance>,
    info: PluginInfo,
    default_layout: BusLayout,
    params: Vec<ParamInfo>,
    editor: Option<Editor>,
    failure_reported: bool,
    /// Losses already logged: controller events that did not fit, mailbox writes refused.
    logged_losses: (u32, u32),
}

impl Vst3Controller {
    pub(crate) fn new(inst: Arc<Instance>) -> Vst3Controller {
        let mut c = Vst3Controller { info: inst.info.clone(), default_layout: inst.read_layout(), params: Vec::new(), editor: None, failure_reported: false, logged_losses: (0, 0), inst };
        c.refresh_params();
        c
    }

    fn check_alive(&self) -> Result<(), PluginError> {
        if self.inst.is_failed() {
            Err(PluginError::Poisoned(self.inst.error.lock().unwrap_or_else(PoisonError::into_inner).clone().unwrap_or_else(|| "it threw while processing".into())))
        } else {
            Ok(())
        }
    }

    /// The plugin's objects, unless it failed.
    fn objs(&self) -> Option<&Objects> {
        if self.inst.is_failed() { None } else { self.inst.objs() }
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
        let (layout, generation, latency) = self.inst.start(setup)?;
        Ok(Box::new(Vst3Processor::new(self.inst.clone(), generation, layout, setup, latency)))
    }

    fn deactivate(&mut self) {
        // The running processor goes stale (and any block in progress finishes) first. Values
        // set meanwhile wait in the mailbox for the next activation.
        self.inst.stop();
    }

    fn latency_samples(&mut self) -> u32 {
        let Some(o) = self.objs() else { return 0 };
        // SAFETY: valid processor.
        self.inst.soft("IAudioProcessor::getLatencySamples", unsafe { o.processor.get_latency_samples() }).unwrap_or(0)
    }

    fn tail_samples(&mut self) -> u32 {
        let Some(o) = self.objs() else { return 0 };
        // SAFETY: valid processor.
        self.inst.soft("IAudioProcessor::getTailSamples", unsafe { o.processor.get_tail_samples() }).unwrap_or(0)
    }

    fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    fn refresh_params(&mut self) {
        self.params.clear();
        let inst = Arc::clone(&self.inst);
        if inst.is_failed() {
            return;
        }
        let Some(c) = inst.objs().and_then(|o| o.controller.as_ref()) else { return };
        // SAFETY: valid controller.
        let n = inst.soft("IEditController::getParameterCount", unsafe { c.get_parameter_count() }).unwrap_or(0).clamp(0, MAX_PARAMS);
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
            let plain = |n: f64, fallback: f64| {
                // SAFETY: valid controller.
                match inst.soft("IEditController::normalizedParamToPlain", unsafe { c.normalized_to_plain(info.id, n) }) {
                    Ok(v) if v.is_finite() => v,
                    _ => fallback,
                }
            };
            let (min, max) = (plain(0.0, 0.0), plain(1.0, 1.0));
            self.params.push(ParamInfo {
                id: info.id,
                name: read_c16(&info.title),
                short_name: read_c16(&info.shortTitle),
                units: read_c16(&info.units),
                min,
                max,
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
        let c = self.objs()?.controller.as_ref()?;
        // SAFETY: valid controller.
        let v = self.inst.soft("IEditController::getParamNormalized", unsafe { c.get_param_normalized(id) }).ok()?;
        v.is_finite().then(|| v.clamp(0.0, 1.0))
    }

    fn set_param(&mut self, id: ParamId, normalized: f64) {
        if !normalized.is_finite() || self.inst.is_failed() {
            return;
        }
        let v = normalized.clamp(0.0, 1.0);
        if let Some(c) = self.objs().and_then(|o| o.controller.as_ref()) {
            // SAFETY: valid controller.
            let _ = self.inst.soft("IEditController::setParamNormalized", unsafe { c.set_param_normalized(id, v) });
        }
        self.inst.host.mailbox.post(id, v);
    }

    fn param_text(&mut self, id: ParamId, normalized: f64) -> Option<String> {
        let c = self.objs()?.controller.as_ref()?;
        let mut s: sv::String128 = [0; 128];
        // SAFETY: valid controller; `s` is a writable String128.
        let r = self.inst.soft("IEditController::getParamStringByValue", unsafe { c.get_param_string_by_value(id, normalized.clamp(0.0, 1.0), &mut s) }).ok()?;
        ok(r).then(|| read_c16(&s)).filter(|t| !t.is_empty())
    }

    fn param_plain(&mut self, id: ParamId, normalized: f64) -> Option<f64> {
        let c = self.objs()?.controller.as_ref()?;
        // SAFETY: valid controller.
        let v = self.inst.soft("IEditController::normalizedParamToPlain", unsafe { c.normalized_to_plain(id, normalized.clamp(0.0, 1.0)) }).ok()?;
        v.is_finite().then_some(v)
    }

    fn save_state(&mut self) -> Result<Vec<u8>, PluginError> {
        self.check_alive()?;
        let o = self.objs().ok_or_else(|| PluginError::Failed("the plugin is gone".into()))?;
        // SAFETY: valid component; the stream lives in `read_state` for the whole call.
        let comp = self.read_state("IComponent::getState", |s| unsafe { o.component.get_state(s) })?;
        let ctrl = match &o.controller {
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

    fn has_pending_params(&self) -> bool {
        self.inst.host.mailbox.pending()
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), PluginError> {
        self.check_alive()?;
        let o = self.objs().ok_or_else(|| PluginError::Failed("the plugin is gone".into()))?;
        let (comp, ctrl) = split_state(data).ok_or_else(|| PluginError::Failed("not a plugin state".into()))?;
        let stream = MemoryStream::new(comp.to_vec());
        let s = stream.as_com_ref::<sb::IBStream>().ok_or_else(|| PluginError::Failed("no stream".into()))?;
        // SAFETY: valid component; the stream lives on this frame for the call.
        let r = self.inst.soft("IComponent::setState", unsafe { o.component.set_state(s.as_ptr()) })?;
        if !ok(r) {
            return Err(PluginError::Failed(format!("the plugin refused the state (result {r})")));
        }
        if let Some(c) = &o.controller {
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
        if self.inst.host.midi_mapping_changed.swap(false, Ordering::AcqRel) && !self.inst.is_failed() {
            self.inst.refresh_midi_mapping();
        }
        let losses = (self.inst.host.events_dropped(), self.inst.host.mailbox.refused());
        if losses != self.logged_losses {
            log::warn!("vst3 {}: {} editor event(s) dropped and {} parameter value(s) refused so far", self.info.name, losses.0, losses.1);
            self.logged_losses = losses;
        }
        if self.inst.is_failed() && !self.failure_reported {
            self.failure_reported = true;
            out.push(ControllerEvent::Failed);
        }
    }

    fn take_error(&mut self) -> Option<String> {
        self.inst.take_error()
    }

    fn has_editor(&mut self) -> bool {
        let inst = &self.inst;
        self.objs().and_then(|o| o.controller.as_ref()).is_some_and(|c| Editor::supported(inst, c))
    }

    fn open_editor(&mut self, parent: NativeParent, on_resize: ResizeCallback) -> Result<EditorSize, PluginError> {
        self.check_alive()?;
        if self.editor.is_some() {
            self.close_editor();
        }
        let c = self.objs().and_then(|o| o.controller.as_ref()).ok_or_else(|| PluginError::Unsupported("this plugin has no editor".into()))?;
        let (editor, size) = Editor::open(&self.inst, c, parent, on_resize)?;
        self.editor = Some(editor);
        Ok(size)
    }

    fn close_editor(&mut self) {
        if let Some(e) = self.editor.take()
            && e.close(&self.inst)
        {
            // The view was leaked (closed off the main thread, or the plugin had failed): its
            // timers may still call into the plugin, so the plugin must not be terminated either.
            self.inst.abandon("its editor could not be closed properly");
        }
    }

    fn resize_editor(&mut self, size: EditorSize) -> Option<EditorSize> {
        if self.inst.is_failed() {
            return None;
        }
        self.editor.as_ref().map(|e| e.resize(&self.inst, size))
    }

    fn editor_resizable(&mut self) -> bool {
        !self.inst.is_failed() && self.editor.as_ref().is_some_and(|e| e.resizable(&self.inst))
    }
}

impl Drop for Vst3Controller {
    fn drop(&mut self) {
        // The editor goes before the controller it belongs to.
        self.close_editor();
        self.inst.shutdown();
        // The plugin's destructors run here, on the controller's thread, even when the audio
        // thread still holds the processor.
        self.inst.release_objects();
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
    latency: u32,
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
    dropped: Dropped,
}

// SAFETY: the raw pointers point into this struct's own heap tables or at COM objects it owns; it
// moves to the audio thread as a whole and is used by one thread at a time.
unsafe impl Send for Vst3Processor {}

impl Vst3Processor {
    fn new(inst: Arc<Instance>, generation: u64, layout: BusLayout, setup: &ProcessSetup, latency: u32) -> Vst3Processor {
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
            latency,
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
            dropped: Dropped::default(),
            layout,
        };
        let (ins, _) = bus_table(&p.layout.audio_inputs, &mut p.in_ptrs);
        let (outs, slots) = bus_table(&p.layout.audio_outputs, &mut p.out_ptrs);
        p.in_buses = ins;
        p.out_buses = outs;
        p.out_slots = slots;
        p
    }

    /// The parameter changes for this block: values waiting in the mailbox (the UI, the editor)
    /// at its start, then MIDI controllers through the plugin's mapping, then the caller's.
    fn gather(&mut self, block: &ProcessBlock<'_>, last: u32) {
        let in_params = &self.in_params;
        self.inst.host.mailbox.drain(|id, v| {
            in_params.add(id, 0, v);
        });
        for e in block.events {
            let e = Event { offset: e.offset.min(last), ..*e };
            if let Some((ch, ctrl, value)) = midi_as_param(&e.kind) {
                let id = self.inst.midi_map[ch * MIDI_CONTROLLERS + ctrl].load(Ordering::Relaxed);
                if id != u32::MAX {
                    self.in_params.add(id, e.offset, value);
                }
            } else if block.frames > 0
                && let Some(v) = events::to_vst3_event(&e)
            {
                self.in_events.push(v);
            }
        }
        for c in block.param_changes {
            self.in_params.add(c.id, c.offset.min(last), c.value.clamp(0.0, 1.0));
        }
    }

    fn run(&mut self, block: &mut ProcessBlock<'_>) -> Result<(), ProcessError> {
        let frames = block.frames;
        if frames > self.max_block
            || block.inputs.channels() != self.in_ptrs.len()
            || block.outputs.channels() != self.out_ptrs.len()
            || (frames > 0 && (block.outputs.len() < frames || (!self.in_ptrs.is_empty() && block.inputs.len() < frames)))
        {
            block.outputs.silence(frames);
            return Err(ProcessError::BadBlock);
        }
        self.in_events.clear();
        self.in_params.clear();
        self.out_events.clear();
        self.out_params.clear();
        // Offsets past the block would send the plugin outside its buffers: they land on its last
        // frame instead.
        self.gather(block, frames.saturating_sub(1) as u32);
        apply_transport(&mut self.context, block.transport);
        let flush = frames == 0;
        if flush && self.in_params.queue_count() == 0 {
            return Ok(());
        }
        if !flush {
            // Audio: point VST3's tables at the caller's buffers (the plugin must not write inputs,
            // unless an input is also an output).
            for (p, &c) in self.in_ptrs.iter_mut().zip(block.inputs.as_ptrs()) {
                *p = c.cast_mut();
            }
            for (p, &c) in self.out_ptrs.iter_mut().zip(block.outputs.as_ptrs()) {
                *p = c;
            }
            for b in self.in_buses.iter_mut().chain(self.out_buses.iter_mut()) {
                b.silenceFlags = 0;
            }
        }
        // A zero-frame call only delivers parameters: no audio buses, as VST3 hosts flush.
        let (n_in, n_out) = if flush { (0, 0) } else { (self.in_buses.len(), self.out_buses.len()) };
        let mut data = sv::ProcessData {
            processMode: self.process_mode,
            symbolicSampleSize: sv::SymbolicSampleSizes_::kSample32 as i32,
            numSamples: frames as i32,
            numInputs: n_in as i32,
            numOutputs: n_out as i32,
            inputs: if n_in == 0 { std::ptr::null_mut() } else { self.in_buses.as_mut_ptr() },
            outputs: if n_out == 0 { std::ptr::null_mut() } else { self.out_buses.as_mut_ptr() },
            inputParameterChanges: self.p_in_params,
            outputParameterChanges: self.p_out_params,
            inputEvents: self.p_in_events,
            outputEvents: self.p_out_events,
            processContext: &mut self.context,
        };
        let Some(o) = self.inst.objs() else {
            block.outputs.silence(frames);
            return Err(ProcessError::Stale);
        };
        // SAFETY: valid, active processor. Every channel pointer refers to `frames` (or more) valid
        // f32s borrowed from `block` for the whole call; the bus tables, the parameter/event
        // objects and the context are owned by `self` and untouched until the call returns.
        let r = unsafe { o.processor.process_rt(&mut data) };
        self.dropped.events = self.dropped.events.saturating_add(self.in_events.take_dropped()).saturating_add(self.out_events.take_dropped());
        self.dropped.param_points = self.dropped.param_points.saturating_add(self.in_params.take_dropped()).saturating_add(self.out_params.take_dropped());
        match r {
            Ok(code) if ok(code) => {}
            Ok(code) => {
                block.outputs.silence(frames);
                return Err(ProcessError::Plugin(code));
            }
            Err(shim::Thrown) => {
                // The plugin threw: it is switched off and the block is silent. Nothing here
                // allocates, locks or logs; the controller reports it later.
                self.inst.fail_rt("IAudioProcessor::process");
                block.outputs.silence(frames);
                return Err(ProcessError::Failed);
            }
        }
        if !flush {
            // Channels the plugin marked silent need not have been written.
            let outs = block.outputs.as_ptrs();
            for (bus, slot) in self.out_buses.iter().zip(&self.out_slots) {
                if bus.silenceFlags != 0 {
                    for ch in 0..slot.channels.min(64) {
                        if bus.silenceFlags & (1u64 << ch) != 0
                            && let Some(&p) = outs.get(slot.first + ch)
                        {
                            // SAFETY: an output channel of `block`, valid for `frames` writes.
                            unsafe { std::slice::from_raw_parts_mut(p, frames) }.fill(0.0);
                        }
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

    fn latency_samples(&self) -> u32 {
        self.latency
    }

    fn process(&mut self, block: &mut ProcessBlock<'_>) -> Result<(), ProcessError> {
        let inst = &*self.inst;
        if inst.dead.load(Ordering::Acquire) || inst.generation.load(Ordering::Acquire) != self.generation {
            block.outputs.silence(block.frames);
            return Err(ProcessError::Stale);
        }
        if inst.is_failed() {
            block.outputs.silence(block.frames);
            return Err(ProcessError::Failed);
        }
        // Say a call is running, then look again: either `stop`/`shutdown` sees us and waits, or
        // we see what it changed and do not call the plugin.
        inst.in_process.fetch_add(1, Ordering::SeqCst);
        if inst.dead.load(Ordering::SeqCst) || inst.generation.load(Ordering::SeqCst) != self.generation || inst.is_failed() {
            inst.in_process.fetch_sub(1, Ordering::SeqCst);
            block.outputs.silence(block.frames);
            return Err(ProcessError::Stale);
        }
        let r = self.run(block);
        self.inst.in_process.fetch_sub(1, Ordering::SeqCst);
        r
    }

    fn take_dropped(&mut self) -> Dropped {
        std::mem::take(&mut self.dropped)
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
    fn a_bar_number_becomes_quarter_notes() {
        let t = Transport { time_sig_numerator: 6, time_sig_denominator: 8, ..Transport::default() };
        assert_eq!(t.bar_start_from(2), 6.0, "6/8 bars are three quarter notes long");
        assert_eq!(Transport::default().bar_start_from(3), 12.0);
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

    #[test]
    fn a_failure_message_from_the_audio_thread_is_kept_once() {
        let m = RtMessage::new();
        assert_eq!(m.take(), None);
        m.write("IAudioProcessor::process");
        m.write("ignored: only the first failure is kept");
        let s = m.take().unwrap();
        assert!(s.starts_with("IAudioProcessor::process: "), "{s}");
        assert_eq!(m.take(), None, "taken once");
    }
}
