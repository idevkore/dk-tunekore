//! A live CLAP plugin: the shared instance, and its two halves.
//!
//! Derived from soundCraft's `clap-host` (`ffi.rs`: `Instance`), MIT OR Apache-2.0; see
//! `third_party/soundcraft/`. What changed: the instance is split into [`ClapController`] (main
//! thread) and [`ClapProcessor`] (audio thread) behind the `Plugin` traits, as the VST3 host is,
//! with the same handshake: activating again makes the old processor stale, stopping waits for a
//! block in progress, and a block that runs for 10 s leaves the plugin switched off and leaked;
//! every call goes through the exception-catching shim, and a plugin that throws is never called
//! again; `process` takes every port (inactive ones get silent buffers of their own), events,
//! ramped parameter changes and the transport, and allocates, locks and logs nothing; values set
//! from the UI wait in a per-parameter mailbox and parameters are normalized at the interface;
//! `start_processing` runs on the audio thread; the plugin's main-thread requests (callbacks,
//! timers, flushes, resizes) are served from `poll`; and the plugin is destroyed on the
//! controller's thread, never on the audio thread.

use super::editor::Editor;
use super::events::{self, Dialects, InEvents, OutEvents};
use super::host::Host;
use super::module::{read_chars, Module};
use super::shim;
use crate::plugin::*;
use crate::rt::RtMessage;
use crate::vst3::shim::{Exception, Thrown};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::{CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_VALUE};
use clap_sys::ext::audio_ports::{clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS};
use clap_sys::ext::audio_ports_activation::{clap_plugin_audio_ports_activation, CLAP_EXT_AUDIO_PORTS_ACTIVATION, CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT};
use clap_sys::ext::gui::{clap_plugin_gui, CLAP_EXT_GUI};
use clap_sys::ext::latency::{clap_plugin_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::note_ports::{clap_note_port_info, clap_plugin_note_ports, CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI};
use clap_sys::ext::params::*;
use clap_sys::ext::render::{clap_plugin_render, CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE, CLAP_RENDER_REALTIME};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::ext::tail::{clap_plugin_tail, CLAP_EXT_TAIL};
use clap_sys::ext::timer_support::{clap_plugin_timer_support, CLAP_EXT_TIMER_SUPPORT};
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{clap_process, CLAP_PROCESS_ERROR};
use clap_sys::stream::{clap_istream, clap_ostream};
use std::cell::UnsafeCell;
use std::ffi::{c_char, c_void, CStr};
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Most audio ports per direction we provide buffers for.
const MAX_PORTS: u32 = 16;
/// Most channels per audio port.
const MAX_PORT_CHANNELS: u32 = 64;
/// Most parameters read from one plugin.
const MAX_PARAMS: u32 = 4096;
/// Largest plugin state we take from or give to a plugin.
pub(crate) const MAX_STATE_BYTES: usize = 128 << 20;
/// How long stopping or shutting down waits for a block still running on the audio thread (as
/// for VST3: an offline render can spend seconds in one block).
const PROCESS_WAIT: Duration = Duration::from_secs(10);
/// Room for a parameter's text.
const TEXT_BYTES: usize = 256;

/// The extensions a plugin offers, asked for once after `init`. Null where it has none.
#[derive(Clone, Copy)]
struct Exts {
    params: *const clap_plugin_params,
    state: *const clap_plugin_state,
    latency: *const clap_plugin_latency,
    tail: *const clap_plugin_tail,
    audio_ports: *const clap_plugin_audio_ports,
    ports_activation: *const clap_plugin_audio_ports_activation,
    note_ports: *const clap_plugin_note_ports,
    gui: *const clap_plugin_gui,
    timer: *const clap_plugin_timer_support,
    render: *const clap_plugin_render,
}

/// The plugin object and its extensions.
#[derive(Clone, Copy)]
pub(crate) struct Objects {
    pub plugin: *const clap_plugin,
    ext: Exts,
}

impl Objects {
    /// The plugin's own function table.
    pub fn vt(&self) -> clap_plugin {
        // SAFETY: a live plugin object (the instance keeps it until `release_objects`).
        unsafe { *self.plugin }
    }

    fn table<T: Copy>(p: *const T) -> Option<T> {
        // SAFETY: a non-null extension pointer the plugin returned; CLAP keeps it valid for the
        // plugin's life.
        unsafe { p.as_ref() }.copied()
    }

    fn params(&self) -> Option<clap_plugin_params> {
        Self::table(self.ext.params)
    }
    fn state(&self) -> Option<clap_plugin_state> {
        Self::table(self.ext.state)
    }
    fn latency(&self) -> Option<clap_plugin_latency> {
        Self::table(self.ext.latency)
    }
    fn tail(&self) -> Option<clap_plugin_tail> {
        Self::table(self.ext.tail)
    }
    fn audio_ports(&self) -> Option<clap_plugin_audio_ports> {
        Self::table(self.ext.audio_ports)
    }
    fn ports_activation(&self) -> Option<clap_plugin_audio_ports_activation> {
        Self::table(self.ext.ports_activation)
    }
    fn note_ports(&self) -> Option<clap_plugin_note_ports> {
        Self::table(self.ext.note_ports)
    }
    pub fn gui(&self) -> Option<clap_plugin_gui> {
        Self::table(self.ext.gui)
    }
    fn timer(&self) -> Option<clap_plugin_timer_support> {
        Self::table(self.ext.timer)
    }
    fn render(&self) -> Option<clap_plugin_render> {
        Self::table(self.ext.render)
    }
}

/// A parameter's range, for turning normalized values into the plugin's own and back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Range {
    id: u32,
    min: f64,
    max: f64,
    stepped: bool,
}

impl Range {
    fn plain(&self, normalized: f64) -> f64 {
        let v = self.min + normalized.clamp(0.0, 1.0) * (self.max - self.min);
        if self.stepped { v.round() } else { v }
    }

    fn normalized(&self, plain: f64) -> f64 {
        let span = self.max - self.min;
        if span > 0.0 && plain.is_finite() { ((plain - self.min) / span).clamp(0.0, 1.0) } else { 0.0 }
    }
}

/// The range of `id` (`None` for a parameter the plugin didn't list). No allocation.
fn range(ranges: &[Range], id: u32) -> Option<&Range> {
    ranges.binary_search_by_key(&id, |r| r.id).ok().map(|i| &ranges[i])
}

/// `normalized` in `id`'s own units; an unlisted parameter keeps 0 to 1.
fn plain_of(ranges: &[Range], id: u32, normalized: f64) -> f64 {
    range(ranges, id).map_or(normalized.clamp(0.0, 1.0), |r| r.plain(normalized))
}

fn normalized_of(ranges: &[Range], id: u32, plain: f64) -> f64 {
    range(ranges, id).map_or(if plain.is_finite() { plain.clamp(0.0, 1.0) } else { 0.0 }, |r| r.normalized(plain))
}

fn thrown(during: &'static str, e: Exception) -> PluginError {
    PluginError::Exception { during, message: e.0 }
}

/// The state both halves share.
pub(crate) struct Instance {
    pub info: PluginInfo,
    /// `None` once released or leaked. Only the controller's thread replaces it, and only when no
    /// `process` call can be running (see `release_objects`).
    objects: UnsafeCell<Option<Objects>>,
    /// Kept (leaked) with the plugin when the plugin is leaked: its threads may still call it.
    pub host: ManuallyDrop<Host>,
    host_leaked: AtomicBool,
    failed: AtomicBool,
    error: Mutex<Option<String>>,
    rt_error: RtMessage,
    dead: AtomicBool,
    in_process: AtomicU32,
    generation: AtomicU64,
    active: AtomicBool,
    /// `start_processing` ran (on the audio thread) since the last activation.
    processing: AtomicBool,
    /// Inside `activate`, where `latency.changed` is expected.
    activating: AtomicBool,
    /// Latency at the last activation.
    latency: AtomicU32,
    // Last: the module keeps the plugin's code mapped until everything above is gone.
    module: Arc<Module>,
}

// SAFETY: the plugin is called under CLAP's threading rules, kept by the two halves: the
// controller (one thread at a time, the main thread by contract) and the processor (the audio
// thread). `objects` is only replaced by the controller's thread after the processor is shut out
// (`dead` plus the `in_process` handshake). Everything else shared is atomics, mutexes or the
// host, whose state is atomics, a lock-free queue and a main-thread-only mutex.
unsafe impl Send for Instance {}
// SAFETY: as above.
unsafe impl Sync for Instance {}

impl Instance {
    /// Creates and initialises plugin `info.class_id` from `module`.
    pub fn create(module: &Arc<Module>, info: &PluginInfo) -> Result<Arc<Instance>, PluginError> {
        let host = Host::new(&info.name, MAX_PARAMS as usize);
        let plugin = match module.create(host.as_ptr(), &info.class_id) {
            Ok(p) => p,
            Err(e) => {
                // A factory that threw may have kept the host: keep it too.
                if matches!(e, PluginError::Exception { .. }) {
                    std::mem::forget(host);
                }
                return Err(e);
            }
        };
        // SAFETY: a fresh plugin from the factory.
        let vt = unsafe { *plugin };
        // SAFETY: `init` once, right after creation, on the thread that made it (the main thread).
        match unsafe { shim::b_p(vt.init, plugin) } {
            Some(Ok(true)) => {}
            Some(Err(e)) => {
                // It threw: never touch it again, not even to destroy it.
                module.mark_leaked();
                std::mem::forget(host);
                return Err(thrown("clap_plugin.init", e));
            }
            Some(Ok(false)) | None => {
                // SAFETY: created, never activated; destroyed once.
                if let Some(Err(_)) = unsafe { shim::v_p(vt.destroy, plugin) } {
                    module.mark_leaked();
                    std::mem::forget(host);
                }
                return Err(PluginError::Failed("the plugin failed to initialise (clap_plugin.init)".into()));
            }
        }
        let query = |id: &CStr| -> *const c_void {
            // SAFETY: an initialised plugin; `id` is a static NUL-terminated string. One that
            // throws here just has no such extension.
            match unsafe { shim::p_pp(vt.get_extension, plugin, id.as_ptr()) } {
                Some(Ok(p)) => p,
                _ => std::ptr::null(),
            }
        };
        let ports_activation = {
            let p = query(CLAP_EXT_AUDIO_PORTS_ACTIVATION);
            if p.is_null() { query(CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT) } else { p }
        };
        let ext = Exts {
            params: query(CLAP_EXT_PARAMS).cast(),
            state: query(CLAP_EXT_STATE).cast(),
            latency: query(CLAP_EXT_LATENCY).cast(),
            tail: query(CLAP_EXT_TAIL).cast(),
            audio_ports: query(CLAP_EXT_AUDIO_PORTS).cast(),
            ports_activation: ports_activation.cast(),
            note_ports: query(CLAP_EXT_NOTE_PORTS).cast(),
            gui: query(CLAP_EXT_GUI).cast(),
            timer: query(CLAP_EXT_TIMER_SUPPORT).cast(),
            render: query(CLAP_EXT_RENDER).cast(),
        };
        Ok(Arc::new(Instance {
            info: info.clone(),
            objects: UnsafeCell::new(Some(Objects { plugin, ext })),
            host: ManuallyDrop::new(host),
            host_leaked: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            error: Mutex::new(None),
            rt_error: RtMessage::new(),
            dead: AtomicBool::new(false),
            in_process: AtomicU32::new(0),
            generation: AtomicU64::new(0),
            active: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            activating: AtomicBool::new(false),
            latency: AtomicU32::new(0),
            module: module.clone(),
        }))
    }

    /// The plugin's objects, while they exist.
    pub fn objs(&self) -> Option<Objects> {
        // SAFETY: only `release_objects` replaces the cell, from the controller's thread, when no
        // other use of its contents can be running (see `Instance`).
        unsafe { *self.objects.get() }
    }

    /// The objects, unless the plugin failed.
    pub fn live(&self) -> Option<Objects> {
        if self.is_failed() { None } else { self.objs() }
    }

    /// Switches the plugin off after it threw while being set up or doing work: records why.
    /// Main thread.
    pub fn fail(&self, during: &'static str, message: &str) {
        log::error!("clap {}: {during}: {message}", self.info.name);
        self.error.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(|| format!("{during}: {message}"));
        self.failed.store(true, Ordering::Release);
    }

    /// Switches the plugin off from the audio thread: no allocation, lock or log; the message
    /// waits for `take_error`. `why` is `None` when it threw (the message is the exception).
    fn fail_rt(&self, during: &'static str, why: Option<&'static str>) {
        match why {
            Some(w) => self.rt_error.write_str(during, w),
            None => self.rt_error.write(during),
        }
        self.failed.store(true, Ordering::Release);
    }

    /// Gives up on the plugin without calling it again (it hung, or its editor could not be
    /// closed properly): it is switched off, and it and its module are leaked. Main thread.
    pub fn abandon(&self, why: &str) {
        log::error!("clap {}: {why}; leaking it", self.info.name);
        self.error.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(|| why.to_string());
        self.failed.store(true, Ordering::Release);
        self.module.mark_leaked();
    }

    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub fn take_error(&self) -> Option<String> {
        if let Some(m) = self.rt_error.take() {
            log::error!("clap {}: {m}", self.info.name);
            return Some(m);
        }
        self.error.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    fn poisoned(&self) -> PluginError {
        PluginError::Poisoned(self.error.lock().unwrap_or_else(PoisonError::into_inner).clone().unwrap_or_else(|| "it threw while processing".into()))
    }

    /// A guarded call that sets the plugin up or does work: an exception becomes an error and
    /// switches the plugin off, because its state is no longer known. Main thread.
    pub fn guard<T>(&self, during: &'static str, r: shim::Call<T>) -> Result<Option<T>, PluginError> {
        match r {
            None => Ok(None),
            Some(Ok(v)) => Ok(Some(v)),
            Some(Err(e)) => {
                self.fail(during, &e.0);
                Err(thrown(during, e))
            }
        }
    }

    /// A guarded call that only asks something (a parameter's text, its state): an exception is
    /// remembered for `take_error`, but the plugin keeps running. Main thread.
    pub fn soft<T>(&self, during: &'static str, r: shim::Call<T>) -> Result<Option<T>, PluginError> {
        match r {
            None => Ok(None),
            Some(Ok(v)) => Ok(Some(v)),
            Some(Err(e)) => {
                self.note_exception(during, &e.0);
                Err(thrown(during, e))
            }
        }
    }

    /// Records that a call threw, without switching the plugin off. Main thread.
    pub fn note_exception(&self, during: &'static str, message: &str) {
        log::warn!("clap {}: {during}: {message}", self.info.name);
        self.error.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(|| format!("{during}: {message}"));
    }

    // ---- ports ----------------------------------------------------------------------------

    fn ports(&self, o: &Objects, input: bool) -> Vec<BusInfo> {
        let Some(ap) = o.audio_ports() else { return Vec::new() };
        // SAFETY: a live plugin and its own extension; main thread.
        let n = self.soft("clap_plugin_audio_ports.count", unsafe { shim::u_pz(ap.count, o.plugin, input) }).ok().flatten().unwrap_or(0).min(MAX_PORTS);
        (0..n)
            .filter_map(|i| {
                // SAFETY: plain data; all-zero is valid (null pointers, zero numbers).
                let mut p: clap_audio_port_info = unsafe { std::mem::zeroed() };
                // SAFETY: index below the count; `p` is writable.
                let got = self.soft("clap_plugin_audio_ports.get", unsafe { shim::b_puzp(ap.get, o.plugin, i, input, &mut p as *mut clap_audio_port_info) });
                (got == Ok(Some(true))).then(|| BusInfo {
                    name: read_chars(&p.name),
                    channels: p.channel_count.min(MAX_PORT_CHANNELS),
                    kind: if p.flags & CLAP_AUDIO_PORT_IS_MAIN != 0 { BusKind::Main } else { BusKind::Aux },
                    // CLAP ports start active.
                    active: true,
                })
            })
            .collect()
    }

    fn note_port_count(&self, o: &Objects, input: bool) -> u32 {
        let Some(np) = o.note_ports() else { return 0 };
        // SAFETY: a live plugin and its own extension; main thread.
        self.soft("clap_plugin_note_ports.count", unsafe { shim::u_pz(np.count, o.plugin, input) }).ok().flatten().unwrap_or(0).min(MAX_PORTS)
    }

    /// The ports as the plugin reports them now. Main thread.
    pub fn read_layout(&self) -> BusLayout {
        let Some(o) = self.live() else { return BusLayout::default() };
        BusLayout { audio_inputs: self.ports(&o, true), audio_outputs: self.ports(&o, false), event_inputs: self.note_port_count(&o, true), event_outputs: self.note_port_count(&o, false) }
    }

    /// How the first note input takes notes. Main thread.
    fn dialects(&self, o: &Objects) -> Dialects {
        let Some(np) = o.note_ports() else { return Dialects::default() };
        if self.note_port_count(o, true) == 0 {
            return Dialects::default();
        }
        // SAFETY: plain data; all-zero is valid.
        let mut p: clap_note_port_info = unsafe { std::mem::zeroed() };
        // SAFETY: a live plugin; index 0 is below the count; `p` is writable.
        let got = self.soft("clap_plugin_note_ports.get", unsafe { shim::b_puzp(np.get, o.plugin, 0, true, &mut p as *mut clap_note_port_info) });
        if got != Ok(Some(true)) {
            return Dialects::default();
        }
        Dialects { clap: p.supported_dialects & CLAP_NOTE_DIALECT_CLAP != 0, midi: p.supported_dialects & CLAP_NOTE_DIALECT_MIDI != 0 }
    }

    // ---- activation -----------------------------------------------------------------------

    /// Waits for every running `process` call to return. `false` if one is still running after
    /// [`PROCESS_WAIT`].
    fn wait_for_process(&self) -> bool {
        let t = Instant::now();
        let mut spins = 0u32;
        while self.in_process.load(Ordering::SeqCst) != 0 {
            if t.elapsed() >= PROCESS_WAIT {
                return false;
            }
            if spins < 64 {
                spins += 1;
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        true
    }

    /// Makes the current processor stale and waits for a block it may be running, then stops
    /// processing and deactivates (when needed). A block that never ends leaves the plugin
    /// switched off and leaked. Main thread.
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
        let vt = o.vt();
        if was_processing {
            // CLAP stops processing on the audio thread. No block runs now and the processor is
            // stale, so this thread stands in for it (and says so to the plugin's thread checks).
            let shared = &self.host.shared;
            shared.set_audio_thread();
            // SAFETY: a live, processing plugin; no `process` call is running or can start.
            let r = unsafe { shim::v_p(vt.stop_processing, o.plugin) };
            shared.clear_audio_thread();
            if self.guard("clap_plugin.stop_processing", r).is_err() {
                return;
            }
        }
        if was_active {
            // SAFETY: a live, active plugin, not processing; main thread.
            let _ = self.guard("clap_plugin.deactivate", unsafe { shim::v_p(vt.deactivate, o.plugin) });
        }
    }

    /// Switches the chosen ports on, sets the render mode, activates the plugin and reads its
    /// latency. Returns the layout in force, the new generation number, the latency and how the
    /// plugin takes notes. Main thread.
    pub fn start(&self, setup: &ProcessSetup) -> Result<(BusLayout, u64, u32, Dialects), PluginError> {
        if self.is_failed() {
            return Err(PluginError::Poisoned(self.take_error().unwrap_or_default()));
        }
        self.stop();
        if self.is_failed() {
            return Err(PluginError::Poisoned(self.take_error().unwrap_or_default()));
        }
        let o = self.objs().ok_or_else(|| PluginError::Failed("the plugin is gone".into()))?;
        let mut layout = self.read_layout();
        let main_index = |buses: &[BusInfo]| buses.iter().position(|b| b.kind == BusKind::Main).unwrap_or(0);
        for (input, buses, sel) in [(true, &mut layout.audio_inputs, &setup.inputs), (false, &mut layout.audio_outputs, &setup.outputs)] {
            let main = main_index(buses);
            for (i, b) in buses.iter_mut().enumerate() {
                b.active = sel.includes(i, main);
            }
            // The plugin may skip work for ports that are off; they get silent buffers either way.
            if let Some(pa) = o.ports_activation() {
                for (i, b) in buses.iter().enumerate() {
                    // SAFETY: a live, inactive plugin; index below the port count; main thread.
                    self.guard("clap_plugin_audio_ports_activation.set_active", unsafe { shim::b_pzuzu(pa.set_active, o.plugin, input, i as u32, b.active, 32) })?;
                }
            }
        }
        let dialects = self.dialects(&o);
        if let Some(r) = o.render() {
            let mode = if setup.realtime { CLAP_RENDER_REALTIME } else { CLAP_RENDER_OFFLINE };
            // SAFETY: a live, inactive plugin; main thread.
            self.soft("clap_plugin_render.set", unsafe { shim::b_pi(r.set, o.plugin, mode) }).ok();
        }
        let max = setup.max_block_size.max(1);
        self.activating.store(true, Ordering::Release);
        // SAFETY: a live, inactive plugin; main thread. Blocks run 1 to `max` frames (a zero-frame
        // block is a parameter flush, not a `process` call).
        let r = unsafe { shim::b_pduu(o.vt().activate, o.plugin, setup.sample_rate, 1, max) };
        let r = self.guard("clap_plugin.activate", r);
        self.activating.store(false, Ordering::Release);
        match r? {
            Some(true) => {}
            _ => return Err(PluginError::Failed(format!("the plugin wouldn't start at {} Hz", setup.sample_rate))),
        }
        self.active.store(true, Ordering::Release);
        let latency = match o.latency() {
            // SAFETY: a live, active plugin; main thread.
            Some(l) => self.soft("clap_plugin_latency.get", unsafe { shim::u_p(l.get, o.plugin) }).ok().flatten().unwrap_or(0),
            None => 0,
        };
        self.latency.store(latency, Ordering::Release);
        // What it said while activating is in the latency just read.
        self.host.shared.latency_changed.store(false, Ordering::Release);
        // `stop` above retired the previous processor; the new one belongs to this generation.
        Ok((layout, self.generation.load(Ordering::SeqCst), latency, dialects))
    }

    // ---- shutdown -------------------------------------------------------------------------

    /// Stops the plugin for good: waits for a running `process`, then stops and deactivates it.
    /// A plugin that failed, or whose block never returns, is not called. Safe to call twice.
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
    }

    /// Destroys the plugin (or leaks it, and its host, if it failed), running its destructors on
    /// the calling thread. Only after `shutdown`, which guarantees no `process` call is running
    /// or can start; the controller's drop calls it, so the plugin goes on the main thread even
    /// when the processor outlives the controller.
    fn release_objects(&self) {
        debug_assert!(self.dead.load(Ordering::SeqCst));
        if self.in_process.load(Ordering::SeqCst) != 0 {
            // `shutdown` gave up on a hung block and leaked the plugin.
            self.host_leaked.store(true, Ordering::Release);
            return;
        }
        // SAFETY: shut down with no `process` running, and `process` checks `dead` before it
        // touches the objects; the controller (the only other user) is the caller.
        let objects = unsafe { (*self.objects.get()).take() };
        let Some(o) = objects else { return };
        if self.is_failed() {
            self.module.mark_leaked();
            self.host_leaked.store(true, Ordering::Release);
            return;
        }
        // SAFETY: a live, deactivated plugin; destroyed once (the objects were taken out above).
        if let Some(Err(e)) = unsafe { shim::v_p(o.vt().destroy, o.plugin) } {
            self.note_exception("clap_plugin.destroy", &e.0);
            self.module.mark_leaked();
            self.host_leaked.store(true, Ordering::Release);
        }
    }

    // ---- the main thread's errands ---------------------------------------------------------

    /// Runs what the plugin asked the main thread for since the last call: `on_main_thread` and
    /// due timers. Main thread (the controller's).
    fn serve(&self) {
        let shared = &self.host.shared;
        if !shared.is_main_thread() {
            return;
        }
        let Some(o) = self.live() else { return };
        if shared.callback.swap(false, Ordering::AcqRel) {
            // SAFETY: a live plugin; main thread.
            if self.guard("clap_plugin.on_main_thread", unsafe { shim::v_p(o.vt().on_main_thread, o.plugin) }).is_err() {
                return;
            }
        }
        let Some(t) = o.timer() else { return };
        for id in shared.due_timers(Instant::now()) {
            // A timer callback may have taken another timer away.
            if shared.has_timer(id) {
                // SAFETY: a live plugin; main thread; a timer it registered.
                if self.guard("clap_plugin_timer_support.on_timer", unsafe { shim::v_pu(t.on_timer, o.plugin, id) }).is_err() {
                    return;
                }
            }
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Normally the controller already did both.
        self.shutdown();
        self.release_objects();
        if !self.host_leaked.load(Ordering::Acquire) {
            // SAFETY: the plugin is destroyed (or was never there), so nothing can call the host;
            // dropped once, here.
            unsafe { ManuallyDrop::drop(&mut self.host) };
        }
    }
}

// ---- state streams ---------------------------------------------------------------------------

/// Where `state.save` writes.
struct Writer {
    data: Vec<u8>,
    too_big: bool,
}

unsafe extern "C" fn stream_write(stream: *const clap_ostream, buffer: *const c_void, size: u64) -> i64 {
    // SAFETY: `ctx` is the `Writer` on `save_state`'s frame, alive for the call.
    let Some(w) = (unsafe { stream.as_ref().and_then(|s| (s.ctx as *mut Writer).as_mut()) }) else { return -1 };
    let Ok(n) = usize::try_from(size) else { return -1 };
    if n == 0 {
        return 0;
    }
    if buffer.is_null() {
        return -1;
    }
    if w.data.len().saturating_add(n) > MAX_STATE_BYTES {
        w.too_big = true;
        return -1;
    }
    // SAFETY: the plugin provides `size` readable bytes at `buffer` for the call.
    w.data.extend_from_slice(unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n) });
    i64::try_from(n).unwrap_or(-1)
}

/// Where `state.load` reads from.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

unsafe extern "C" fn stream_read(stream: *const clap_istream, buffer: *mut c_void, size: u64) -> i64 {
    // SAFETY: `ctx` is the `Reader` on `load_state`'s frame, alive for the call.
    let Some(r) = (unsafe { stream.as_ref().and_then(|s| (s.ctx as *mut Reader<'_>).as_mut()) }) else { return -1 };
    let rest = r.data.get(r.at..).unwrap_or(&[]);
    let n = rest.len().min(usize::try_from(size).unwrap_or(usize::MAX));
    if n == 0 {
        return 0;
    }
    if buffer.is_null() {
        return -1;
    }
    // SAFETY: the plugin provides `size` (>= `n`) writable bytes at `buffer`; `rest` is ours, so
    // they cannot overlap.
    unsafe { std::ptr::copy_nonoverlapping(rest.as_ptr(), buffer.cast::<u8>(), n) };
    r.at += n;
    i64::try_from(n).unwrap_or(-1)
}

// ---- the controller (main thread) ----------------------------------------------------------

/// The main-thread half of a hosted CLAP plugin.
pub struct ClapController {
    inst: Arc<Instance>,
    info: PluginInfo,
    default_layout: BusLayout,
    params: Vec<ParamInfo>,
    ranges: Vec<Range>,
    editor: Option<Editor>,
    failure_reported: bool,
    /// Losses already logged: controller events, mailbox writes refused, audio-thread log lines.
    logged_losses: (u32, u32, u32),
}

impl ClapController {
    pub(crate) fn new(inst: Arc<Instance>) -> ClapController {
        let mut c = ClapController {
            info: inst.info.clone(),
            default_layout: inst.read_layout(),
            params: Vec::new(),
            ranges: Vec::new(),
            editor: None,
            failure_reported: false,
            logged_losses: (0, 0, 0),
            inst,
        };
        c.refresh_params();
        c
    }

    fn check_alive(&self) -> Result<Objects, PluginError> {
        if self.inst.is_failed() {
            return Err(self.inst.poisoned());
        }
        self.inst.objs().ok_or_else(|| PluginError::Failed("the plugin is gone".into()))
    }

    /// Hands values waiting in the mailbox to an inactive plugin through `params.flush`, on the
    /// main thread (an active one gets them in its next block), and reports what it says back.
    fn flush_now(&mut self) {
        let shared = &self.inst.host.shared;
        if self.inst.is_active() || !shared.is_main_thread() {
            return;
        }
        let Some(o) = self.inst.live() else { return };
        let Some(p) = o.params() else { return };
        if p.flush.is_none() {
            return;
        }
        let mut ins = Box::new(InEvents::new());
        let ranges = &self.ranges;
        shared.mailbox.drain(|id, v| ins.can_take(id) && ins.add_param(id, 0, plain_of(ranges, id, v)));
        ins.seal();
        let mut outs = Box::new(OutEvents::new());
        let (i, out) = (ins.as_clap(), outs.as_clap());
        // SAFETY: a live, inactive plugin; main thread; both lists outlive the call.
        if self.inst.guard("clap_plugin_params.flush", unsafe { shim::v_ppp(p.flush, o.plugin, &i as *const _, &out as *const _) }).is_err() {
            return;
        }
        report_out(&self.inst, &self.ranges, &outs, None);
    }

    /// The parameter's value now, in its own units, as the plugin's main thread sees it.
    fn plugin_value(&self, id: ParamId) -> Option<f64> {
        let o = self.inst.live()?;
        let p = o.params()?;
        let mut v = 0.0f64;
        // SAFETY: a live plugin; main thread; `v` is writable.
        let got = self.inst.soft("clap_plugin_params.get_value", unsafe { shim::b_pup(p.get_value, o.plugin, id, &mut v as *mut f64) }).ok().flatten();
        (got == Some(true) && v.is_finite()).then_some(v)
    }
}

/// Reports what the plugin handed back (edits in its window) to the controller's queue and, from
/// the audio thread, into the block's outputs.
fn report_out(inst: &Instance, ranges: &[Range], outs: &OutEvents, mut block: Option<&mut ProcessBlock<'_>>) {
    let shared = &inst.host.shared;
    for e in outs.items() {
        let h = e.header();
        match h.type_ {
            CLAP_EVENT_PARAM_VALUE => {
                // SAFETY: the type says it is a parameter value (its size was checked when kept).
                let p = unsafe { e.param };
                if !p.value.is_finite() {
                    continue;
                }
                let value = normalized_of(ranges, p.param_id, p.value);
                if let Some(b) = block.as_deref_mut() {
                    b.param_out.push(ParamChange { id: p.param_id, offset: h.time, value });
                }
                shared.report(ControllerEvent::ParamEdit { id: p.param_id, value });
            }
            // SAFETY (both): the type says it is a gesture.
            CLAP_EVENT_PARAM_GESTURE_BEGIN => shared.report(ControllerEvent::EditBegin { id: unsafe { e.gesture }.param_id }),
            CLAP_EVENT_PARAM_GESTURE_END => shared.report(ControllerEvent::EditEnd { id: unsafe { e.gesture }.param_id }),
            _ => {
                if let (Some(b), Some(ev)) = (block.as_deref_mut(), events::from_clap(e)) {
                    b.midi_out.push(ev);
                }
            }
        }
    }
}

impl PluginController for ClapController {
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
        let has_flush = self.inst.live().and_then(|o| o.params()).is_some_and(|p| p.flush.is_some());
        let (layout, generation, latency, dialects) = self.inst.start(setup)?;
        Ok(Box::new(ClapProcessor::new(self.inst.clone(), generation, layout, setup, latency, dialects, has_flush, self.ranges.clone())))
    }

    fn deactivate(&mut self) {
        // The running processor goes stale (and any block in progress finishes) first. Values
        // set meanwhile reach the plugin through `params.flush`.
        self.inst.stop();
    }

    fn latency_samples(&mut self) -> u32 {
        let Some(o) = self.inst.live() else { return 0 };
        // CLAP asks for the latency only while active.
        match (self.inst.is_active(), o.latency()) {
            // SAFETY: a live, active plugin; main thread.
            (true, Some(l)) => self.inst.soft("clap_plugin_latency.get", unsafe { shim::u_p(l.get, o.plugin) }).ok().flatten().unwrap_or(0),
            _ => self.inst.latency.load(Ordering::Acquire),
        }
    }

    fn tail_samples(&mut self) -> u32 {
        let Some(o) = self.inst.live() else { return 0 };
        let Some(t) = o.tail() else { return 0 };
        // SAFETY: a live plugin; main thread.
        let n = self.inst.soft("clap_plugin_tail.get", unsafe { shim::u_p(t.get, o.plugin) }).ok().flatten().unwrap_or(0);
        // CLAP: INT32_MAX and up is a tail that never ends.
        if n >= i32::MAX as u32 { u32::MAX } else { n }
    }

    fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    fn refresh_params(&mut self) {
        self.params.clear();
        self.ranges.clear();
        let inst = Arc::clone(&self.inst);
        let Some(o) = inst.live() else { return };
        let Some(p) = o.params() else { return };
        // SAFETY: a live plugin; main thread.
        let n = inst.soft("clap_plugin_params.count", unsafe { shim::u_p(p.count, o.plugin) }).ok().flatten().unwrap_or(0).min(MAX_PARAMS);
        for i in 0..n {
            // SAFETY: plain data; all-zero is valid.
            let mut info: clap_param_info = unsafe { std::mem::zeroed() };
            // SAFETY: index below the count; `info` is writable.
            if inst.soft("clap_plugin_params.get_info", unsafe { shim::b_pup(p.get_info, o.plugin, i, &mut info as *mut clap_param_info) }) != Ok(Some(true)) {
                continue;
            }
            if self.ranges.iter().any(|r| r.id == info.id) {
                continue;
            }
            let (mut min, mut max) = (info.min_value, info.max_value);
            if !(min.is_finite() && max.is_finite()) {
                (min, max) = (0.0, 1.0);
            }
            if max < min {
                std::mem::swap(&mut min, &mut max);
            }
            let f = info.flags;
            let stepped = f & CLAP_PARAM_IS_STEPPED != 0;
            let r = Range { id: info.id, min, max, stepped };
            let name = read_chars(&info.name);
            self.params.push(ParamInfo {
                id: info.id,
                short_name: name.clone(),
                name,
                // CLAP parameters carry no unit; their text has it.
                units: String::new(),
                min,
                max,
                default: r.normalized(info.default_value),
                step_count: if stepped { (max - min).round().clamp(0.0, f64::from(u32::MAX)) as u32 } else { 0 },
                automatable: f & CLAP_PARAM_IS_AUTOMATABLE != 0,
                hidden: f & CLAP_PARAM_IS_HIDDEN != 0,
                read_only: f & CLAP_PARAM_IS_READONLY != 0,
                is_bypass: f & CLAP_PARAM_IS_BYPASS != 0,
            });
            self.ranges.push(r);
        }
        self.ranges.sort_by_key(|r| r.id);
    }

    fn param_value(&mut self, id: ParamId) -> Option<f64> {
        // A value set from the UI that the plugin hasn't had yet is the one it is about to have.
        if let Some(v) = self.inst.host.shared.mailbox.peek(id) {
            return Some(v);
        }
        self.plugin_value(id).map(|v| normalized_of(&self.ranges, id, v))
    }

    fn set_param(&mut self, id: ParamId, normalized: f64) {
        if !normalized.is_finite() || self.inst.is_failed() {
            return;
        }
        self.inst.host.shared.mailbox.post(id, normalized.clamp(0.0, 1.0));
        self.flush_now();
    }

    fn param_text(&mut self, id: ParamId, normalized: f64) -> Option<String> {
        let o = self.inst.live()?;
        let p = o.params()?;
        let mut buf = [0 as c_char; TEXT_BYTES];
        let plain = plain_of(&self.ranges, id, normalized);
        // SAFETY: a live plugin; main thread; `buf` is writable for its length.
        let r = self.inst.soft("clap_plugin_params.value_to_text", unsafe { shim::b_pudpu(p.value_to_text, o.plugin, id, plain, buf.as_mut_ptr(), TEXT_BYTES as u32) }).ok().flatten();
        buf[TEXT_BYTES - 1] = 0;
        (r == Some(true)).then(|| read_chars(&buf)).filter(|t| !t.is_empty())
    }

    fn param_plain(&mut self, id: ParamId, normalized: f64) -> Option<f64> {
        range(&self.ranges, id).map(|r| r.plain(normalized))
    }

    fn save_state(&mut self) -> Result<Vec<u8>, PluginError> {
        let o = self.check_alive()?;
        // A plugin without the state extension has no state to keep.
        let Some(s) = o.state() else { return Ok(Vec::new()) };
        let mut w = Writer { data: Vec::new(), too_big: false };
        let stream = clap_ostream { ctx: (&mut w as *mut Writer).cast(), write: Some(stream_write) };
        // SAFETY: a live plugin; main thread; the stream and its writer live on this frame.
        let r = self.inst.soft("clap_plugin_state.save", unsafe { shim::b_pp(s.save, o.plugin, &stream as *const clap_ostream) })?;
        if w.too_big {
            return Err(PluginError::Failed("the plugin's state is too large".into()));
        }
        match r {
            Some(true) => Ok(w.data),
            _ => Err(PluginError::Failed("the plugin wouldn't give its state".into())),
        }
    }

    fn has_pending_params(&self) -> bool {
        self.inst.host.shared.mailbox.pending()
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), PluginError> {
        let o = self.check_alive()?;
        // Values set before the load must not reach the plugin after it, over the loaded state.
        self.inst.host.shared.mailbox.drain(|_, _| true);
        let Some(s) = o.state() else {
            return if data.is_empty() { Ok(()) } else { Err(PluginError::Unsupported("this plugin keeps no state".into())) };
        };
        if data.is_empty() || data.len() > MAX_STATE_BYTES {
            return Err(PluginError::Failed("not a plugin state".into()));
        }
        let mut r = Reader { data, at: 0 };
        let stream = clap_istream { ctx: (&mut r as *mut Reader<'_>).cast(), read: Some(stream_read) };
        // SAFETY: a live plugin; main thread; the stream and its reader live on this frame.
        match self.inst.soft("clap_plugin_state.load", unsafe { shim::b_pp(s.load, o.plugin, &stream as *const clap_istream) })? {
            Some(true) => Ok(()),
            _ => Err(PluginError::Failed("the plugin refused the state".into())),
        }
    }

    fn poll(&mut self, out: &mut Vec<ControllerEvent>) {
        self.inst.serve();
        let shared = &self.inst.host.shared;
        if shared.flush.swap(false, Ordering::AcqRel) {
            // An active plugin hands its edits back in its next block.
            self.flush_now();
        }
        let size = self.inst.host.shared.resize.swap(0, Ordering::AcqRel);
        if size != 0
            && let Some(e) = &mut self.editor
        {
            e.requested(EditorSize { width: (size >> 32) as u32, height: size as u32 });
        }
        let rescan = self.inst.host.shared.rescan.swap(0, Ordering::AcqRel);
        if rescan & CLAP_PARAM_RESCAN_INFO != 0 {
            // Names and flags changed (ranges only change with a full rescan, while it is off):
            // the list the app reads on `ParamsChanged` is read again here.
            self.refresh_params();
        }
        let shared = &self.inst.host.shared;
        shared.take_events(out);
        let mut push = |e: ControllerEvent| {
            if !out.contains(&e) {
                out.push(e);
            }
        };
        let (restart, ports) = (shared.restart.swap(false, Ordering::AcqRel), shared.ports_changed.swap(false, Ordering::AcqRel));
        if restart || ports || rescan & CLAP_PARAM_RESCAN_ALL != 0 {
            push(ControllerEvent::ReloadRequested);
        }
        let dirty = shared.dirty.swap(false, Ordering::AcqRel);
        if dirty || rescan & (CLAP_PARAM_RESCAN_VALUES | CLAP_PARAM_RESCAN_TEXT | CLAP_PARAM_RESCAN_INFO) != 0 {
            push(ControllerEvent::ParamsChanged);
        }
        if !self.inst.activating.load(Ordering::Acquire) && shared.latency_changed.swap(false, Ordering::AcqRel) {
            push(ControllerEvent::LatencyChanged);
        }
        let losses = (shared.events_dropped(), shared.mailbox.refused(), shared.logs_dropped.load(Ordering::Relaxed));
        if losses != self.logged_losses {
            log::warn!(
                "clap {}: {} editor event(s) dropped, {} parameter value(s) refused and {} audio-thread log line(s) dropped so far",
                self.info.name,
                losses.0,
                losses.1,
                losses.2
            );
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
        Editor::supported(&self.inst)
    }

    fn open_editor(&mut self, parent: NativeParent, on_resize: ResizeCallback) -> Result<EditorSize, PluginError> {
        self.check_alive()?;
        if self.editor.is_some() {
            self.close_editor();
        }
        // A size it asked for before this editor existed is not this editor's.
        self.inst.host.shared.resize.store(0, Ordering::Release);
        let (editor, size) = Editor::open(&self.inst, parent, on_resize)?;
        self.editor = Some(editor);
        Ok(size)
    }

    fn close_editor(&mut self) {
        if let Some(e) = self.editor.take()
            && e.close(&self.inst)
        {
            // Its window could not be closed properly (off the main thread, or the plugin had
            // failed): the plugin may still draw into it, so it must not be destroyed either.
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

impl Drop for ClapController {
    fn drop(&mut self) {
        // The editor goes before the plugin it belongs to.
        self.close_editor();
        self.inst.shutdown();
        // The plugin is destroyed here, on the controller's thread, even when the audio thread
        // still holds the processor.
        self.inst.release_objects();
    }
}

// ---- the processor (audio thread) ----------------------------------------------------------

/// Where one active output port's channels sit, for filling channels the plugin marked constant.
struct OutPort {
    port: usize,
    first: usize,
    channels: usize,
}

/// A mask with the low `n` bits set (every channel of an `n`-channel port).
fn all_channels(n: usize) -> u64 {
    if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }
}

/// The audio-thread half of a hosted CLAP plugin.
///
/// Everything `process` touches is allocated in `new`: the port tables, silent buffers for ports
/// that are off, copies for inputs that are also outputs, and the event lists.
pub struct ClapProcessor {
    inst: Arc<Instance>,
    generation: u64,
    layout: BusLayout,
    max_block: usize,
    sample_rate: f64,
    latency: u32,
    dialects: Dialects,
    has_flush: bool,
    ranges: Vec<Range>,
    events: Box<InEvents>,
    out: Box<OutEvents>,
    in_bufs: Vec<clap_audio_buffer>,
    out_bufs: Vec<clap_audio_buffer>,
    /// Channel pointers for every port, in port order: the active ports' are rewritten every
    /// block, the others point into `silent_in`/`spare_out` for good.
    in_ptrs: Vec<*mut f32>,
    out_ptrs: Vec<*mut f32>,
    /// Where the block's channel `k` goes in `in_ptrs`/`out_ptrs`.
    in_map: Vec<usize>,
    out_map: Vec<usize>,
    out_ports: Vec<OutPort>,
    silent_in: Vec<f32>,
    spare_out: Vec<f32>,
    /// Copies of inputs that are also outputs (CLAP processes in place only on paired ports).
    in_copy: Vec<f32>,
    dropped: Dropped,
}

// SAFETY: the raw pointers point into this struct's own heap buffers or at the caller's buffers
// during a call; it moves to the audio thread as a whole and is used by one thread at a time.
unsafe impl Send for ClapProcessor {}

impl ClapProcessor {
    #[allow(clippy::too_many_arguments)]
    fn new(inst: Arc<Instance>, generation: u64, layout: BusLayout, setup: &ProcessSetup, latency: u32, dialects: Dialects, has_flush: bool, ranges: Vec<Range>) -> ClapProcessor {
        let max = setup.max_block_size as usize;
        let total = |buses: &[BusInfo]| buses.iter().map(|b| b.channels as usize).sum::<usize>();
        let off = |buses: &[BusInfo]| buses.iter().filter(|b| !b.active).map(|b| b.channels as usize).sum::<usize>();
        let mut p = ClapProcessor {
            in_ptrs: vec![std::ptr::null_mut(); total(&layout.audio_inputs)],
            out_ptrs: vec![std::ptr::null_mut(); total(&layout.audio_outputs)],
            silent_in: vec![0.0; off(&layout.audio_inputs) * max],
            spare_out: vec![0.0; off(&layout.audio_outputs) * max],
            in_copy: vec![0.0; layout.input_channels() * max],
            in_bufs: Vec::with_capacity(layout.audio_inputs.len()),
            out_bufs: Vec::with_capacity(layout.audio_outputs.len()),
            in_map: Vec::with_capacity(layout.input_channels()),
            out_map: Vec::with_capacity(layout.output_channels()),
            out_ports: Vec::new(),
            inst,
            generation,
            max_block: max,
            sample_rate: setup.sample_rate,
            latency,
            dialects,
            has_flush,
            ranges,
            events: Box::new(InEvents::new()),
            out: Box::new(OutEvents::new()),
            dropped: Dropped::default(),
            layout,
        };
        for input in [true, false] {
            let (buses, ptrs, spare, bufs, map) = if input {
                (&p.layout.audio_inputs, &mut p.in_ptrs, &mut p.silent_in, &mut p.in_bufs, &mut p.in_map)
            } else {
                (&p.layout.audio_outputs, &mut p.out_ptrs, &mut p.spare_out, &mut p.out_bufs, &mut p.out_map)
            };
            let (mut next, mut spare_at, mut block_ch) = (0usize, 0usize, 0usize);
            for (i, b) in buses.iter().enumerate() {
                let ch = b.channels as usize;
                if b.active {
                    map.extend(next..next + ch);
                    if !input {
                        p.out_ports.push(OutPort { port: i, first: block_ch, channels: ch });
                    }
                    block_ch += ch;
                } else {
                    for c in 0..ch {
                        ptrs[next + c] = spare[spare_at * max..].as_mut_ptr();
                        spare_at += 1;
                    }
                }
                bufs.push(clap_audio_buffer {
                    data32: if ch > 0 { ptrs[next..].as_mut_ptr() } else { std::ptr::null_mut() },
                    data64: std::ptr::null_mut(),
                    channel_count: ch as u32,
                    latency: 0,
                    // A port that is off is silence, and says so.
                    constant_mask: if input && !b.active { all_channels(ch) } else { 0 },
                });
                next += ch;
            }
        }
        p
    }

    /// This block's events: values waiting in the mailbox at its start, the caller's parameter
    /// changes, then its notes and MIDI. Parameters go first at any one time.
    fn gather(&mut self, block: &ProcessBlock<'_>, last: u32) {
        let (inst, events, ranges) = (&self.inst, &mut *self.events, &self.ranges);
        // A zero-frame block delivers parameters through `flush`; without one they keep waiting.
        if block.frames > 0 || self.has_flush {
            // Values that find no room this block stay waiting for the next one.
            inst.host.shared.mailbox.drain(|id, v| events.can_take(id) && events.add_param(id, 0, plain_of(ranges, id, v)));
        }
        for c in block.param_changes {
            events.add_param(c.id, c.offset.min(last), plain_of(ranges, c.id, c.value));
        }
        if block.frames > 0 {
            for e in block.events {
                let e = Event { offset: e.offset.min(last), ..*e };
                if let Some(raw) = events::to_clap(&e, self.dialects) {
                    events.add_event(raw);
                }
            }
        }
        events.seal();
    }

    fn take_counts(&mut self) {
        self.dropped.events = self.dropped.events.saturating_add(std::mem::take(&mut self.events.dropped_events)).saturating_add(std::mem::take(&mut self.out.dropped));
        self.dropped.param_points = self.dropped.param_points.saturating_add(std::mem::take(&mut self.events.dropped_points));
    }

    fn run(&mut self, block: &mut ProcessBlock<'_>) -> Result<(), ProcessError> {
        let frames = block.frames;
        let (n_in, n_out) = (self.layout.input_channels(), self.layout.output_channels());
        if frames > self.max_block
            || block.inputs.channels() != n_in
            || block.outputs.channels() != n_out
            || (frames > 0 && ((n_out > 0 && block.outputs.len() < frames) || (n_in > 0 && block.inputs.len() < frames)))
        {
            block.outputs.silence(frames);
            return Err(ProcessError::BadBlock);
        }
        let inst = Arc::clone(&self.inst);
        inst.host.shared.set_audio_thread();
        self.events.clear();
        self.out.clear();
        // Offsets past the block would send the plugin outside its buffers: they land on its last
        // frame instead.
        self.gather(block, frames.saturating_sub(1) as u32);
        let Some(o) = inst.objs() else {
            block.outputs.silence(frames);
            return Err(ProcessError::Stale);
        };
        let vt = o.vt();
        if frames == 0 {
            // A zero-frame block only delivers parameters: CLAP flushes them.
            if self.events.len() == 0 {
                return Ok(());
            }
            let flush = o.params().and_then(|p| p.flush);
            let (ins, outs) = (self.events.as_clap(), self.out.as_clap());
            // SAFETY: a live, active plugin; the audio thread; no `process` runs; both lists live on
            // this frame and in `self` for the call.
            let r = unsafe { shim::v_ppp_rt(flush, o.plugin, &ins as *const _, &outs as *const _) };
            self.take_counts();
            if let Some(Err(Thrown)) = r {
                inst.fail_rt("clap_plugin_params.flush", None);
                return Err(ProcessError::Failed);
            }
            report_out(&inst, &self.ranges, &self.out, Some(block));
            return Ok(());
        }
        if !inst.processing.load(Ordering::Acquire) {
            // SAFETY: a live, active plugin; the audio thread, before its first block.
            match unsafe { shim::b_p_rt(vt.start_processing, o.plugin) } {
                Some(Ok(true)) | None => inst.processing.store(true, Ordering::Release),
                Some(Ok(false)) => {
                    block.outputs.silence(frames);
                    return Err(ProcessError::Plugin(CLAP_PROCESS_ERROR));
                }
                Some(Err(Thrown)) => {
                    inst.fail_rt("clap_plugin.start_processing", None);
                    block.outputs.silence(frames);
                    return Err(ProcessError::Failed);
                }
            }
        }
        // Point the port tables at the caller's buffers. An input that is also an output is
        // copied first: CLAP plugins may write an output before they read its input.
        let (ins, outs) = (block.inputs.as_ptrs(), block.outputs.as_ptrs());
        for (k, (&slot, &src)) in self.in_map.iter().zip(ins).enumerate() {
            self.in_ptrs[slot] = if outs.iter().any(|&o| o.cast_const() == src) {
                let copy = self.in_copy[k * self.max_block..].as_mut_ptr();
                // SAFETY: `src` is valid for `frames` reads (checked above); the copy has room for
                // `max_block` >= `frames`; they are different allocations.
                unsafe { std::ptr::copy_nonoverlapping(src, copy, frames) };
                copy
            } else {
                src.cast_mut()
            };
        }
        for (&slot, &dst) in self.out_map.iter().zip(outs) {
            self.out_ptrs[slot] = dst;
        }
        // Ports that are off: silent inputs, whatever the plugin did to them last time.
        for ch in self.silent_in.chunks_mut(self.max_block) {
            ch[..frames].fill(0.0);
        }
        for b in &mut self.out_bufs {
            b.constant_mask = 0;
        }
        let transport = events::transport(block.transport, self.sample_rate);
        let (in_events, out_events) = (self.events.as_clap(), self.out.as_clap());
        let process = clap_process {
            steady_time: if block.transport.continuous_samples >= 0 { block.transport.continuous_samples } else { -1 },
            frames_count: frames as u32,
            transport: &transport,
            audio_inputs: if self.in_bufs.is_empty() { std::ptr::null() } else { self.in_bufs.as_ptr() },
            audio_outputs: if self.out_bufs.is_empty() { std::ptr::null_mut() } else { self.out_bufs.as_mut_ptr() },
            audio_inputs_count: self.in_bufs.len() as u32,
            audio_outputs_count: self.out_bufs.len() as u32,
            in_events: &in_events,
            out_events: &out_events,
        };
        // SAFETY: a live, active, processing plugin, on the audio thread. Every channel pointer
        // refers to `frames` (or more) valid f32s: the caller's (borrowed for the whole call) or
        // this processor's own buffers; the port tables, event lists and transport live in `self`
        // or on this frame, untouched until the call returns.
        let r = unsafe { shim::i_pp_rt(vt.process, o.plugin, &process as *const clap_process) };
        self.take_counts();
        match r {
            Some(Ok(status)) if status != CLAP_PROCESS_ERROR => {}
            Some(Ok(status)) => {
                block.outputs.silence(frames);
                return Err(ProcessError::Plugin(status));
            }
            None => {
                block.outputs.silence(frames);
                return Err(ProcessError::Plugin(CLAP_PROCESS_ERROR));
            }
            Some(Err(Thrown)) => {
                // The plugin threw: it is switched off and the block is silent. Nothing here
                // allocates, locks or logs; the controller reports it later.
                inst.fail_rt("clap_plugin.process", None);
                block.outputs.silence(frames);
                return Err(ProcessError::Failed);
            }
        }
        // A channel the plugin marked constant holds its value in the first frame only.
        for port in &self.out_ports {
            let mask = self.out_bufs[port.port].constant_mask;
            if mask == 0 {
                continue;
            }
            for c in 0..port.channels.min(64) {
                if mask & (1 << c) != 0
                    && let Some(&p) = outs.get(port.first + c)
                {
                    // SAFETY: an output channel of `block`, valid for `frames` writes.
                    let ch = unsafe { std::slice::from_raw_parts_mut(p, frames) };
                    let v = ch[0];
                    ch.fill(v);
                }
            }
        }
        report_out(&inst, &self.ranges, &self.out, Some(block));
        Ok(())
    }
}

impl Plugin for ClapProcessor {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_turn_normalized_values_into_the_plugins_own_and_back() {
        let ranges = vec![Range { id: 3, min: 0.0, max: 2.0, stepped: true }, Range { id: 7, min: 20.0, max: 20_000.0, stepped: false }];
        assert_eq!(plain_of(&ranges, 7, 0.0), 20.0);
        assert_eq!(plain_of(&ranges, 7, 1.0), 20_000.0);
        assert_eq!(plain_of(&ranges, 3, 0.3), 1.0, "a stepped parameter lands on a step");
        assert_eq!(normalized_of(&ranges, 3, 1.0), 0.5);
        assert_eq!(normalized_of(&ranges, 7, 1e9), 1.0, "clamped");
        assert_eq!(plain_of(&ranges, 99, 0.25), 0.25, "unlisted: 0 to 1");
        assert_eq!(normalized_of(&ranges, 99, f64::NAN), 0.0);
        let flat = Range { id: 1, min: 5.0, max: 5.0, stepped: false };
        assert_eq!(flat.normalized(5.0), 0.0, "no span, no division by zero");
    }

    #[test]
    fn channel_masks_cover_a_port() {
        assert_eq!(all_channels(0), 0);
        assert_eq!(all_channels(2), 0b11);
        assert_eq!(all_channels(64), u64::MAX);
    }

    #[test]
    fn state_streams_read_and_write_in_pieces_and_refuse_too_much() {
        let mut w = Writer { data: Vec::new(), too_big: false };
        let s = clap_ostream { ctx: (&mut w as *mut Writer).cast(), write: Some(stream_write) };
        // SAFETY: our own callbacks with valid buffers.
        unsafe {
            assert_eq!(stream_write(&s, b"abc".as_ptr().cast(), 3), 3);
            assert_eq!(stream_write(&s, b"de".as_ptr().cast(), 2), 2);
            assert_eq!(stream_write(&s, std::ptr::null(), 0), 0);
            assert_eq!(stream_write(&s, b"x".as_ptr().cast(), MAX_STATE_BYTES as u64), -1);
        }
        assert!(w.too_big);
        assert_eq!(w.data, b"abcde");
        let data = b"hello".to_vec();
        let mut r = Reader { data: &data, at: 0 };
        let s = clap_istream { ctx: (&mut r as *mut Reader<'_>).cast(), read: Some(stream_read) };
        let mut buf = [0u8; 3];
        // SAFETY: as above.
        unsafe {
            assert_eq!(stream_read(&s, buf.as_mut_ptr().cast(), 3), 3);
            assert_eq!(&buf, b"hel");
            assert_eq!(stream_read(&s, buf.as_mut_ptr().cast(), 3), 2);
            assert_eq!(stream_read(&s, buf.as_mut_ptr().cast(), 3), 0, "the end");
        }
    }
}
