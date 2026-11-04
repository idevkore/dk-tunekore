//! The host side of the CLAP conversation: the `clap_host` a plugin is given and the host
//! extensions it may ask for.
//!
//! Derived from soundCraft's `clap-host` (`ffi.rs`: `HostState` and the `host_*` callbacks), MIT
//! OR Apache-2.0; see `third_party/soundcraft/`. What changed: one host per instance whose state is
//! atomics, a lock-free event queue and the parameter mailbox, so a plugin may call it from any
//! thread, the audio thread included, without it allocating, locking or logging there; added
//! thread-check (the main thread is the one that made the instance, the audio thread the one in
//! `process`), timer support, the tail, audio-ports and note-ports host extensions; log messages
//! sent from the audio thread are counted, not written.
//!
//! The plugin's requests (a main-thread callback, a parameter flush, a restart, a resize) are
//! flags here; [`super::instance`] acts on them on the main thread, from `poll`.

use crate::plugin::ControllerEvent;
use crate::rt::os_thread;
use crate::vst3::mailbox::ParamMailbox;
use clap_sys::ext::audio_ports::{clap_host_audio_ports, CLAP_AUDIO_PORTS_RESCAN_NAMES, CLAP_EXT_AUDIO_PORTS};
use clap_sys::ext::gui::{clap_host_gui, CLAP_EXT_GUI};
use clap_sys::ext::latency::{clap_host_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::log::*;
use clap_sys::ext::note_ports::{clap_host_note_ports, clap_note_dialect, CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, CLAP_NOTE_PORTS_RESCAN_ALL};
use clap_sys::ext::params::{clap_host_params, clap_param_clear_flags, clap_param_rescan_flags, CLAP_EXT_PARAMS};
use clap_sys::ext::state::{clap_host_state, CLAP_EXT_STATE};
use clap_sys::ext::tail::{clap_host_tail, CLAP_EXT_TAIL};
use clap_sys::ext::thread_check::{clap_host_thread_check, CLAP_EXT_THREAD_CHECK};
use clap_sys::ext::timer_support::{clap_host_timer_support, CLAP_EXT_TIMER_SUPPORT};
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::version::CLAP_VERSION;
use crossbeam_queue::ArrayQueue;
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Events buffered between two `poll`s.
const EVENT_CAP: usize = 4096;
/// Most timers one plugin may hold.
const MAX_TIMERS: usize = 64;
/// The shortest timer period we keep to (the main thread runs them from `poll`).
const MIN_TIMER_PERIOD: Duration = Duration::from_millis(10);

/// A timer the plugin registered: `on_timer(id)` every `period`, on the main thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timer {
    pub id: clap_id,
    pub period: Duration,
    pub next: Instant,
}

/// What the plugin told the host. Every field is safe to touch from any thread without locking,
/// except `timers`, which only the main thread uses (CLAP's timer calls are main-thread only).
pub(crate) struct HostShared {
    /// The plugin's name, for log lines.
    pub name: String,
    /// The OS thread that made the instance: CLAP's main thread.
    main_thread: usize,
    /// The OS thread processing now or last (0 before the first block).
    audio_thread: AtomicUsize,
    /// Values set from outside the processor (the UI), the latest per parameter, normalized.
    pub mailbox: ParamMailbox,
    /// What the controller reports from `poll` (edits in the plugin's window).
    events: ArrayQueue<ControllerEvent>,
    events_dropped: AtomicU32,
    /// `request_restart`.
    pub restart: AtomicBool,
    /// `request_callback`: run `on_main_thread`.
    pub callback: AtomicBool,
    /// `params.request_flush`.
    pub flush: AtomicBool,
    /// `latency.changed`.
    pub latency_changed: AtomicBool,
    /// `params.rescan` flags, ORed.
    pub rescan: AtomicU32,
    /// `state.mark_dirty`.
    pub dirty: AtomicBool,
    /// `audio_ports.rescan` or `note_ports.rescan` with more than names.
    pub ports_changed: AtomicBool,
    /// A size the editor asked for (`width << 32 | height`; 0 for none).
    pub resize: AtomicU64,
    timers: Mutex<Vec<Timer>>,
    next_timer: AtomicU32,
    /// Log lines dropped because they came from the audio thread.
    pub logs_dropped: AtomicU32,
}

impl HostShared {
    fn new(name: &str, params: usize) -> HostShared {
        HostShared {
            name: name.to_string(),
            main_thread: os_thread(),
            audio_thread: AtomicUsize::new(0),
            mailbox: ParamMailbox::new(params.saturating_mul(2)),
            events: ArrayQueue::new(EVENT_CAP),
            events_dropped: AtomicU32::new(0),
            restart: AtomicBool::new(false),
            callback: AtomicBool::new(false),
            flush: AtomicBool::new(false),
            latency_changed: AtomicBool::new(false),
            rescan: AtomicU32::new(0),
            dirty: AtomicBool::new(false),
            ports_changed: AtomicBool::new(false),
            resize: AtomicU64::new(0),
            timers: Mutex::new(Vec::new()),
            next_timer: AtomicU32::new(1),
            logs_dropped: AtomicU32::new(0),
        }
    }

    pub fn is_main_thread(&self) -> bool {
        os_thread() == self.main_thread
    }

    pub fn is_audio_thread(&self) -> bool {
        let a = self.audio_thread.load(Ordering::Relaxed);
        a != 0 && a == os_thread()
    }

    /// Marks the calling thread as the audio thread. No allocation: called in `process`.
    pub fn set_audio_thread(&self) {
        self.audio_thread.store(os_thread(), Ordering::Relaxed);
    }

    /// No thread is the audio thread until the next block.
    pub fn clear_audio_thread(&self) {
        self.audio_thread.store(0, Ordering::Relaxed);
    }

    /// Queues an event for `poll`. Any thread; never allocates or locks.
    pub fn report(&self, e: ControllerEvent) {
        if self.events.push(e).is_err() {
            self.events_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Moves the reported events into `out`. A run of edits to one control collapses into its
    /// latest value. Main thread.
    pub fn take_events(&self, out: &mut Vec<ControllerEvent>) {
        while let Some(e) = self.events.pop() {
            if let (ControllerEvent::ParamEdit { id, value }, Some(ControllerEvent::ParamEdit { id: last_id, value: last })) = (e, out.last_mut())
                && id == *last_id
            {
                *last = value;
                continue;
            }
            out.push(e);
        }
    }

    pub fn events_dropped(&self) -> u32 {
        self.events_dropped.load(Ordering::Relaxed)
    }

    fn timers(&self) -> std::sync::MutexGuard<'_, Vec<Timer>> {
        self.timers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register_timer(&self, period_ms: u32) -> Option<clap_id> {
        let mut t = self.timers();
        if t.len() >= MAX_TIMERS {
            return None;
        }
        let id = self.next_timer.fetch_add(1, Ordering::Relaxed);
        let period = Duration::from_millis(u64::from(period_ms)).max(MIN_TIMER_PERIOD);
        t.push(Timer { id, period, next: Instant::now() + period });
        Some(id)
    }

    fn unregister_timer(&self, id: clap_id) -> bool {
        let mut t = self.timers();
        let before = t.len();
        t.retain(|x| x.id != id);
        t.len() != before
    }

    /// The timers due at `now`, each moved on to its next time (a timer that fell behind fires
    /// once, not once per missed period). Main thread.
    pub fn due_timers(&self, now: Instant) -> Vec<clap_id> {
        let mut due = Vec::new();
        for t in self.timers().iter_mut().filter(|t| t.next <= now) {
            due.push(t.id);
            t.next = now + t.period;
        }
        due
    }

    /// Whether timer `id` is still registered (a timer callback may unregister another).
    pub fn has_timer(&self, id: clap_id) -> bool {
        self.timers().iter().any(|t| t.id == id)
    }

    #[cfg(test)]
    pub fn timer_count(&self) -> usize {
        self.timers().len()
    }
}

/// The `clap_host` handed to one plugin instance, and the state its callbacks write.
pub(crate) struct Host {
    raw: Box<clap_host>,
    pub shared: Box<HostShared>,
}

// SAFETY: `raw` holds pointers to static strings and to `shared`, which is `Sync` (atomics, a
// lock-free queue and a mutex) and lives as long as `raw` (both are owned here).
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

impl Host {
    /// A host for a plugin called `name` with room in its mailbox for `params` parameters.
    pub fn new(name: &str, params: usize) -> Host {
        let shared = Box::new(HostShared::new(name, params));
        let raw = Box::new(clap_host {
            clap_version: CLAP_VERSION,
            host_data: (&*shared as *const HostShared).cast_mut().cast::<c_void>(),
            name: c"tuneKore".as_ptr(),
            vendor: c"devKore".as_ptr(),
            url: c"https://tunekore.com".as_ptr(),
            version: concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast(),
            get_extension: Some(get_extension),
            request_restart: Some(request_restart),
            request_process: Some(request_process),
            request_callback: Some(request_callback),
        });
        Host { raw, shared }
    }

    /// The pointer the plugin gets. Valid as long as this `Host`.
    pub fn as_ptr(&self) -> *const clap_host {
        &*self.raw
    }
}

/// The state behind a host pointer the plugin passes back.
///
/// # Safety
/// `host` is null or one of our `clap_host` structs, alive (the instance owns it and destroys the
/// plugin before it).
unsafe fn shared<'a>(host: *const clap_host) -> Option<&'a HostShared> {
    if host.is_null() {
        return None;
    }
    // SAFETY: per the contract, a live host made by `Host::new`, whose `host_data` is its state.
    unsafe { ((*host).host_data as *const HostShared).as_ref() }
}

// The callbacks. CLAP calls them from whatever thread it says (some from any); none of them
// unwinds (nothing in them panics) and none allocates or locks unless CLAP says it is called on
// the main thread.

unsafe extern "C" fn get_extension(_host: *const clap_host, id: *const c_char) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    // SAFETY: extension ids are NUL-terminated strings (CLAP contract); non-null checked above.
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_LOG {
        (&HOST_LOG as *const clap_host_log).cast()
    } else if id == CLAP_EXT_THREAD_CHECK {
        (&HOST_THREAD_CHECK as *const clap_host_thread_check).cast()
    } else if id == CLAP_EXT_PARAMS {
        (&HOST_PARAMS as *const clap_host_params).cast()
    } else if id == CLAP_EXT_STATE {
        (&HOST_STATE as *const clap_host_state).cast()
    } else if id == CLAP_EXT_LATENCY {
        (&HOST_LATENCY as *const clap_host_latency).cast()
    } else if id == CLAP_EXT_TAIL {
        (&HOST_TAIL as *const clap_host_tail).cast()
    } else if id == CLAP_EXT_GUI {
        (&HOST_GUI as *const clap_host_gui).cast()
    } else if id == CLAP_EXT_TIMER_SUPPORT {
        (&HOST_TIMER as *const clap_host_timer_support).cast()
    } else if id == CLAP_EXT_AUDIO_PORTS {
        (&HOST_AUDIO_PORTS as *const clap_host_audio_ports).cast()
    } else if id == CLAP_EXT_NOTE_PORTS {
        (&HOST_NOTE_PORTS as *const clap_host_note_ports).cast()
    } else {
        // Everything else (POSIX fd support, thread pool, presets, ...) is not offered; CLAP
        // allows null.
        std::ptr::null()
    }
}

unsafe extern "C" fn request_restart(host: *const clap_host) {
    // SAFETY: the plugin passes back the host pointer we gave it (all callbacks below too).
    if let Some(s) = unsafe { shared(host) } {
        s.restart.store(true, Ordering::Release);
    }
}

/// The engine runs every device every block, so there is nothing to wake.
unsafe extern "C" fn request_process(_host: *const clap_host) {}

unsafe extern "C" fn request_callback(host: *const clap_host) {
    if let Some(s) = unsafe { shared(host) } {
        s.callback.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn log(host: *const clap_host, severity: clap_log_severity, msg: *const c_char) {
    let Some(s) = (unsafe { shared(host) }) else { return };
    // No logging on the audio thread: the line is counted instead.
    if s.is_audio_thread() {
        s.logs_dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if msg.is_null() {
        return;
    }
    // SAFETY: a log message is a NUL-terminated string valid for the call.
    let text = unsafe { super::module::read_cstr(msg) };
    let level = match severity {
        CLAP_LOG_DEBUG => log::Level::Debug,
        CLAP_LOG_INFO => log::Level::Info,
        CLAP_LOG_WARNING => log::Level::Warn,
        _ => log::Level::Error,
    };
    let who = if severity == CLAP_LOG_HOST_MISBEHAVING { " (says tuneKore misbehaves)" } else { "" };
    log::log!(level, "clap {}{who}: {text}", s.name);
}

unsafe extern "C" fn is_main_thread(host: *const clap_host) -> bool {
    unsafe { shared(host) }.is_some_and(HostShared::is_main_thread)
}

unsafe extern "C" fn is_audio_thread(host: *const clap_host) -> bool {
    unsafe { shared(host) }.is_some_and(HostShared::is_audio_thread)
}

unsafe extern "C" fn params_rescan(host: *const clap_host, flags: clap_param_rescan_flags) {
    if let Some(s) = unsafe { shared(host) } {
        s.rescan.fetch_or(flags, Ordering::AcqRel);
    }
}

/// Automation and modulation are the engine's; there is none of the plugin's to clear yet.
unsafe extern "C" fn params_clear(_host: *const clap_host, _id: clap_id, _flags: clap_param_clear_flags) {}

unsafe extern "C" fn params_request_flush(host: *const clap_host) {
    if let Some(s) = unsafe { shared(host) } {
        s.flush.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn state_mark_dirty(host: *const clap_host) {
    if let Some(s) = unsafe { shared(host) } {
        s.dirty.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn latency_changed(host: *const clap_host) {
    if let Some(s) = unsafe { shared(host) } {
        s.latency_changed.store(true, Ordering::Release);
    }
}

/// The engine doesn't render tails yet; the controller reads the tail when asked.
unsafe extern "C" fn tail_changed(_host: *const clap_host) {}

unsafe extern "C" fn gui_resize_hints_changed(_host: *const clap_host) {}

unsafe extern "C" fn gui_request_resize(host: *const clap_host, width: u32, height: u32) -> bool {
    let Some(s) = (unsafe { shared(host) }) else { return false };
    if width == 0 || height == 0 {
        return false;
    }
    s.resize.store((u64::from(width.min(16_384)) << 32) | u64::from(height.min(16_384)), Ordering::Release);
    true
}

/// Editors are embedded in windows tuneKore shows and hides itself.
unsafe extern "C" fn gui_request_show(_host: *const clap_host) -> bool {
    false
}

unsafe extern "C" fn gui_request_hide(_host: *const clap_host) -> bool {
    false
}

/// Only for floating windows, which tuneKore doesn't ask for.
unsafe extern "C" fn gui_closed(_host: *const clap_host, _was_destroyed: bool) {}

unsafe extern "C" fn register_timer(host: *const clap_host, period_ms: u32, timer_id: *mut clap_id) -> bool {
    let Some(s) = (unsafe { shared(host) }) else { return false };
    if timer_id.is_null() || !s.is_main_thread() {
        return false;
    }
    match s.register_timer(period_ms) {
        Some(id) => {
            // SAFETY: the plugin's out-pointer, non-null.
            unsafe { *timer_id = id };
            true
        }
        None => false,
    }
}

unsafe extern "C" fn unregister_timer(host: *const clap_host, timer_id: clap_id) -> bool {
    let Some(s) = (unsafe { shared(host) }) else { return false };
    s.is_main_thread() && s.unregister_timer(timer_id)
}

unsafe extern "C" fn audio_ports_rescan_supported(_host: *const clap_host, _flag: u32) -> bool {
    true
}

unsafe extern "C" fn audio_ports_rescan(host: *const clap_host, flags: u32) {
    // Names alone change nothing the engine uses; anything else needs a restart.
    if flags & !CLAP_AUDIO_PORTS_RESCAN_NAMES != 0
        && let Some(s) = unsafe { shared(host) }
    {
        s.ports_changed.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn note_dialects(_host: *const clap_host) -> clap_note_dialect {
    CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI
}

unsafe extern "C" fn note_ports_rescan(host: *const clap_host, flags: u32) {
    if flags & CLAP_NOTE_PORTS_RESCAN_ALL != 0
        && let Some(s) = unsafe { shared(host) }
    {
        s.ports_changed.store(true, Ordering::Release);
    }
}

static HOST_LOG: clap_host_log = clap_host_log { log: Some(log) };
static HOST_THREAD_CHECK: clap_host_thread_check = clap_host_thread_check { is_main_thread: Some(is_main_thread), is_audio_thread: Some(is_audio_thread) };
static HOST_PARAMS: clap_host_params = clap_host_params { rescan: Some(params_rescan), clear: Some(params_clear), request_flush: Some(params_request_flush) };
static HOST_STATE: clap_host_state = clap_host_state { mark_dirty: Some(state_mark_dirty) };
static HOST_LATENCY: clap_host_latency = clap_host_latency { changed: Some(latency_changed) };
static HOST_TAIL: clap_host_tail = clap_host_tail { changed: Some(tail_changed) };
static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(gui_resize_hints_changed),
    request_resize: Some(gui_request_resize),
    request_show: Some(gui_request_show),
    request_hide: Some(gui_request_hide),
    closed: Some(gui_closed),
};
static HOST_TIMER: clap_host_timer_support = clap_host_timer_support { register_timer: Some(register_timer), unregister_timer: Some(unregister_timer) };
static HOST_AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports { is_rescan_flag_supported: Some(audio_ports_rescan_supported), rescan: Some(audio_ports_rescan) };
static HOST_NOTE_PORTS: clap_host_note_ports = clap_host_note_ports { supported_dialects: Some(note_dialects), rescan: Some(note_ports_rescan) };

#[cfg(test)]
mod tests {
    use super::*;

    fn ext<T>(h: &Host, id: &CStr) -> &'static T {
        // SAFETY: our own callback with our own host; the result points at a static of type `T`.
        let p = unsafe { get_extension(h.as_ptr(), id.as_ptr()) }.cast::<T>();
        unsafe { p.as_ref() }.expect("offered")
    }

    #[test]
    fn the_main_thread_is_the_one_that_made_the_host_and_the_audio_thread_the_one_processing() {
        let h = Host::new("Test", 8);
        let tc: &clap_host_thread_check = ext(&h, CLAP_EXT_THREAD_CHECK);
        let p = h.as_ptr();
        // SAFETY: our callbacks, our host.
        unsafe {
            assert!((tc.is_main_thread.unwrap())(p));
            assert!(!(tc.is_audio_thread.unwrap())(p), "nothing has processed yet");
        }
        let addr = p as usize;
        let other = std::thread::spawn(move || {
            let p = addr as *const clap_host;
            // SAFETY: as above; `h` outlives the thread (joined below).
            let shared = unsafe { shared(p) }.unwrap();
            let before = unsafe { (tc.is_main_thread.unwrap())(p) };
            shared.set_audio_thread();
            (before, unsafe { (tc.is_audio_thread.unwrap())(p) })
        });
        assert_eq!(other.join().unwrap(), (false, true));
        // SAFETY: as above.
        assert!(!unsafe { (tc.is_audio_thread.unwrap())(p) }, "the main thread isn't the audio thread");
    }

    #[test]
    fn requests_become_flags_and_unknown_extensions_are_null() {
        let h = Host::new("Test", 8);
        let p = h.as_ptr();
        // SAFETY: our callbacks, our host.
        unsafe {
            (*p).request_callback.unwrap()(p);
            (*p).request_restart.unwrap()(p);
            let params: &clap_host_params = ext(&h, CLAP_EXT_PARAMS);
            params.request_flush.unwrap()(p);
            params.rescan.unwrap()(p, 1);
            params.rescan.unwrap()(p, 4);
            let gui: &clap_host_gui = ext(&h, CLAP_EXT_GUI);
            assert!(gui.request_resize.unwrap()(p, 640, 480));
            assert!(!gui.request_resize.unwrap()(p, 0, 480), "no empty sizes");
            let ports: &clap_host_audio_ports = ext(&h, CLAP_EXT_AUDIO_PORTS);
            ports.rescan.unwrap()(p, CLAP_AUDIO_PORTS_RESCAN_NAMES);
            assert!(!h.shared.ports_changed.load(Ordering::Acquire), "names alone don't need a restart");
            ports.rescan.unwrap()(p, CLAP_AUDIO_PORTS_RESCAN_NAMES | 4);
            assert!(get_extension(p, c"clap.posix-fd-support".as_ptr()).is_null());
            assert!(get_extension(p, std::ptr::null()).is_null());
        }
        let s = &h.shared;
        assert!(s.callback.load(Ordering::Acquire) && s.restart.load(Ordering::Acquire) && s.flush.load(Ordering::Acquire));
        assert_eq!(s.rescan.load(Ordering::Acquire), 5, "rescan flags add up");
        assert_eq!(s.resize.load(Ordering::Acquire), (640 << 32) | 480);
        assert!(s.ports_changed.load(Ordering::Acquire));
    }

    #[test]
    fn timers_are_main_thread_only_and_fire_once_when_late() {
        let h = Host::new("Test", 8);
        let t: &clap_host_timer_support = ext(&h, CLAP_EXT_TIMER_SUPPORT);
        let p = h.as_ptr();
        let mut id = 0;
        // SAFETY: our callbacks, our host, a writable id.
        assert!(unsafe { (t.register_timer.unwrap())(p, 1, &mut id) });
        assert_eq!(h.shared.timer_count(), 1);
        let addr = p as usize;
        let off_main = std::thread::spawn(move || {
            let mut id = 0;
            // SAFETY: as above; `h` outlives the thread.
            unsafe { (t.register_timer.unwrap())(addr as *const clap_host, 20, &mut id) }
        });
        assert!(!off_main.join().unwrap(), "refused off the main thread");
        let later = Instant::now() + Duration::from_secs(1);
        assert_eq!(h.shared.due_timers(later), vec![id], "a 1 ms timer is kept to 10 ms and fires once");
        assert!(h.shared.due_timers(later).is_empty());
        // SAFETY: as above.
        assert!(unsafe { (t.unregister_timer.unwrap())(p, id) });
        assert!(!unsafe { (t.unregister_timer.unwrap())(p, id) }, "already gone");
        assert!(!h.shared.has_timer(id));
    }

    #[test]
    fn edits_to_one_control_collapse_into_the_latest() {
        let h = Host::new("Test", 8);
        for v in [0.1, 0.2, 0.3] {
            h.shared.report(ControllerEvent::ParamEdit { id: 7, value: v });
        }
        h.shared.report(ControllerEvent::EditEnd { id: 7 });
        let mut out = Vec::new();
        h.shared.take_events(&mut out);
        assert_eq!(out, vec![ControllerEvent::ParamEdit { id: 7, value: 0.3 }, ControllerEvent::EditEnd { id: 7 }]);
    }
}
