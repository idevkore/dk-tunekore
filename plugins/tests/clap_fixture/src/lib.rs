//! tuneKore's CLAP test plugins: three in one `.clap`, written for the plugin host's tests.
//!
//! - **Fixture CLAP Gain** (`dev.tunekore.fixture.gain`): an effect with a stereo main input, a
//!   stereo sidechain and a stereo output. Output = main input × Gain, or the input when bypassed.
//!   Parameters: 7 Gain (0 to 2, default 1), 3 Mode (stepped, 0 to 2), 9 Hidden, 11 Bypass,
//!   12 Frequency (20 to 20,000 Hz, default 1,000), and read-only ones the tests read: 20 timer
//!   ticks, 21 main-thread callbacks, 22 errors, 23 the sample rate it was activated at, 24 how
//!   many times its module is initialised right now. Latency 3 samples, 5 in mode C: switching mode C in or out
//!   asks the host for a restart (and, while off, says the latency changed); activating with a
//!   new latency says so too. Tail 100. State: `FXCG` and gain, mode and frequency as
//!   little-endian f64s; loading one tells the host the values changed. It turns ports on and off
//!   (`audio-ports-activation`).
//!
//!   Its editor embeds in a cocoa or win32 view, 320 × 240, resizable within 200 × 150 and
//!   1000 × 800. Creating it starts a 20 ms timer and asks for a main-thread callback; showing it
//!   "turns the gain knob" to 1.8 (handed back as a gesture and a value at the next flush or
//!   block) and asks the host for 400 × 300.
//!
//!   It checks the buffers it is given: every port gets as many channels as it declares, and a
//!   port the host switched off is marked constant. If not, it writes -1.
//!
//! Every plugin counts errors (the Gain shows the count as parameter 22), from its own records,
//! not only the host's word: a main-thread call on another thread than the one that made it (or
//! while the host says it is an audio thread), `process` on another thread than
//! `start_processing`, audio-thread calls that overlap, or calls out of CLAP's order (activating
//! twice, deactivating while processing, processing while not started).
//! - **Fixture CLAP Synth** (`.synth`): an instrument (CLAP and MIDI notes, CLAP preferred) with a
//!   stereo output: the held note's velocity from the note's frame, 0 after its note-off. MIDI CC 7
//!   sets the level to its value / 127. Parameter 7 Volume (0 to 1) scales it. No state.
//! - **Fixture CLAP Thru** (`.thru`): passes audio through and hands back every note, MIDI
//!   message and parameter value it gets (notes as MIDI only), allocating nothing. Its state is
//!   empty: it saves nothing and loads only nothing.
#![allow(clippy::missing_safety_doc)]

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::*;
use clap_sys::ext::audio_ports::*;
use clap_sys::ext::audio_ports_activation::*;
use clap_sys::ext::gui::*;
use clap_sys::ext::latency::*;
use clap_sys::ext::note_ports::*;
use clap_sys::ext::params::*;
use clap_sys::ext::state::*;
use clap_sys::ext::tail::*;
use clap_sys::ext::thread_check::*;
use clap_sys::ext::timer_support::*;
use clap_sys::factory::plugin_factory::*;
use clap_sys::host::clap_host;
use clap_sys::id::{clap_id, CLAP_INVALID_ID};
use clap_sys::plugin::*;
use clap_sys::process::*;
use clap_sys::stream::*;
use clap_sys::string_sizes::{CLAP_NAME_SIZE, CLAP_PATH_SIZE};
use clap_sys::version::CLAP_VERSION;
use std::cell::UnsafeCell;
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// `clap_entry.init` calls not yet balanced by `deinit`: CLAP allows one at a time.
static MODULE_INITS: AtomicI32 = AtomicI32::new(0);

/// The calling thread's OS id, for the plugin's own thread records.
fn os_thread() -> usize {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn pthread_self() -> usize;
        }
        unsafe { pthread_self() }
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThreadId() -> u32;
        }
        unsafe { GetCurrentThreadId() as usize }
    }
}

struct Features<const N: usize>([*const c_char; N]);
// SAFETY: pointers to static strings, never written.
unsafe impl<const N: usize> Sync for Features<N> {}

static GAIN_FEATURES: Features<3> = Features([c"audio-effect".as_ptr(), c"stereo".as_ptr(), std::ptr::null()]);
static SYNTH_FEATURES: Features<3> = Features([c"instrument".as_ptr(), c"synthesizer".as_ptr(), std::ptr::null()]);
static THRU_FEATURES: Features<3> = Features([c"audio-effect".as_ptr(), c"utility".as_ptr(), std::ptr::null()]);

const fn desc(id: &'static CStr, name: &'static CStr, features: &'static Features<3>) -> clap_plugin_descriptor {
    clap_plugin_descriptor {
        clap_version: CLAP_VERSION,
        id: id.as_ptr(),
        name: name.as_ptr(),
        vendor: c"tuneKore tests".as_ptr(),
        url: c"".as_ptr(),
        manual_url: c"".as_ptr(),
        support_url: c"".as_ptr(),
        version: c"1.2.3".as_ptr(),
        description: c"A plugin for tuneKore's tests".as_ptr(),
        features: features.0.as_ptr(),
    }
}

static GAIN_DESC: clap_plugin_descriptor = desc(c"dev.tunekore.fixture.gain", c"Fixture CLAP Gain", &GAIN_FEATURES);
static SYNTH_DESC: clap_plugin_descriptor = desc(c"dev.tunekore.fixture.synth", c"Fixture CLAP Synth", &SYNTH_FEATURES);
static THRU_DESC: clap_plugin_descriptor = desc(c"dev.tunekore.fixture.thru", c"Fixture CLAP Thru", &THRU_FEATURES);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Gain,
    Synth,
    Thru,
}

const GAIN: clap_id = 7;
const MODE: clap_id = 3;
const HIDDEN: clap_id = 9;
const BYPASS: clap_id = 11;
const FREQ: clap_id = 12;
const TICKS: clap_id = 20;
const CALLBACKS: clap_id = 21;
const ERRORS: clap_id = 22;
const RATE: clap_id = 23;
const INITS: clap_id = 24;

/// (id, name, min, max, default, flags) for the Gain's parameters.
const GAIN_PARAMS: [(clap_id, &CStr, f64, f64, f64, u32); 10] = [
    (GAIN, c"Gain", 0.0, 2.0, 1.0, CLAP_PARAM_IS_AUTOMATABLE),
    (MODE, c"Mode", 0.0, 2.0, 0.0, CLAP_PARAM_IS_AUTOMATABLE | CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_ENUM),
    (HIDDEN, c"Hidden", 0.0, 1.0, 0.0, CLAP_PARAM_IS_HIDDEN),
    (BYPASS, c"Bypass", 0.0, 1.0, 0.0, CLAP_PARAM_IS_AUTOMATABLE | CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_BYPASS),
    (FREQ, c"Frequency", 20.0, 20_000.0, 1_000.0, CLAP_PARAM_IS_AUTOMATABLE),
    (TICKS, c"Timer ticks", 0.0, 1e6, 0.0, CLAP_PARAM_IS_READONLY),
    (CALLBACKS, c"Main-thread calls", 0.0, 1e6, 0.0, CLAP_PARAM_IS_READONLY),
    (ERRORS, c"Errors", 0.0, 1e6, 0.0, CLAP_PARAM_IS_READONLY),
    (RATE, c"Sample rate", 0.0, 1e6, 0.0, CLAP_PARAM_IS_READONLY),
    (INITS, c"Module inits", 0.0, 100.0, 0.0, CLAP_PARAM_IS_READONLY),
];
const SYNTH_PARAMS: [(clap_id, &CStr, f64, f64, f64, u32); 1] = [(GAIN, c"Volume", 0.0, 1.0, 1.0, CLAP_PARAM_IS_AUTOMATABLE)];

const STATE_MAGIC: &[u8; 4] = b"FXCG";
const STATE_LEN: usize = 4 + 3 * 8;

#[cfg(target_os = "macos")]
const API: &CStr = CLAP_WINDOW_API_COCOA;
#[cfg(not(target_os = "macos"))]
const API: &CStr = CLAP_WINDOW_API_WIN32;

/// A value both threads touch.
struct Value(AtomicU64);

impl Value {
    fn new(v: f64) -> Value {
        Value(AtomicU64::new(v.to_bits()))
    }
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Acquire))
    }
    fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Release);
    }
}

/// The editor's state (main thread only).
#[derive(Default)]
struct Gui {
    created: bool,
    parent: usize,
    width: u32,
    height: u32,
    timer: Option<clap_id>,
}

struct Plug {
    plugin: clap_plugin,
    kind: Kind,
    host: *const clap_host,
    host_params: *const clap_host_params,
    host_gui: *const clap_host_gui,
    host_timer: *const clap_host_timer_support,
    host_check: *const clap_host_thread_check,
    host_latency: *const clap_host_latency,
    gain: Value,
    mode: Value,
    hidden: Value,
    bypass: Value,
    freq: Value,
    active: AtomicBool,
    /// The latency the host last heard.
    told_latency: AtomicU32,
    /// Input ports the host switched off (bit per port).
    ports_off: AtomicU32,
    ticks: AtomicU32,
    callbacks: AtomicU32,
    errors: AtomicU32,
    /// The thread that made it (its main thread) and the one that started processing.
    main_tid: AtomicUsize,
    audio_tid: AtomicUsize,
    processing: AtomicBool,
    /// Audio-thread calls running now (never more than one).
    in_audio: AtomicU32,
    /// The sample rate it was last activated at.
    rate: Value,
    /// The editor "turned the gain knob": hand it back at the next flush or block.
    pending_edit: AtomicBool,
    /// The synth's level (audio thread only).
    level: UnsafeCell<f32>,
    gui: UnsafeCell<Gui>,
}

unsafe fn plug<'a>(p: *const clap_plugin) -> &'a Plug {
    unsafe { &*((*p).plugin_data as *const Plug) }
}

/// An audio-thread call, for as long as it runs: checks it doesn't overlap another one and that
/// the host calls this an audio thread (and not the main thread, unless it is).
struct AudioCall<'a>(&'a Plug);

impl<'a> AudioCall<'a> {
    fn new(x: &'a Plug) -> AudioCall<'a> {
        if x.in_audio.fetch_add(1, Ordering::AcqRel) != 0 {
            x.error();
        }
        if !x.host_says(|c| c.is_audio_thread) {
            x.error();
        }
        if os_thread() != x.main_tid.load(Ordering::Acquire) && x.host_says(|c| c.is_main_thread) {
            x.error();
        }
        AudioCall(x)
    }
}

impl Drop for AudioCall<'_> {
    fn drop(&mut self) {
        self.0.in_audio.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Plug {
    fn error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// What the host's thread check answers (true when it has none).
    fn host_says(&self, f: impl Fn(&clap_host_thread_check) -> Option<unsafe extern "C" fn(*const clap_host) -> bool>) -> bool {
        unsafe { self.host_check.as_ref() }.and_then(f).is_none_or(|f| unsafe { f(self.host) })
    }

    /// A main-thread call: on the thread that made the plugin, which the host calls the main
    /// thread and not an audio thread.
    fn main_thread(&self) {
        if os_thread() != self.main_tid.load(Ordering::Acquire) || !self.host_says(|c| c.is_main_thread) || (!self.host_check.is_null() && self.host_says(|c| c.is_audio_thread)) {
            self.error();
        }
    }

    fn params(&self) -> &'static [(clap_id, &'static CStr, f64, f64, f64, u32)] {
        match self.kind {
            Kind::Gain => &GAIN_PARAMS,
            Kind::Synth => &SYNTH_PARAMS,
            Kind::Thru => &[],
        }
    }

    fn value(&self, id: clap_id) -> Option<f64> {
        match (self.kind, id) {
            (Kind::Gain | Kind::Synth, GAIN) => Some(self.gain.get()),
            (Kind::Gain, MODE) => Some(self.mode.get()),
            (Kind::Gain, HIDDEN) => Some(self.hidden.get()),
            (Kind::Gain, BYPASS) => Some(self.bypass.get()),
            (Kind::Gain, FREQ) => Some(self.freq.get()),
            (Kind::Gain, TICKS) => Some(f64::from(self.ticks.load(Ordering::Relaxed))),
            (Kind::Gain, CALLBACKS) => Some(f64::from(self.callbacks.load(Ordering::Relaxed))),
            (Kind::Gain, ERRORS) => Some(f64::from(self.errors.load(Ordering::Relaxed))),
            (Kind::Gain, RATE) => Some(self.rate.get()),
            (Kind::Gain, INITS) => Some(f64::from(MODULE_INITS.load(Ordering::Acquire))),
            _ => None,
        }
    }

    fn latency(&self) -> u32 {
        if self.mode.get() >= 1.5 { 5 } else { 3 }
    }

    fn set_value(&self, id: clap_id, v: f64) {
        match (self.kind, id) {
            (Kind::Gain | Kind::Synth, GAIN) => self.gain.set(v),
            (Kind::Gain, MODE) => {
                let before = self.latency();
                self.mode.set(v);
                if self.latency() != before {
                    // Any thread may ask for a restart; only the main thread, while off, may say the
                    // latency changed.
                    if let Some(restart) = unsafe { (*self.host).request_restart } {
                        unsafe { restart(self.host) };
                    }
                    if !self.active.load(Ordering::Acquire)
                        && let Some(changed) = unsafe { self.host_latency.as_ref() }.and_then(|l| l.changed)
                    {
                        self.main_thread();
                        unsafe { changed(self.host) };
                        self.told_latency.store(self.latency(), Ordering::Release);
                    }
                }
            }
            (Kind::Gain, HIDDEN) => self.hidden.set(v),
            (Kind::Gain, BYPASS) => self.bypass.set(v),
            (Kind::Gain, FREQ) => self.freq.set(v),
            _ => {}
        }
    }

    /// Applies one input event; returns the synth's new level, if it changes it.
    fn apply(&self, h: &clap_event_header) -> Option<f32> {
        if h.space_id != CLAP_CORE_EVENT_SPACE_ID {
            return None;
        }
        match h.type_ {
            CLAP_EVENT_PARAM_VALUE => {
                let e = unsafe { &*(h as *const clap_event_header).cast::<clap_event_param_value>() };
                self.set_value(e.param_id, e.value);
                None
            }
            CLAP_EVENT_NOTE_ON => Some(unsafe { &*(h as *const clap_event_header).cast::<clap_event_note>() }.velocity as f32),
            CLAP_EVENT_NOTE_OFF => Some(0.0),
            CLAP_EVENT_MIDI => {
                let d = unsafe { &*(h as *const clap_event_header).cast::<clap_event_midi>() }.data;
                match d[0] & 0xF0 {
                    0x90 if d[2] > 0 => Some(f32::from(d[2]) / 127.0),
                    0x80 | 0x90 => Some(0.0),
                    0xB0 if d[1] == 7 => Some(f32::from(d[2]) / 127.0),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The knob the editor turned, handed back as a gesture and a value.
    fn hand_back_edit(&self, out: *const clap_output_events) {
        if !self.pending_edit.swap(false, Ordering::AcqRel) {
            return;
        }
        let Some(out) = (unsafe { out.as_ref() }) else { return };
        let Some(push) = out.try_push else { return };
        let header = |type_: u16, size: usize| clap_event_header { size: size as u32, time: 0, space_id: CLAP_CORE_EVENT_SPACE_ID, type_, flags: 0 };
        let begin = clap_event_param_gesture { header: header(CLAP_EVENT_PARAM_GESTURE_BEGIN, size_of::<clap_event_param_gesture>()), param_id: GAIN };
        let value = clap_event_param_value {
            header: header(CLAP_EVENT_PARAM_VALUE, size_of::<clap_event_param_value>()),
            param_id: GAIN,
            cookie: std::ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value: self.gain.get(),
        };
        let end = clap_event_param_gesture { header: header(CLAP_EVENT_PARAM_GESTURE_END, size_of::<clap_event_param_gesture>()), param_id: GAIN };
        unsafe {
            push(out, &begin.header);
            push(out, &value.header);
            push(out, &end.header);
        }
    }
}

// ---- the plugin --------------------------------------------------------------------------------

unsafe extern "C" fn init(p: *const clap_plugin) -> bool {
    let x = unsafe { &mut *((*p).plugin_data as *mut Plug) };
    let ext = |id: &CStr| unsafe { (*x.host).get_extension.map_or(std::ptr::null(), |f| f(x.host, id.as_ptr())) };
    x.host_params = ext(CLAP_EXT_PARAMS).cast();
    x.host_gui = ext(CLAP_EXT_GUI).cast();
    x.host_timer = ext(CLAP_EXT_TIMER_SUPPORT).cast();
    x.host_check = ext(CLAP_EXT_THREAD_CHECK).cast();
    x.host_latency = ext(CLAP_EXT_LATENCY).cast();
    x.main_tid.store(os_thread(), Ordering::Release);
    x.main_thread();
    true
}

unsafe extern "C" fn destroy(p: *const clap_plugin) {
    drop(unsafe { Box::from_raw((*p).plugin_data as *mut Plug) });
}

unsafe extern "C" fn activate(p: *const clap_plugin, rate: f64, min: u32, max: u32) -> bool {
    let x = unsafe { plug(p) };
    x.main_thread();
    if x.active.load(Ordering::Acquire) {
        x.error();
    }
    x.rate.set(rate);
    // A latency the host hasn't heard yet is told while activating, as CLAP allows.
    if x.kind == Kind::Gain
        && x.told_latency.swap(x.latency(), Ordering::AcqRel) != x.latency()
        && let Some(changed) = unsafe { x.host_latency.as_ref() }.and_then(|l| l.changed)
    {
        unsafe { changed(x.host) };
    }
    x.active.store(true, Ordering::Release);
    min >= 1 && max >= min
}

unsafe extern "C" fn deactivate(p: *const clap_plugin) {
    let x = unsafe { plug(p) };
    x.main_thread();
    if !x.active.load(Ordering::Acquire) || x.processing.load(Ordering::Acquire) {
        x.error();
    }
    x.active.store(false, Ordering::Release);
}

unsafe extern "C" fn start_processing(p: *const clap_plugin) -> bool {
    let x = unsafe { plug(p) };
    let _call = AudioCall::new(x);
    if !x.active.load(Ordering::Acquire) || x.processing.swap(true, Ordering::AcqRel) {
        x.error();
    }
    x.audio_tid.store(os_thread(), Ordering::Release);
    true
}

unsafe extern "C" fn stop_processing(p: *const clap_plugin) {
    let x = unsafe { plug(p) };
    let _call = AudioCall::new(x);
    if !x.processing.swap(false, Ordering::AcqRel) {
        x.error();
    }
}

unsafe extern "C" fn reset(p: *const clap_plugin) {
    let x = unsafe { plug(p) };
    let _call = AudioCall::new(x);
    if !x.active.load(Ordering::Acquire) {
        x.error();
    }
}

/// Channel `c` of buffer `b`.
unsafe fn channel<'a>(b: &clap_audio_buffer, c: usize, n: usize) -> &'a mut [f32] {
    unsafe { std::slice::from_raw_parts_mut(*b.data32.add(c), n) }
}

/// Whether every port has the buffers it declared (2 channels each), and ports that are off are
/// marked silent.
fn buffers_fit(x: &Plug, pr: &clap_process, inputs: u32, outputs: u32) -> bool {
    let fits = |bufs: *const clap_audio_buffer, n: u32| {
        (n == 0 || !bufs.is_null())
            && (0..n as usize).all(|i| {
                let b = unsafe { &*bufs.add(i) };
                b.channel_count == 2 && !b.data32.is_null() && (0..2).all(|c| !unsafe { *b.data32.add(c) }.is_null())
            })
    };
    let off = x.ports_off.load(Ordering::Acquire);
    let marked = (0..inputs as usize).all(|i| off & (1 << i) == 0 || unsafe { (*pr.audio_inputs.add(i)).constant_mask } & 0b11 == 0b11);
    pr.audio_inputs_count == inputs && pr.audio_outputs_count == outputs && fits(pr.audio_inputs, inputs) && fits(pr.audio_outputs.cast_const(), outputs) && marked
}

/// Whether the first output port has two channels to write.
fn output_fits(pr: &clap_process) -> bool {
    pr.audio_outputs_count >= 1
        && !pr.audio_outputs.is_null()
        && unsafe { (*pr.audio_outputs).channel_count >= 2 && !(*pr.audio_outputs).data32.is_null() }
        && (0..2).all(|c| !unsafe { *(*pr.audio_outputs).data32.add(c) }.is_null())
}

unsafe extern "C" fn process(p: *const clap_plugin, pr: *const clap_process) -> clap_process_status {
    let x = unsafe { plug(p) };
    let _call = AudioCall::new(x);
    // Only between start_processing and stop_processing, on the thread that started it.
    if !x.processing.load(Ordering::Acquire) || os_thread() != x.audio_tid.load(Ordering::Acquire) {
        x.error();
    }
    let pr = unsafe { &*pr };
    let n = pr.frames_count as usize;
    let ins = unsafe { &*pr.in_events };
    let count = ins.size.map_or(0, |f| unsafe { f(ins) });
    let event = |i: u32| unsafe { ins.get.map(|f| f(ins, i)).filter(|h| !h.is_null()).map(|h| &*h) };
    match x.kind {
        Kind::Thru => {
            if !buffers_fit(x, pr, 1, 1) {
                return CLAP_PROCESS_ERROR;
            }
            let (i, o) = unsafe { (&*pr.audio_inputs, &*pr.audio_outputs) };
            for c in 0..2 {
                let (src, dst) = unsafe { (channel(i, c, n), channel(o, c, n)) };
                dst.copy_from_slice(src);
            }
            if let Some(out) = unsafe { pr.out_events.as_ref() }
                && let Some(push) = out.try_push
            {
                for k in 0..count {
                    if let Some(h) = event(k)
                        && matches!(h.type_, CLAP_EVENT_MIDI | CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF | CLAP_EVENT_PARAM_VALUE)
                    {
                        unsafe { push(out, h) };
                    }
                }
            }
        }
        Kind::Gain => {
            let ok = buffers_fit(x, pr, 2, 1);
            if !output_fits(pr) {
                return CLAP_PROCESS_ERROR;
            }
            let out = unsafe { &*pr.audio_outputs };
            // Clear the outputs first, as many plugins do: an input that is also an output reads
            // zeros unless the host copied it.
            for c in 0..2 {
                unsafe { channel(out, c, n) }.fill(0.0);
            }
            let mut next = 0;
            for f in 0..n {
                while next < count && let Some(h) = event(next) && h.time as usize <= f {
                    x.apply(h);
                    next += 1;
                }
                for c in 0..2 {
                    let v = if !ok {
                        -1.0
                    } else {
                        let input = unsafe { channel(&*pr.audio_inputs, c, n) }[f];
                        if x.bypass.get() >= 0.5 { input } else { input * x.gain.get() as f32 }
                    };
                    let ch = unsafe { channel(out, c, n) };
                    ch[f] = v;
                }
            }
            x.hand_back_edit(pr.out_events);
        }
        Kind::Synth => {
            let ok = buffers_fit(x, pr, 0, 1);
            if !output_fits(pr) {
                return CLAP_PROCESS_ERROR;
            }
            let out = unsafe { &*pr.audio_outputs };
            let level = unsafe { &mut *x.level.get() };
            let mut next = 0;
            for f in 0..n {
                while next < count && let Some(h) = event(next) && h.time as usize <= f {
                    if let Some(l) = x.apply(h) {
                        *level = l;
                    }
                    next += 1;
                }
                let v = if ok { *level * x.gain.get() as f32 } else { -1.0 };
                for c in 0..2 {
                    let ch = unsafe { channel(out, c, n) };
                    ch[f] = v;
                }
            }
        }
    }
    CLAP_PROCESS_CONTINUE
}

unsafe extern "C" fn get_extension(p: *const clap_plugin, id: *const c_char) -> *const c_void {
    let kind = unsafe { plug(p) }.kind;
    let id = unsafe { CStr::from_ptr(id) };
    let gain = kind == Kind::Gain;
    if id == CLAP_EXT_PARAMS {
        (&PARAMS as *const clap_plugin_params).cast()
    } else if id == CLAP_EXT_AUDIO_PORTS {
        (&PORTS as *const clap_plugin_audio_ports).cast()
    } else if id == CLAP_EXT_NOTE_PORTS && kind != Kind::Gain {
        (&NOTES as *const clap_plugin_note_ports).cast()
    } else if id == CLAP_EXT_LATENCY && gain {
        (&LATENCY as *const clap_plugin_latency).cast()
    } else if id == CLAP_EXT_TAIL && gain {
        (&TAIL as *const clap_plugin_tail).cast()
    } else if id == CLAP_EXT_STATE && kind != Kind::Synth {
        (&STATE as *const clap_plugin_state).cast()
    } else if id == CLAP_EXT_GUI && gain {
        (&GUI as *const clap_plugin_gui).cast()
    } else if id == CLAP_EXT_TIMER_SUPPORT && gain {
        (&TIMER as *const clap_plugin_timer_support).cast()
    } else if id == CLAP_EXT_AUDIO_PORTS_ACTIVATION && gain {
        (&ACTIVATION as *const clap_plugin_audio_ports_activation).cast()
    } else {
        std::ptr::null()
    }
}

unsafe extern "C" fn on_main_thread(p: *const clap_plugin) {
    let x = unsafe { plug(p) };
    x.main_thread();
    x.callbacks.fetch_add(1, Ordering::Relaxed);
}

// ---- parameters --------------------------------------------------------------------------------

fn write_text(dst: *mut c_char, cap: u32, s: &str) -> bool {
    if dst.is_null() || cap == 0 {
        return false;
    }
    let n = s.len().min(cap as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(s.as_ptr().cast::<c_char>(), dst, n);
        *dst.add(n) = 0;
    }
    true
}

unsafe extern "C" fn params_count(p: *const clap_plugin) -> u32 {
    let x = unsafe { plug(p) };
    x.main_thread();
    x.params().len() as u32
}

unsafe extern "C" fn params_get_info(p: *const clap_plugin, index: u32, info: *mut clap_param_info) -> bool {
    let x = unsafe { plug(p) };
    x.main_thread();
    let Some(&(id, name, min, max, default, flags)) = x.params().get(index as usize) else { return false };
    let info = unsafe { &mut *info };
    info.id = id;
    info.flags = flags;
    info.cookie = std::ptr::null_mut();
    info.name = [0; CLAP_NAME_SIZE];
    for (d, s) in info.name.iter_mut().zip(name.to_bytes()) {
        *d = *s as c_char;
    }
    info.module = [0; CLAP_PATH_SIZE];
    info.min_value = min;
    info.max_value = max;
    info.default_value = default;
    true
}

unsafe extern "C" fn params_get_value(p: *const clap_plugin, id: clap_id, out: *mut f64) -> bool {
    let x = unsafe { plug(p) };
    x.main_thread();
    match x.value(id) {
        Some(v) => {
            unsafe { *out = v };
            true
        }
        None => false,
    }
}

unsafe extern "C" fn params_value_to_text(p: *const clap_plugin, id: clap_id, v: f64, buf: *mut c_char, cap: u32) -> bool {
    let x = unsafe { plug(p) };
    x.main_thread();
    let text = match (x.kind, id) {
        (Kind::Gain, GAIN) => format!("x{v:.2}"),
        (Kind::Gain, MODE) => ["A", "B", "C"][(v.round() as usize).min(2)].to_string(),
        (Kind::Gain, FREQ) => format!("{v:.0} Hz"),
        _ if x.value(id).is_some() => format!("{v:.3}"),
        _ => return false,
    };
    write_text(buf, cap, &text)
}

unsafe extern "C" fn params_text_to_value(_p: *const clap_plugin, _id: clap_id, _text: *const c_char, _out: *mut f64) -> bool {
    false
}

unsafe extern "C" fn params_flush(p: *const clap_plugin, ins: *const clap_input_events, out: *const clap_output_events) {
    let x = unsafe { plug(p) };
    // While active it belongs to the audio thread (never during a block); while off, to the main.
    let _call = if x.active.load(Ordering::Acquire) {
        Some(AudioCall::new(x))
    } else {
        x.main_thread();
        None
    };
    if let Some(ins) = unsafe { ins.as_ref() } {
        let count = ins.size.map_or(0, |f| unsafe { f(ins) });
        for k in 0..count {
            if let Some(h) = unsafe { ins.get.map(|f| f(ins, k)).filter(|h| !h.is_null()).map(|h| &*h) } {
                x.apply(h);
            }
        }
    }
    x.hand_back_edit(out);
}

static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_get_info),
    get_value: Some(params_get_value),
    value_to_text: Some(params_value_to_text),
    text_to_value: Some(params_text_to_value),
    flush: Some(params_flush),
};

// ---- ports -------------------------------------------------------------------------------------

/// (inputs, outputs).
fn port_counts(kind: Kind) -> (u32, u32) {
    match kind {
        Kind::Gain => (2, 1),
        Kind::Synth => (0, 1),
        Kind::Thru => (1, 1),
    }
}

unsafe extern "C" fn ports_count(p: *const clap_plugin, is_input: bool) -> u32 {
    let (i, o) = port_counts(unsafe { plug(p) }.kind);
    if is_input { i } else { o }
}

unsafe extern "C" fn ports_get(p: *const clap_plugin, index: u32, is_input: bool, info: *mut clap_audio_port_info) -> bool {
    let x = unsafe { plug(p) };
    let (i, o) = port_counts(x.kind);
    if index >= if is_input { i } else { o } {
        return false;
    }
    let name: &CStr = match (is_input, index) {
        (true, 0) => c"Main In",
        (true, _) => c"Sidechain",
        _ => c"Main Out",
    };
    let info = unsafe { &mut *info };
    info.id = index + if is_input { 0 } else { 100 };
    info.name = [0; CLAP_NAME_SIZE];
    for (d, s) in info.name.iter_mut().zip(name.to_bytes()) {
        *d = *s as c_char;
    }
    info.flags = if index == 0 { CLAP_AUDIO_PORT_IS_MAIN } else { 0 };
    info.channel_count = 2;
    info.port_type = CLAP_PORT_STEREO.as_ptr();
    info.in_place_pair = CLAP_INVALID_ID;
    true
}

static PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports { count: Some(ports_count), get: Some(ports_get) };

unsafe extern "C" fn activation_while_processing(_p: *const clap_plugin) -> bool {
    false
}

unsafe extern "C" fn activation_set(p: *const clap_plugin, is_input: bool, index: u32, active: bool, _sample_size: u32) -> bool {
    let x = unsafe { plug(p) };
    x.main_thread();
    if x.active.load(Ordering::Acquire) || index >= 32 {
        return false;
    }
    if is_input {
        if active {
            x.ports_off.fetch_and(!(1 << index), Ordering::AcqRel);
        } else {
            x.ports_off.fetch_or(1 << index, Ordering::AcqRel);
        }
    }
    true
}

static ACTIVATION: clap_plugin_audio_ports_activation = clap_plugin_audio_ports_activation { can_activate_while_processing: Some(activation_while_processing), set_active: Some(activation_set) };

unsafe extern "C" fn notes_count(p: *const clap_plugin, is_input: bool) -> u32 {
    match unsafe { plug(p) }.kind {
        Kind::Synth => u32::from(is_input),
        Kind::Thru => 1,
        Kind::Gain => 0,
    }
}

unsafe extern "C" fn notes_get(p: *const clap_plugin, index: u32, is_input: bool, info: *mut clap_note_port_info) -> bool {
    let kind = unsafe { plug(p) }.kind;
    if index != 0 || unsafe { notes_count(p, is_input) } == 0 {
        return false;
    }
    let info = unsafe { &mut *info };
    info.id = 0;
    (info.supported_dialects, info.preferred_dialect) = match kind {
        Kind::Synth => (CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI, CLAP_NOTE_DIALECT_CLAP),
        _ => (CLAP_NOTE_DIALECT_MIDI, CLAP_NOTE_DIALECT_MIDI),
    };
    info.name = [0; CLAP_NAME_SIZE];
    for (d, s) in info.name.iter_mut().zip(b"Notes") {
        *d = *s as c_char;
    }
    true
}

static NOTES: clap_plugin_note_ports = clap_plugin_note_ports { count: Some(notes_count), get: Some(notes_get) };

unsafe extern "C" fn latency_get(p: *const clap_plugin) -> u32 {
    let x = unsafe { plug(p) };
    x.main_thread();
    x.latency()
}

static LATENCY: clap_plugin_latency = clap_plugin_latency { get: Some(latency_get) };

unsafe extern "C" fn tail_get(_p: *const clap_plugin) -> u32 {
    100
}

static TAIL: clap_plugin_tail = clap_plugin_tail { get: Some(tail_get) };

// ---- state -------------------------------------------------------------------------------------

unsafe extern "C" fn state_save(p: *const clap_plugin, stream: *const clap_ostream) -> bool {
    let x = unsafe { plug(p) };
    x.main_thread();
    let s = unsafe { &*stream };
    let Some(write) = s.write else { return false };
    if x.kind == Kind::Thru {
        // Nothing to keep: an empty state, and that is a success.
        return true;
    }
    let mut values = Vec::with_capacity(24);
    for v in [x.gain.get(), x.mode.get(), x.freq.get()] {
        values.extend_from_slice(&v.to_le_bytes());
    }
    // In two pieces, as plugins often write.
    unsafe { write(s, STATE_MAGIC.as_ptr().cast(), 4) == 4 && write(s, values.as_ptr().cast(), values.len() as u64) == values.len() as i64 }
}

unsafe extern "C" fn state_load(p: *const clap_plugin, stream: *const clap_istream) -> bool {
    let x = unsafe { plug(p) };
    x.main_thread();
    let s = unsafe { &*stream };
    let Some(read) = s.read else { return false };
    if x.kind == Kind::Thru {
        let mut one = [0u8; 1];
        return unsafe { read(s, one.as_mut_ptr().cast(), 1) } == 0;
    }
    let mut data = Vec::new();
    let mut buf = [0u8; 5];
    loop {
        let n = unsafe { read(s, buf.as_mut_ptr().cast(), buf.len() as u64) };
        if n < 0 {
            return false;
        }
        if n == 0 || data.len() > STATE_LEN {
            break;
        }
        data.extend_from_slice(&buf[..n as usize]);
    }
    if data.len() != STATE_LEN || &data[..4] != STATE_MAGIC {
        return false;
    }
    let f = |i: usize| f64::from_le_bytes(data[4 + i * 8..12 + i * 8].try_into().unwrap());
    x.gain.set(f(0));
    x.mode.set(f(1));
    x.freq.set(f(2));
    if let Some(rescan) = unsafe { x.host_params.as_ref() }.and_then(|h| h.rescan) {
        unsafe { rescan(x.host, CLAP_PARAM_RESCAN_VALUES) };
    }
    true
}

static STATE: clap_plugin_state = clap_plugin_state { save: Some(state_save), load: Some(state_load) };

// ---- the editor --------------------------------------------------------------------------------

unsafe fn gui<'a>(p: *const clap_plugin) -> &'a mut Gui {
    unsafe { &mut *plug(p).gui.get() }
}

unsafe extern "C" fn gui_is_api_supported(p: *const clap_plugin, api: *const c_char, floating: bool) -> bool {
    unsafe { plug(p) }.main_thread();
    !floating && !api.is_null() && unsafe { CStr::from_ptr(api) } == API
}

unsafe extern "C" fn gui_get_preferred_api(_p: *const clap_plugin, api: *mut *const c_char, floating: *mut bool) -> bool {
    unsafe {
        *api = API.as_ptr();
        *floating = false;
    }
    true
}

unsafe extern "C" fn gui_create(p: *const clap_plugin, api: *const c_char, floating: bool) -> bool {
    let x = unsafe { plug(p) };
    x.main_thread();
    if !unsafe { gui_is_api_supported(p, api, floating) } {
        return false;
    }
    let g = unsafe { gui(p) };
    *g = Gui { created: true, parent: 0, width: 320, height: 240, timer: None };
    if let Some(register) = unsafe { x.host_timer.as_ref() }.and_then(|t| t.register_timer) {
        let mut id = CLAP_INVALID_ID;
        if unsafe { register(x.host, 20, &mut id) } {
            g.timer = Some(id);
        }
    }
    if let Some(cb) = unsafe { (*x.host).request_callback } {
        unsafe { cb(x.host) };
    }
    true
}

unsafe extern "C" fn gui_destroy(p: *const clap_plugin) {
    let x = unsafe { plug(p) };
    x.main_thread();
    let g = unsafe { gui(p) };
    if let (Some(id), Some(unregister)) = (g.timer.take(), unsafe { x.host_timer.as_ref() }.and_then(|t| t.unregister_timer)) {
        unsafe { unregister(x.host, id) };
    }
    *g = Gui::default();
}

unsafe extern "C" fn gui_set_scale(_p: *const clap_plugin, _scale: f64) -> bool {
    false
}

unsafe extern "C" fn gui_get_size(p: *const clap_plugin, w: *mut u32, h: *mut u32) -> bool {
    unsafe { plug(p) }.main_thread();
    let g = unsafe { gui(p) };
    if !g.created {
        return false;
    }
    unsafe {
        *w = g.width;
        *h = g.height;
    }
    true
}

unsafe extern "C" fn gui_can_resize(p: *const clap_plugin) -> bool {
    unsafe { plug(p) }.main_thread();
    true
}

unsafe extern "C" fn gui_get_resize_hints(_p: *const clap_plugin, hints: *mut clap_gui_resize_hints) -> bool {
    unsafe { *hints = clap_gui_resize_hints { can_resize_horizontally: true, can_resize_vertically: true, preserve_aspect_ratio: false, aspect_ratio_width: 0, aspect_ratio_height: 0 } };
    true
}

unsafe extern "C" fn gui_adjust_size(p: *const clap_plugin, w: *mut u32, h: *mut u32) -> bool {
    unsafe { plug(p) }.main_thread();
    unsafe {
        *w = (*w).clamp(200, 1000);
        *h = (*h).clamp(150, 800);
    }
    true
}

unsafe extern "C" fn gui_set_size(p: *const clap_plugin, w: u32, h: u32) -> bool {
    unsafe { plug(p) }.main_thread();
    let g = unsafe { gui(p) };
    if !(200..=1000).contains(&w) || !(150..=800).contains(&h) {
        return false;
    }
    (g.width, g.height) = (w, h);
    true
}

unsafe extern "C" fn gui_set_parent(p: *const clap_plugin, window: *const clap_window) -> bool {
    unsafe { plug(p) }.main_thread();
    let Some(w) = (unsafe { window.as_ref() }) else { return false };
    if w.api.is_null() || unsafe { CStr::from_ptr(w.api) } != API {
        return false;
    }
    let parent = unsafe { w.specific.ptr } as usize;
    unsafe { gui(p) }.parent = parent;
    parent != 0
}

unsafe extern "C" fn gui_set_transient(_p: *const clap_plugin, _window: *const clap_window) -> bool {
    false
}

unsafe extern "C" fn gui_suggest_title(_p: *const clap_plugin, _title: *const c_char) {}

unsafe extern "C" fn gui_show(p: *const clap_plugin) -> bool {
    let x = unsafe { plug(p) };
    x.main_thread();
    if unsafe { gui(p) }.parent == 0 {
        return false;
    }
    // The user turns the gain knob: the host hears about it at the next flush or block.
    x.gain.set(1.8);
    x.pending_edit.store(true, Ordering::Release);
    if let Some(flush) = unsafe { x.host_params.as_ref() }.and_then(|h| h.request_flush) {
        unsafe { flush(x.host) };
    }
    if let Some(resize) = unsafe { x.host_gui.as_ref() }.and_then(|h| h.request_resize) {
        unsafe { resize(x.host, 400, 300) };
    }
    true
}

unsafe extern "C" fn gui_hide(p: *const clap_plugin) -> bool {
    unsafe { plug(p) }.main_thread();
    true
}

static GUI: clap_plugin_gui = clap_plugin_gui {
    is_api_supported: Some(gui_is_api_supported),
    get_preferred_api: Some(gui_get_preferred_api),
    create: Some(gui_create),
    destroy: Some(gui_destroy),
    set_scale: Some(gui_set_scale),
    get_size: Some(gui_get_size),
    can_resize: Some(gui_can_resize),
    get_resize_hints: Some(gui_get_resize_hints),
    adjust_size: Some(gui_adjust_size),
    set_size: Some(gui_set_size),
    set_parent: Some(gui_set_parent),
    set_transient: Some(gui_set_transient),
    suggest_title: Some(gui_suggest_title),
    show: Some(gui_show),
    hide: Some(gui_hide),
};

unsafe extern "C" fn on_timer(p: *const clap_plugin, id: clap_id) {
    let x = unsafe { plug(p) };
    x.main_thread();
    if unsafe { gui(p) }.timer == Some(id) {
        x.ticks.fetch_add(1, Ordering::Relaxed);
    }
}

static TIMER: clap_plugin_timer_support = clap_plugin_timer_support { on_timer: Some(on_timer) };

// ---- the factory and the entry -----------------------------------------------------------------

const DESCS: [&clap_plugin_descriptor; 3] = [&GAIN_DESC, &SYNTH_DESC, &THRU_DESC];

unsafe extern "C" fn factory_count(_f: *const clap_plugin_factory) -> u32 {
    DESCS.len() as u32
}

unsafe extern "C" fn factory_desc(_f: *const clap_plugin_factory, i: u32) -> *const clap_plugin_descriptor {
    DESCS.get(i as usize).map_or(std::ptr::null(), |d| *d as *const clap_plugin_descriptor)
}

unsafe extern "C" fn factory_create(_f: *const clap_plugin_factory, host: *const clap_host, id: *const c_char) -> *const clap_plugin {
    if host.is_null() || id.is_null() {
        return std::ptr::null();
    }
    let id = unsafe { CStr::from_ptr(id) };
    let (kind, desc) = match DESCS.iter().position(|d| unsafe { CStr::from_ptr(d.id) } == id) {
        Some(0) => (Kind::Gain, &GAIN_DESC),
        Some(1) => (Kind::Synth, &SYNTH_DESC),
        Some(2) => (Kind::Thru, &THRU_DESC),
        _ => return std::ptr::null(),
    };
    let plug = Box::into_raw(Box::new(Plug {
        plugin: clap_plugin {
            desc,
            plugin_data: std::ptr::null_mut(),
            init: Some(init),
            destroy: Some(destroy),
            activate: Some(activate),
            deactivate: Some(deactivate),
            start_processing: Some(start_processing),
            stop_processing: Some(stop_processing),
            reset: Some(reset),
            process: Some(process),
            get_extension: Some(get_extension),
            on_main_thread: Some(on_main_thread),
        },
        kind,
        host,
        host_params: std::ptr::null(),
        host_gui: std::ptr::null(),
        host_timer: std::ptr::null(),
        host_check: std::ptr::null(),
        host_latency: std::ptr::null(),
        gain: Value::new(1.0),
        mode: Value::new(0.0),
        hidden: Value::new(0.0),
        bypass: Value::new(0.0),
        freq: Value::new(1_000.0),
        active: AtomicBool::new(false),
        told_latency: AtomicU32::new(3),
        ports_off: AtomicU32::new(0),
        ticks: AtomicU32::new(0),
        callbacks: AtomicU32::new(0),
        errors: AtomicU32::new(0),
        main_tid: AtomicUsize::new(0),
        audio_tid: AtomicUsize::new(0),
        processing: AtomicBool::new(false),
        in_audio: AtomicU32::new(0),
        rate: Value::new(0.0),
        pending_edit: AtomicBool::new(false),
        level: UnsafeCell::new(0.0),
        gui: UnsafeCell::new(Gui::default()),
    }));
    unsafe {
        (*plug).plugin.plugin_data = plug.cast();
        &(*plug).plugin
    }
}

static FACTORY: clap_plugin_factory = clap_plugin_factory { get_plugin_count: Some(factory_count), get_plugin_descriptor: Some(factory_desc), create_plugin: Some(factory_create) };

unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
    MODULE_INITS.fetch_add(1, Ordering::AcqRel);
    true
}

unsafe extern "C" fn entry_deinit() {
    MODULE_INITS.fetch_sub(1, Ordering::AcqRel);
}

unsafe extern "C" fn entry_get_factory(id: *const c_char) -> *const c_void {
    if !id.is_null() && unsafe { CStr::from_ptr(id) } == CLAP_PLUGIN_FACTORY_ID {
        (&FACTORY as *const clap_plugin_factory).cast()
    } else {
        std::ptr::null()
    }
}

#[allow(non_upper_case_globals)]
#[unsafe(no_mangle)]
pub static clap_entry: clap_plugin_entry = clap_plugin_entry { clap_version: CLAP_VERSION, init: Some(entry_init), deinit: Some(entry_deinit), get_factory: Some(entry_get_factory) };
