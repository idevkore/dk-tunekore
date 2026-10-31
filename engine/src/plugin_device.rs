//! A hosted plugin on a track (#37): an instrument in a track's source slot,
//! or an effect in its chain.
//!
//! A loaded plugin has two halves (`tunekore_plugins`): the controller, which
//! the shell keeps on the main thread (state, parameters, the editor), and the
//! audio-thread [`Plugin`], which this device runs. Activation belongs to the
//! main thread (VST3 wants it on the UI thread), so the device never
//! activates anything itself:
//!
//! - [`Device::prepare`] (on whatever thread holds the engine) records the
//!   rate and, unless it already has a plugin activated at that rate, asks the
//!   shell for one through its [`PluginLink`].
//! - The shell activates the controller and hands the new `Plugin` over with
//!   [`PluginLink::give`]. The device takes it at the top of a block and sends
//!   the one it replaces back on a queue, to be dropped on the main thread
//!   ([`PluginLink::collect`]). The same path carries a plugin reactivated for
//!   a new latency or a reload.
//! - Until a plugin at the right rate arrives, an instrument is silent and an
//!   effect passes its input through.
//!
//! Notes reach an instrument from the track's [`Events`] (clips, later) and
//! from the link's live queue ([`PluginLink::send_live`]: a MIDI controller),
//! which the device drains at the top of each block.
//!
//! [`Device::process`] allocates, locks and logs nothing: the channel tables,
//! scratch buffers and event lists are made in [`plugin_device`].

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use rtrb::{Consumer, Producer, RingBuffer};
use tunekore_plugins::{ChannelTable, Event as PluginEvent, EventKind, OutBuf, ParamChange, Plugin, ProcessBlock, ProcessError, Transport as PluginTransport};

use crate::device::{Buffers, Device, Event, Events};
use crate::graph::MAX_FRAMES;
use crate::snapshot::Mailbox;
use crate::transport::ProcessContext;

/// The most channels a plugin's main buses may have here. A plugin with more
/// stays silent (stereo is what activation asks for).
pub const MAX_CHANNELS: usize = 8;
/// Notes and MIDI one block can carry into a plugin.
const PLUGIN_EVENTS: usize = 1024;
/// Live notes waiting between the MIDI thread and the next block.
const LIVE_EVENTS: usize = 512;
/// Replaced plugins waiting to be dropped on the main thread.
const RETURNS: usize = 8;
/// What the plugin sends out (notes, its own parameter moves), kept for later.
const OUT_EVENTS: usize = 256;

/// Where the device sits on its track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// In the source slot: plays the track's notes and adds its sound.
    Instrument,
    /// In the chain: changes the sound in place.
    Effect,
}

/// A plugin activated for the device, and the rate it was activated at.
pub struct Activated {
    pub plugin: Box<dyn Plugin>,
    pub rate: u32,
}

/// The main thread's handle on a [`PluginDevice`]: hands it activated plugins
/// and live notes, and reads what it reports. Shared; the audio thread only
/// touches its atomics and its mailbox.
pub struct PluginLink {
    role: Role,
    incoming: Mailbox<Activated>,
    returned: Mutex<Consumer<Box<Activated>>>,
    live: Mutex<Producer<PluginEvent>>,
    /// The rate the device was last prepared for; 0 before.
    rate: AtomicU32,
    bypass: AtomicBool,
    /// Set when the plugin failed while processing: it is switched off for good.
    failed: AtomicBool,
    /// Whether the device holds a plugin activated at its rate.
    ready: AtomicBool,
    /// Asks the shell for a plugin activated at [`rate`](Self::rate). Called
    /// from `prepare`, maybe while the engine's lock is held: it must not
    /// block (the shell queues the work for its main thread).
    request: Box<dyn Fn() + Send + Sync>,
}

impl PluginLink {
    pub fn role(&self) -> Role {
        self.role
    }

    /// Main thread: hands the device `plugin`, activated at `rate`. A plugin
    /// handed over before and never taken is dropped here.
    pub fn give(&self, plugin: Box<dyn Plugin>, rate: u32) {
        drop(self.incoming.put(Box::new(Activated { plugin, rate })));
    }

    /// Main thread: drops the plugins the device replaced.
    pub fn collect(&self) {
        let mut q = self.returned.lock().unwrap_or_else(|e| e.into_inner());
        while q.pop().is_ok() {}
    }

    /// The rate the device runs at, which a plugin handed to it should be
    /// activated at; 0 while it has never been prepared.
    pub fn rate(&self) -> u32 {
        self.rate.load(Ordering::Acquire)
    }

    /// Whether the device has a plugin activated at its rate.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Off: an instrument goes silent (its held notes are released first), an effect passes its input through.
    pub fn set_bypass(&self, on: bool) {
        self.bypass.store(on, Ordering::Relaxed);
    }

    pub fn bypass(&self) -> bool {
        self.bypass.load(Ordering::Relaxed)
    }

    /// Whether the plugin failed while processing and is switched off.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Clears the failed flag, once a fresh plugin is on its way (a reload).
    pub fn clear_failed(&self) {
        self.failed.store(false, Ordering::Relaxed);
    }

    /// Any thread but the audio thread: a note or MIDI message to play now
    /// (an instrument's only; an effect ignores them). Returns false when the
    /// queue is full and it was dropped.
    pub fn send_live(&self, kind: EventKind) -> bool {
        let mut q = self.live.lock().unwrap_or_else(|e| e.into_inner());
        q.push(PluginEvent { offset: 0, kind }).is_ok()
    }
}

/// A plugin as a [`Device`]. Made with [`plugin_device`].
pub struct PluginDevice {
    link: Arc<PluginLink>,
    current: Option<Box<Activated>>,
    returns: Producer<Box<Activated>>,
    live: Consumer<PluginEvent>,
    rate: u32,
    table: ChannelTable,
    scratch_in: Vec<Vec<f32>>,
    scratch_out: Vec<Vec<f32>>,
    events: Vec<PluginEvent>,
    midi_out: OutBuf<PluginEvent>,
    param_out: OutBuf<ParamChange>,
    /// Notes held, a bit per key for each channel, so they can be released.
    held: [u128; 16],
    was_bypassed: bool,
}

/// A device for a plugin in `role`, and the link the main thread keeps.
/// `request` asks for a plugin activated at the link's rate (see
/// [`PluginLink`]); it must not block. Off the audio thread: it allocates.
pub fn plugin_device(role: Role, request: Box<dyn Fn() + Send + Sync>) -> (PluginDevice, Arc<PluginLink>) {
    let (returns, returned) = RingBuffer::new(RETURNS);
    let (live_tx, live) = RingBuffer::new(LIVE_EVENTS);
    let link = Arc::new(PluginLink {
        role,
        incoming: Mailbox::new(),
        returned: Mutex::new(returned),
        live: Mutex::new(live_tx),
        rate: AtomicU32::new(0),
        bypass: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        ready: AtomicBool::new(false),
        request,
    });
    let device = PluginDevice {
        link: link.clone(),
        current: None,
        returns,
        live,
        rate: 0,
        table: ChannelTable::with_capacity(MAX_CHANNELS, MAX_CHANNELS),
        scratch_in: (0..MAX_CHANNELS).map(|_| vec![0.0; MAX_FRAMES]).collect(),
        scratch_out: (0..MAX_CHANNELS).map(|_| vec![0.0; MAX_FRAMES]).collect(),
        events: Vec::with_capacity(PLUGIN_EVENTS),
        midi_out: OutBuf::with_capacity(OUT_EVENTS),
        param_out: OutBuf::with_capacity(OUT_EVENTS),
        held: [0; 16],
        was_bypassed: false,
    };
    (device, link)
}

/// The engine's clock as the plugin's transport (the table in
/// `tunekore_plugins::Transport`'s notes).
pub fn plugin_transport(ctx: &ProcessContext) -> PluginTransport {
    PluginTransport {
        playing: ctx.playing,
        recording: false,
        looping: ctx.looping,
        tempo: ctx.tempo,
        time_sig_numerator: ctx.time_signature.numerator,
        time_sig_denominator: ctx.time_signature.denominator,
        project_samples: ctx.frame as i64,
        continuous_samples: ctx.continuous_frame as i64,
        project_beats: ctx.beat,
        bar_start_beats: ctx.bar_start,
        loop_start_beats: ctx.loop_range.start,
        loop_end_beats: ctx.loop_range.end,
    }
}

/// A track event as the plugin's.
fn to_plugin(e: &Event) -> PluginEvent {
    let kind = match *e {
        Event::NoteOn { channel, key, velocity, .. } => EventKind::NoteOn { channel, key, velocity, note_id: -1 },
        Event::NoteOff { channel, key, velocity, .. } => EventKind::NoteOff { channel, key, velocity, note_id: -1 },
        Event::Control { channel, controller, value, .. } => EventKind::ControlChange { channel, controller, value },
    };
    PluginEvent { offset: e.frame(), kind }
}

impl PluginDevice {
    pub fn link(&self) -> &Arc<PluginLink> {
        &self.link
    }

    /// Main thread, before the device goes on a track: the plugin it starts
    /// with, activated at `rate`. Saves a second activation when the device
    /// is prepared at that rate.
    pub fn install(&mut self, plugin: Box<dyn Plugin>, rate: u32) {
        self.current = Some(Box::new(Activated { plugin, rate }));
        self.publish_ready();
    }

    /// Takes a newly activated plugin, if one is waiting and the old one has
    /// somewhere to go.
    fn swap_in(&mut self) {
        if self.returns.slots() == 0 {
            return;
        }
        let Some(next) = self.link.incoming.take() else { return };
        if let Some(old) = self.current.replace(next) {
            // Room was checked above; never dropped here.
            if let Err(rtrb::PushError::Full(old)) = self.returns.push(old) {
                std::mem::forget(old);
            }
        }
        // A new instance holds no notes.
        self.held = [0; 16];
        self.publish_ready();
    }

    fn publish_ready(&self) {
        let ready = self.current.as_ref().is_some_and(|a| a.rate == self.rate && self.rate != 0);
        self.link.ready.store(ready, Ordering::Release);
    }

    fn note(&mut self, kind: &EventKind) {
        match *kind {
            EventKind::NoteOn { channel, key, .. } => self.held[usize::from(channel & 15)] |= 1u128 << (key & 127),
            EventKind::NoteOff { channel, key, .. } => self.held[usize::from(channel & 15)] &= !(1u128 << (key & 127)),
            _ => {}
        }
    }

    /// Adds a note-off for every held note, at the block's start.
    fn release_held(&mut self) {
        for channel in 0..16u8 {
            let mut keys = self.held[usize::from(channel)];
            while keys != 0 && self.events.len() < self.events.capacity() {
                let key = keys.trailing_zeros() as u8;
                keys &= keys - 1;
                self.events.push(PluginEvent { offset: 0, kind: EventKind::NoteOff { channel, key, velocity: 0.0, note_id: -1 } });
            }
        }
        self.held = [0; 16];
    }

    /// Gathers this block's notes: live ones first (at its start), then the track's.
    fn gather(&mut self, events: &Events) {
        self.events.clear();
        if self.link.role == Role::Instrument {
            while self.events.len() < self.events.capacity() {
                let Ok(e) = self.live.pop() else { break };
                self.note(&e.kind);
                self.events.push(e);
            }
        }
        for e in events.as_slice() {
            if self.events.len() == self.events.capacity() {
                break;
            }
            let pe = to_plugin(e);
            self.note(&pe.kind);
            self.events.push(pe);
        }
    }

    /// Runs the plugin over `n` frames of the scratch buffers. False when it
    /// made no sound: no plugin at this rate, a stale one, or a failure.
    fn run(&mut self, ctx: &ProcessContext, n: usize) -> bool {
        let PluginDevice { current, table, scratch_in, scratch_out, events, midi_out, param_out, link, rate, .. } = self;
        let Some(act) = current.as_mut().filter(|a| a.rate == *rate) else { return false };
        let layout = act.plugin.layout();
        let (n_in, n_out) = (layout.input_channels(), layout.output_channels());
        if n_in > MAX_CHANNELS || n_out > MAX_CHANNELS || n_out == 0 {
            return false;
        }
        let transport = plugin_transport(ctx);
        midi_out.clear();
        param_out.clear();
        let (inputs, outputs) = table.audio(&scratch_in[..n_in], &mut scratch_out[..n_out]);
        let mut block = ProcessBlock { frames: n, inputs, outputs, events, param_changes: &[], transport: &transport, midi_out, param_out };
        match act.plugin.process(&mut block) {
            Ok(()) => true,
            Err(ProcessError::Failed) => {
                link.failed.store(true, Ordering::Relaxed);
                false
            }
            Err(_) => false,
        }
    }
}

impl Device for PluginDevice {
    /// Records the rate and, unless the plugin is already activated at it,
    /// asks the main thread for one that is. Activates nothing itself.
    fn prepare(&mut self, sample_rate: u32, _max_frames: usize) {
        self.rate = sample_rate;
        self.link.rate.store(sample_rate, Ordering::Release);
        self.publish_ready();
        if !self.link.is_ready() {
            (self.link.request)();
        }
    }

    fn process(&mut self, ctx: &ProcessContext, events: &mut Events, audio: Buffers<'_>) {
        self.swap_in();
        let n = audio.frames().min(ctx.frames).min(MAX_FRAMES);
        if n == 0 {
            return;
        }
        let instrument = self.link.role == Role::Instrument;
        self.gather(events);
        if self.link.bypass.load(Ordering::Relaxed) {
            // An instrument switched off lets go of its notes first, so none hang when it comes back.
            if instrument && !self.was_bypassed && self.held.iter().any(|&k| k != 0) {
                self.events.clear();
                self.release_held();
                for ch in &mut self.scratch_in {
                    ch[..n].fill(0.0);
                }
                self.run(ctx, n);
            }
            self.was_bypassed = true;
            return;
        }
        self.was_bypassed = false;

        // Inputs: the track's sound (silence for an instrument in the source slot).
        let n_in = self.current.as_ref().map_or(0, |a| a.plugin.layout().input_channels()).min(MAX_CHANNELS);
        for (i, ch) in self.scratch_in.iter_mut().enumerate().take(n_in) {
            let dst = &mut ch[..n];
            match (i, n_in) {
                (_, 1) => dst.iter_mut().zip(audio.left.iter().zip(audio.right.iter())).for_each(|(d, (l, r))| *d = 0.5 * (l + r)),
                (0, _) => dst.copy_from_slice(&audio.left[..n]),
                (1, _) => dst.copy_from_slice(&audio.right[..n]),
                _ => dst.fill(0.0),
            }
        }
        if !self.run(ctx, n) {
            // Silent: an instrument adds nothing, and an effect leaves its input as it was.
            return;
        }
        let n_out = self.current.as_ref().map_or(0, |a| a.plugin.layout().output_channels());
        let (l, r) = if n_out >= 2 { (&self.scratch_out[0][..n], &self.scratch_out[1][..n]) } else { (&self.scratch_out[0][..n], &self.scratch_out[0][..n]) };
        if instrument {
            audio.left[..n].iter_mut().zip(l).for_each(|(d, s)| *d += s);
            audio.right[..n].iter_mut().zip(r).for_each(|(d, s)| *d += s);
        } else {
            audio.left[..n].copy_from_slice(l);
            audio.right[..n].copy_from_slice(r);
        }
    }

    fn latency(&self) -> usize {
        self.current.as_ref().filter(|a| a.rate == self.rate).map_or(0, |a| a.plugin.latency_samples() as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::realtime;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use tunekore_plugins::{BusInfo, BusKind, BusLayout, Category, Dropped, Format, PluginInfo};

    const RATE: u32 = 48_000;

    /// What a fake plugin saw, in atomics (it runs inside the real-time checks).
    #[derive(Default)]
    struct Seen {
        blocks: AtomicUsize,
        tempo: AtomicU64,
        /// The last note event: offset, channel and key, and 1 for on or 2 for off.
        last: AtomicU64,
    }

    fn pack(offset: u32, kind: &EventKind) -> u64 {
        match *kind {
            EventKind::NoteOn { channel, key, .. } => (1 << 48) | (u64::from(offset) << 16) | (u64::from(channel) << 8) | u64::from(key),
            EventKind::NoteOff { channel, key, .. } => (2 << 48) | (u64::from(offset) << 16) | (u64::from(channel) << 8) | u64::from(key),
            _ => 0,
        }
    }

    /// A stand-in plugin: an instrument that outputs 0.5 while a note is held,
    /// or an effect that doubles its input; `fail` makes it fail as a plugin
    /// that threw.
    struct Fake {
        info: PluginInfo,
        layout: BusLayout,
        effect: bool,
        on: bool,
        fail: bool,
        seen: Arc<Seen>,
    }

    fn bus(channels: u32) -> BusInfo {
        BusInfo { name: "main".into(), channels, kind: BusKind::Main, active: true }
    }

    fn make(effect: bool, fail: bool, seen: Arc<Seen>) -> Box<dyn Plugin> {
        let layout = BusLayout { audio_inputs: if effect { vec![bus(2)] } else { vec![] }, audio_outputs: vec![bus(2)], event_inputs: 1, event_outputs: 0 };
        let info = PluginInfo {
            format: Format::Vst3,
            name: "Fake".into(),
            vendor: "devKore".into(),
            version: "1".into(),
            category: if effect { Category::Effect } else { Category::Instrument },
            sub_categories: vec![],
            class_id: "0".repeat(32),
            path: "/fake.vst3".into(),
        };
        Box::new(Fake { info, layout, effect, on: false, fail, seen })
    }

    fn fake(effect: bool, seen: Arc<Seen>) -> Box<dyn Plugin> {
        make(effect, false, seen)
    }

    impl Plugin for Fake {
        fn info(&self) -> &PluginInfo {
            &self.info
        }
        fn layout(&self) -> &BusLayout {
            &self.layout
        }
        fn latency_samples(&self) -> u32 {
            64
        }
        fn process(&mut self, block: &mut ProcessBlock<'_>) -> Result<(), ProcessError> {
            if self.fail {
                return Err(ProcessError::Failed);
            }
            self.seen.blocks.fetch_add(1, Ordering::Relaxed);
            self.seen.tempo.store(block.transport.tempo.to_bits(), Ordering::Relaxed);
            for e in block.events {
                self.seen.last.store(pack(e.offset, &e.kind), Ordering::Relaxed);
                match e.kind {
                    EventKind::NoteOn { .. } => self.on = true,
                    EventKind::NoteOff { .. } => self.on = false,
                    _ => {}
                }
            }
            for c in 0..block.outputs.channels() {
                // SAFETY: the block's outputs are valid for `frames` writes and its inputs for reads.
                let out = unsafe { std::slice::from_raw_parts_mut(block.outputs.as_ptrs()[c], block.frames) };
                if self.effect {
                    let inp = unsafe { std::slice::from_raw_parts(block.inputs.as_ptrs()[c], block.frames) };
                    out.iter_mut().zip(inp).for_each(|(o, i)| *o = 2.0 * i);
                } else {
                    out.fill(if self.on { 0.5 } else { 0.0 });
                }
            }
            Ok(())
        }
        fn take_dropped(&mut self) -> Dropped {
            Dropped::default()
        }
    }

    fn requests() -> (Arc<AtomicUsize>, Box<dyn Fn() + Send + Sync>) {
        let n = Arc::new(AtomicUsize::new(0));
        let c = n.clone();
        (n, Box::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
        }))
    }

    fn on(key: u8) -> EventKind {
        EventKind::NoteOn { channel: 0, key, velocity: 1.0, note_id: -1 }
    }

    /// Runs one block with `input` already in the track's buffers; returns the
    /// left side and the heap calls the audio thread made.
    fn block(device: &mut PluginDevice, ctx: &ProcessContext, events: &mut Events, input: f32) -> (Vec<f32>, usize) {
        let mut l = vec![input; ctx.frames];
        let mut r = vec![input; ctx.frames];
        let (_, allocs) = realtime(|| device.process(ctx, events, Buffers { left: &mut l, right: &mut r }));
        assert_eq!(l, r);
        (l, allocs)
    }

    #[test]
    fn prepare_asks_for_a_plugin_at_its_rate_and_the_device_waits_for_it_in_silence() {
        let (asked, request) = requests();
        let (mut d, link) = plugin_device(Role::Instrument, request);
        d.prepare(RATE, MAX_FRAMES);
        assert_eq!((asked.load(Ordering::Relaxed), link.rate()), (1, RATE));
        assert!(!link.is_ready());
        let ctx = ProcessContext::stopped(RATE, 64);
        let mut ev = Events::with_capacity(8);
        assert!(link.send_live(on(60)));
        let (out, allocs) = block(&mut d, &ctx, &mut ev, 0.0);
        assert_eq!(allocs, 0);
        assert!(out.iter().all(|&x| x == 0.0), "no plugin yet");

        // A plugin at another rate is not played; one at the device's rate is.
        let s = Arc::new(Seen::default());
        link.give(fake(false, s.clone()), 44_100);
        let (out, _) = block(&mut d, &ctx, &mut ev, 0.0);
        assert!(out.iter().all(|&x| x == 0.0) && !link.is_ready());
        link.give(fake(false, s.clone()), RATE);
        assert!(link.send_live(on(60)));
        let (out, allocs) = block(&mut d, &ctx, &mut ev, 0.0);
        assert_eq!(allocs, 0, "swapping a plugin in allocates and frees nothing");
        assert!(link.is_ready());
        assert!(out.iter().all(|&x| x == 0.5), "{:?}", &out[..4]);
        link.collect();
        // Prepared again at the same rate: nothing more to ask for.
        d.prepare(RATE, MAX_FRAMES);
        assert_eq!(asked.load(Ordering::Relaxed), 1);
        d.prepare(96_000, MAX_FRAMES);
        assert_eq!(asked.load(Ordering::Relaxed), 2, "a new rate needs a new activation");
        assert_eq!(d.latency(), 0, "no latency counted for a plugin at the wrong rate");
    }

    #[test]
    fn a_plugin_installed_at_the_rate_it_is_prepared_for_needs_no_second_activation() {
        let (asked, request) = requests();
        let (mut d, link) = plugin_device(Role::Effect, request);
        d.install(fake(true, Arc::default()), RATE);
        d.prepare(RATE, MAX_FRAMES);
        assert_eq!(asked.load(Ordering::Relaxed), 0);
        assert!(link.is_ready());
        let ctx = ProcessContext::stopped(RATE, 16);
        let (out, allocs) = block(&mut d, &ctx, &mut Events::with_capacity(4), 0.5);
        assert_eq!(allocs, 0);
        assert!(out.iter().all(|&x| x == 1.0));
    }

    #[test]
    fn an_instrument_adds_to_the_track_and_gets_its_notes_and_the_transport() {
        let (_, request) = requests();
        let (mut d, link) = plugin_device(Role::Instrument, request);
        d.prepare(RATE, MAX_FRAMES);
        let s = Arc::new(Seen::default());
        link.give(fake(false, s.clone()), RATE);
        let mut ctx = ProcessContext::stopped(RATE, 32);
        ctx.tempo = 133.0;
        let mut ev = Events::with_capacity(8);
        ev.push(Event::NoteOn { frame: 5, channel: 2, key: 64, velocity: 0.5 });
        let (out, allocs) = block(&mut d, &ctx, &mut ev, 0.25);
        assert_eq!(allocs, 0);
        assert!(out.iter().all(|&x| x == 0.75), "0.25 already there plus the plugin's 0.5");
        assert_eq!(f64::from_bits(s.tempo.load(Ordering::Relaxed)), 133.0);
        assert_eq!(s.last.load(Ordering::Relaxed), pack(5, &EventKind::NoteOn { channel: 2, key: 64, velocity: 0.5, note_id: -1 }));
        assert_eq!(d.latency(), 64);
    }

    #[test]
    fn switching_an_instrument_off_releases_its_held_notes_then_silences_it() {
        let (_, request) = requests();
        let (mut d, link) = plugin_device(Role::Instrument, request);
        d.prepare(RATE, MAX_FRAMES);
        let s = Arc::new(Seen::default());
        link.give(fake(false, s.clone()), RATE);
        let ctx = ProcessContext::stopped(RATE, 16);
        let mut ev = Events::with_capacity(8);
        link.send_live(on(48));
        block(&mut d, &ctx, &mut ev, 0.0);
        link.set_bypass(true);
        let (out, allocs) = block(&mut d, &ctx, &mut ev, 0.0);
        assert_eq!(allocs, 0);
        assert!(out.iter().all(|&x| x == 0.0));
        assert_eq!(s.last.load(Ordering::Relaxed), pack(0, &EventKind::NoteOff { channel: 0, key: 48, velocity: 0.0, note_id: -1 }));
        let blocks = s.blocks.load(Ordering::Relaxed);
        block(&mut d, &ctx, &mut ev, 0.0);
        assert_eq!(s.blocks.load(Ordering::Relaxed), blocks, "not run while off");
        link.set_bypass(false);
        let (out, _) = block(&mut d, &ctx, &mut ev, 0.0);
        assert!(out.iter().all(|&x| x == 0.0), "back on, with no note hanging");
    }

    #[test]
    fn an_effect_changes_the_sound_in_place_and_passes_it_through_when_off_or_failed() {
        let (_, request) = requests();
        let (mut d, link) = plugin_device(Role::Effect, request);
        d.prepare(RATE, MAX_FRAMES);
        let ctx = ProcessContext::stopped(RATE, 64);
        let mut ev = Events::with_capacity(8);
        let (out, _) = block(&mut d, &ctx, &mut ev, 0.3);
        assert!(out.iter().all(|&x| x == 0.3), "no plugin yet: dry");
        link.give(fake(true, Arc::default()), RATE);
        let (out, allocs) = block(&mut d, &ctx, &mut ev, 0.3);
        assert_eq!(allocs, 0);
        assert!(out.iter().all(|&x| (x - 0.6).abs() < 1e-6));
        link.set_bypass(true);
        let (out, _) = block(&mut d, &ctx, &mut ev, 0.3);
        assert!(out.iter().all(|&x| x == 0.3), "off: dry");
        link.set_bypass(false);
        link.give(make(true, true, Arc::default()), RATE);
        let (out, allocs) = block(&mut d, &ctx, &mut ev, 0.3);
        assert_eq!(allocs, 0);
        assert!(out.iter().all(|&x| x == 0.3), "failed: dry");
        assert!(link.failed());
        link.collect();
    }

    #[test]
    fn replaced_plugins_wait_for_the_main_thread_and_a_full_return_queue_holds_the_next_back() {
        let (_, request) = requests();
        let (mut d, link) = plugin_device(Role::Effect, request);
        d.prepare(RATE, MAX_FRAMES);
        let ctx = ProcessContext::stopped(RATE, 8);
        let mut ev = Events::with_capacity(8);
        // Swap more plugins in than the return queue holds, without collecting.
        for _ in 0..RETURNS + 3 {
            link.give(fake(true, Arc::default()), RATE);
            let (out, allocs) = block(&mut d, &ctx, &mut ev, 0.1);
            assert_eq!(allocs, 0);
            assert!(out.iter().all(|&x| (x - 0.2).abs() < 1e-6), "the current one keeps playing");
        }
        link.collect();
        let (out, _) = block(&mut d, &ctx, &mut ev, 0.1);
        assert!(out.iter().all(|&x| (x - 0.2).abs() < 1e-6));
    }

    #[test]
    fn the_transport_maps_onto_the_plugins() {
        let mut ctx = ProcessContext::stopped(RATE, 64);
        ctx.playing = true;
        ctx.beat = 9.5;
        ctx.bar_start = 8.0;
        ctx.frame = 12_345;
        ctx.continuous_frame = 99_999;
        ctx.looping = true;
        ctx.loop_range = crate::transport::LoopRange { start: 4.0, end: 12.0 };
        let t = plugin_transport(&ctx);
        assert!(t.playing && t.looping && !t.recording);
        assert_eq!((t.project_beats, t.bar_start_beats, t.project_samples, t.continuous_samples), (9.5, 8.0, 12_345, 99_999));
        assert_eq!((t.loop_start_beats, t.loop_end_beats, t.time_sig_numerator, t.time_sig_denominator), (4.0, 12.0, 4, 4));
    }
}
