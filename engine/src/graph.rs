//! The audio graph: every track's devices, then its volume, pan, mute, solo
//! and meter, summed into the master.
//!
//! [`Mixer`] is the main thread's side. It owns the track list (the
//! document) and, while no output is open, the devices. After each edit it
//! puts an immutable [`Snapshot`] of the tracks in a latest-wins mailbox
//! ([`crate::snapshot`]). A snapshot also carries the devices to install
//! with it, prepared on the main thread, so the audio thread never runs a
//! track list ahead of its devices. Removed devices and old snapshots come
//! back to the main thread on a return queue to be dropped.
//!
//! [`Graph`] is the audio thread's side. Each block it takes the newest
//! snapshot (installing its devices), and runs the tracks over each piece of
//! the block the transport hands it. When the output closes, the graph is
//! dropped off the audio thread and sends every device home to the mixer,
//! which prepares them again for the next output.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::device::{Buffers, Device, Events};
use crate::snapshot::Mailbox;
use crate::track::{TrackId, TrackInfo, TrackKind, TrackMeter, TrackMeters, TrackMix, TRACK_SLOTS};
use crate::transport::{ProcessContext, Transport, TransportShared};

/// The most devices the engine holds at once, over all tracks.
pub const DEVICE_SLOTS: usize = 256;
/// The longest block the graph runs. The engine splits longer callbacks.
pub const MAX_FRAMES: usize = 4096;
/// Events one track can carry through one piece of a block.
pub const TRACK_EVENTS: usize = 1024;
/// A volume, pan, mute or solo change takes this long, so it never clicks.
const RAMP_SECONDS: f32 = 0.008;
/// How long [`Mixer::stopped`] waits for the devices to come home.
const HOME_WAIT: Duration = Duration::from_millis(500);
/// Room on the way back: every device and a snapshot, twice over.
const RETURN_QUEUE: usize = DEVICE_SLOTS * 2 + 8;

type Slots = Vec<Option<Box<dyn Device>>>;

fn empty_slots() -> Slots {
    (0..DEVICE_SLOTS).map(|_| None).collect()
}

/// A device for a slot, or `None` to empty it.
pub struct Install {
    slot: usize,
    device: Option<Box<dyn Device>>,
}

/// Adds an install to a list, replacing one already there for the same slot
/// (whose device, never installed, is dropped here on the main thread). So a
/// list holds at most one install per slot.
fn stage(list: &mut Vec<Install>, slot: usize, device: Option<Box<dyn Device>>) {
    match list.iter_mut().find(|i| i.slot == slot) {
        Some(i) => i.device = device,
        None => list.push(Install { slot, device }),
    }
}

/// The audio thread's picture of the tracks, in mix order, and the devices
/// to install before it runs. Built on the main thread after each edit.
#[derive(Default)]
pub struct Snapshot {
    tracks: Vec<TrackSnap>,
    installs: Vec<Install>,
}

#[derive(Debug, Clone)]
struct TrackSnap {
    id: TrackId,
    /// Index into the graph's per-track state and the meters.
    slot: usize,
    source: Option<usize>,
    chain: Vec<usize>,
    /// Left and right gain with volume, pan, mute and solo applied.
    gains: [f32; 2],
}

/// What the audio thread hands back to be dropped on the main thread. The
/// boxes are only ever dropped, never read.
#[allow(dead_code)]
enum Garbage {
    Snapshot(Box<Snapshot>),
    Device(Box<dyn Device>),
}

struct TrackDoc {
    id: TrackId,
    slot: usize,
    name: String,
    kind: TrackKind,
    mix: TrackMix,
    source: Option<usize>,
    chain: Vec<usize>,
}

/// The mixer's ends of a running graph.
struct Link {
    rate: u32,
    mailbox: Arc<Mailbox<Snapshot>>,
    returned: Consumer<Garbage>,
    home: mpsc::Receiver<Slots>,
}

/// The main thread's track list and devices. Call it from one thread at a time.
pub struct Mixer {
    tracks: Vec<TrackDoc>,
    next_id: u32,
    /// The devices, while no graph runs. While one runs, they are on the audio thread.
    devices: Slots,
    taken: Vec<bool>,
    /// Devices to install with the next snapshot, at most one per slot.
    pending: Vec<Install>,
    meters: Arc<TrackMeters>,
    revision: Arc<AtomicU64>,
    link: Option<Link>,
}

impl Default for Mixer {
    fn default() -> Self {
        Self::new()
    }
}

impl Mixer {
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            next_id: 1,
            devices: empty_slots(),
            taken: vec![false; DEVICE_SLOTS],
            pending: Vec::new(),
            meters: Arc::default(),
            revision: Arc::default(),
            link: None,
        }
    }

    /// Bumped by every edit, so the project knows when it has changed.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    /// The revision, readable from any thread without the mixer.
    pub fn revision_handle(&self) -> Arc<AtomicU64> {
        self.revision.clone()
    }

    /// Adds an empty track at the end of the list.
    pub fn add_track(&mut self, name: &str) -> Result<TrackId, String> {
        let id = TrackId(self.next_id);
        self.add_track_as(id, name)?;
        Ok(id)
    }

    /// Adds an empty track with a given id, as a project names it.
    pub fn add_track_as(&mut self, id: TrackId, name: &str) -> Result<(), String> {
        if self.tracks.iter().any(|t| t.id == id) {
            return Err(format!("there is already a track {}", id.0));
        }
        let slot = (0..TRACK_SLOTS)
            .find(|s| self.tracks.iter().all(|t| t.slot != *s))
            .ok_or(format!("tuneKore runs at most {TRACK_SLOTS} tracks"))?;
        self.next_id = self.next_id.max(id.0.saturating_add(1));
        self.tracks.push(TrackDoc { id, slot, name: name.to_string(), kind: TrackKind::default(), mix: TrackMix::default(), source: None, chain: Vec::new() });
        self.changed();
        Ok(())
    }

    /// The id the next new track gets. Projects save it, so a deleted
    /// track's id never comes back for another track after a relaunch.
    pub fn next_track_id(&self) -> TrackId {
        TrackId(self.next_id)
    }

    /// New tracks get ids from `next` on (as a project saved it), or from
    /// past the highest track there is when that is higher. Opening and new
    /// projects set it, so it belongs to the project, not the app's session.
    pub fn set_next_track_id(&mut self, next: TrackId) {
        let highest = self.tracks.iter().map(|t| t.id.0).max().unwrap_or(0);
        self.next_id = next.0.max(highest.saturating_add(1)).max(1);
    }

    /// Removes a track and its devices.
    pub fn remove_track(&mut self, id: TrackId) -> Result<(), String> {
        let i = self.index(id)?;
        let t = self.tracks.remove(i);
        for slot in t.source.into_iter().chain(t.chain) {
            self.free(slot);
        }
        self.changed();
        Ok(())
    }

    pub fn tracks(&self) -> Vec<TrackInfo> {
        self.tracks.iter().map(info).collect()
    }

    pub fn track(&self, id: TrackId) -> Option<TrackInfo> {
        self.tracks.iter().find(|t| t.id == id).map(info)
    }

    pub fn set_mix(&mut self, id: TrackId, mix: TrackMix) -> Result<(), String> {
        let i = self.index(id)?;
        self.tracks[i].mix = mix.sane();
        self.changed();
        Ok(())
    }

    pub fn rename(&mut self, id: TrackId, name: &str) -> Result<(), String> {
        let i = self.index(id)?;
        if self.tracks[i].name != name {
            self.tracks[i].name = name.to_string();
            self.revision.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Says what a track is (an audio track until set). Not heard: it tells
    /// modules whose a track is, and the project saves it.
    pub fn set_kind(&mut self, id: TrackId, kind: TrackKind) -> Result<(), String> {
        let i = self.index(id)?;
        if self.tracks[i].kind != kind {
            self.tracks[i].kind = kind;
            self.revision.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Puts a device in a track's source slot (an instrument, a clip
    /// player), or empties it. The old one is dropped on this thread.
    pub fn set_source(&mut self, id: TrackId, device: Option<Box<dyn Device>>) -> Result<(), String> {
        let i = self.index(id)?;
        let slot = device.map(|d| self.put(d)).transpose()?;
        let old = std::mem::replace(&mut self.tracks[i].source, slot);
        if let Some(old) = old {
            self.free(old);
        }
        self.changed();
        Ok(())
    }

    /// Puts a device back in a track's empty source slot after it was lost
    /// with the output. The same as [`set_source`](Self::set_source) but not an
    /// edit: the project's revision stays, since nothing it saves changed.
    pub fn restore_source(&mut self, id: TrackId, device: Box<dyn Device>) -> Result<(), String> {
        let before = self.revision();
        self.set_source(id, Some(device))?;
        self.revision.store(before, Ordering::Relaxed);
        Ok(())
    }

    /// Inserts a device into a track's chain at `index` (clamped to its end).
    pub fn insert(&mut self, id: TrackId, index: usize, device: Box<dyn Device>) -> Result<(), String> {
        let i = self.index(id)?;
        let slot = self.put(device)?;
        let chain = &mut self.tracks[i].chain;
        chain.insert(index.min(chain.len()), slot);
        self.changed();
        Ok(())
    }

    /// Removes the device at `index` of a track's chain.
    pub fn remove(&mut self, id: TrackId, index: usize) -> Result<(), String> {
        let i = self.index(id)?;
        let chain = &mut self.tracks[i].chain;
        if index >= chain.len() {
            return Err(format!("track {} has no device {}", id.0, index + 1));
        }
        let slot = chain.remove(index);
        self.free(slot);
        self.changed();
        Ok(())
    }

    /// A handle that reads a track's level from any thread.
    pub fn meter(&self, id: TrackId) -> Option<TrackMeter> {
        self.tracks.iter().find(|t| t.id == id).map(|t| TrackMeter { meters: self.meters.clone(), slot: t.slot })
    }

    /// Whether a graph is running.
    pub fn is_running(&self) -> bool {
        self.link.is_some()
    }

    /// Prepares every device for `rate` and moves them into a new graph,
    /// for the audio thread to run. Call [`stopped`](Self::stopped) once that
    /// graph is dropped.
    pub fn start(&mut self, rate: u32) -> Graph {
        if self.link.is_some() {
            self.stopped();
        }
        let rate = rate.max(1);
        for d in self.devices.iter_mut().flatten() {
            d.prepare(rate, MAX_FRAMES);
        }
        let devices = std::mem::replace(&mut self.devices, empty_slots());
        let mailbox = Arc::new(Mailbox::new());
        let (garbage, returned) = RingBuffer::new(RETURN_QUEUE);
        let (home_tx, home) = mpsc::channel();
        let first = self.snapshot();
        self.link = Some(Link { rate, mailbox: mailbox.clone(), returned, home });
        Graph::new(rate, devices, first, mailbox, garbage, home_tx, self.meters.clone())
    }

    /// After the graph is dropped: takes the devices back. Returns false
    /// when they did not come home (the stream died without dropping its
    /// graph); then every track's devices are gone and must be put back.
    pub fn stopped(&mut self) -> bool {
        let Some(mut link) = self.link.take() else { return true };
        while link.returned.pop().is_ok() {}
        match link.home.recv_timeout(HOME_WAIT) {
            Ok(devices) => {
                for (slot, d) in devices.into_iter().enumerate().take(DEVICE_SLOTS) {
                    // A device whose slot was freed while it ran is dropped here.
                    if self.taken[slot] {
                        self.devices[slot] = d;
                    }
                }
                for Install { slot, device } in std::mem::take(&mut self.pending) {
                    self.devices[slot] = device;
                }
                true
            }
            Err(_) => {
                self.taken.fill(false);
                self.pending.clear();
                // Not an edit: the track list and mixes a project saves are unchanged.
                for t in &mut self.tracks {
                    t.source = None;
                    t.chain.clear();
                }
                false
            }
        }
    }

    /// Frees what the audio thread has handed back. Every edit does this too.
    pub fn collect(&mut self) {
        if let Some(link) = &mut self.link {
            while link.returned.pop().is_ok() {}
        }
    }

    fn index(&self, id: TrackId) -> Result<usize, String> {
        self.tracks.iter().position(|t| t.id == id).ok_or(format!("there is no track {}", id.0))
    }

    /// Finds a slot for a device and installs it there: now while no graph
    /// runs, with the next snapshot while one does.
    fn put(&mut self, mut device: Box<dyn Device>) -> Result<usize, String> {
        let slot = self.taken.iter().position(|t| !t).ok_or(format!("tuneKore holds at most {DEVICE_SLOTS} devices"))?;
        match &self.link {
            Some(link) => {
                device.prepare(link.rate, MAX_FRAMES);
                stage(&mut self.pending, slot, Some(device));
            }
            None => self.devices[slot] = Some(device),
        }
        self.taken[slot] = true;
        Ok(slot)
    }

    /// Empties a slot. A running graph sends its device back to be dropped.
    fn free(&mut self, slot: usize) {
        self.taken[slot] = false;
        match &self.link {
            Some(_) => stage(&mut self.pending, slot, None),
            None => self.devices[slot] = None,
        }
    }

    fn snapshot(&self) -> Box<Snapshot> {
        let any_solo = self.tracks.iter().any(|t| t.mix.solo);
        let tracks = self
            .tracks
            .iter()
            .map(|t| {
                let audible = !t.mix.mute && (!any_solo || t.mix.solo);
                TrackSnap {
                    id: t.id,
                    slot: t.slot,
                    source: t.source,
                    chain: t.chain.clone(),
                    gains: if audible { t.mix.gains() } else { [0.0; 2] },
                }
            })
            .collect();
        Box::new(Snapshot { tracks, installs: Vec::new() })
    }

    fn changed(&mut self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
        self.publish();
    }

    /// Puts the document's newest snapshot, with the devices waiting to go
    /// in, in the graph's mailbox. A snapshot the audio thread never took
    /// gives its installs to the new one, so none is lost.
    fn publish(&mut self) {
        if self.link.is_none() {
            return;
        }
        self.collect();
        let mut next = self.snapshot();
        let Some(link) = &self.link else { return };
        let mut installs = link.mailbox.take().map(|mut unread| std::mem::take(&mut unread.installs)).unwrap_or_default();
        for Install { slot, device } in std::mem::take(&mut self.pending) {
            stage(&mut installs, slot, device);
        }
        next.installs = installs;
        // Only this thread puts, so nothing was waiting after the take above.
        drop(link.mailbox.put(next));
    }
}

fn info(t: &TrackDoc) -> TrackInfo {
    TrackInfo { id: t.id, name: t.name.clone(), kind: t.kind, mix: t.mix, has_source: t.source.is_some(), chain: t.chain.len() }
}

/// Per-track state the audio thread keeps between blocks.
#[derive(Debug, Clone, Copy, Default)]
struct TrackState {
    /// The track this slot belongs to now; a new one starts at its level.
    id: Option<TrackId>,
    gains: [f32; 2],
    peak: f32,
}

/// The audio thread's side. Lives in the output callback.
pub struct Graph {
    ramp: f32,
    devices: Slots,
    current: Box<Snapshot>,
    mailbox: Arc<Mailbox<Snapshot>>,
    garbage: Producer<Garbage>,
    /// What didn't fit on the return queue, waiting for room. Allocated once
    /// with room for one snapshot's worth: a new snapshot is taken only once
    /// it is empty.
    spill: Vec<Garbage>,
    state: Vec<TrackState>,
    left: Vec<f32>,
    right: Vec<f32>,
    master: [Vec<f32>; 2],
    frames: usize,
    events: Events,
    meters: Arc<TrackMeters>,
    home: Option<mpsc::Sender<Slots>>,
}

impl Graph {
    fn new(
        rate: u32,
        devices: Slots,
        first: Box<Snapshot>,
        mailbox: Arc<Mailbox<Snapshot>>,
        garbage: Producer<Garbage>,
        home: mpsc::Sender<Slots>,
        meters: Arc<TrackMeters>,
    ) -> Self {
        let mut g = Self {
            ramp: 1.0 / (rate as f32 * RAMP_SECONDS).max(1.0),
            devices,
            current: first,
            mailbox,
            garbage,
            spill: Vec::with_capacity(DEVICE_SLOTS + 2),
            state: vec![TrackState::default(); TRACK_SLOTS],
            left: vec![0.0; MAX_FRAMES],
            right: vec![0.0; MAX_FRAMES],
            master: [vec![0.0; MAX_FRAMES], vec![0.0; MAX_FRAMES]],
            frames: 0,
            events: Events::with_capacity(TRACK_EVENTS),
            meters,
            home: Some(home),
        };
        g.sync_state();
        g
    }

    /// Runs one block of up to [`MAX_FRAMES`] frames: takes the newest
    /// snapshot and its devices, lets `transport` move the clock, and plays
    /// every track over each piece of the block into the master. Real-time safe.
    pub fn run(&mut self, transport: &mut Transport, shared: &TransportShared, frames: usize) {
        let frames = frames.min(MAX_FRAMES);
        self.take_snapshot();
        self.frames = frames;
        self.master[0][..frames].fill(0.0);
        self.master[1][..frames].fill(0.0);
        transport.run(shared, frames, |ctx, offset| self.process(ctx, offset));
        let Graph { current, state, meters, .. } = self;
        for t in &current.tracks {
            let st = &mut state[t.slot];
            meters.raise(t.slot, st.peak);
            st.peak = 0.0;
        }
    }

    /// The master, left and right, for the last block run.
    pub fn master(&self) -> (&[f32], &[f32]) {
        (&self.master[0][..self.frames], &self.master[1][..self.frames])
    }

    /// Sends back what is waiting, then, once nothing is, switches to the
    /// newest snapshot and installs its devices.
    fn take_snapshot(&mut self) {
        while let Some(g) = self.spill.pop() {
            if let Err(PushError::Full(g)) = self.garbage.push(g) {
                self.spill.push(g); // room again: it was just popped
                return;
            }
        }
        let Some(mut next) = self.mailbox.take() else { return };
        for i in 0..next.installs.len() {
            let Install { slot, device } = &mut next.installs[i];
            let old = match self.devices.get_mut(*slot) {
                Some(place) => std::mem::replace(place, device.take()),
                None => device.take(), // not a slot: send it straight back
            };
            if let Some(old) = old {
                self.give_back(Garbage::Device(old));
            }
        }
        let old = std::mem::replace(&mut self.current, next);
        self.give_back(Garbage::Snapshot(old));
        self.sync_state();
    }

    /// Hands something back to the main thread, or keeps it until there is room.
    fn give_back(&mut self, g: Garbage) {
        if let Err(PushError::Full(g)) = self.garbage.push(g) {
            if self.spill.len() < self.spill.capacity() {
                self.spill.push(g);
            } else {
                // Can't happen (a snapshot holds one install per slot), and
                // leaking beats freeing on the audio thread.
                std::mem::forget(g);
            }
        }
    }

    /// A track that is new in its slot starts at its level, with no fade in.
    fn sync_state(&mut self) {
        let Graph { current, state, .. } = self;
        for t in &current.tracks {
            let st = &mut state[t.slot];
            if st.id != Some(t.id) {
                *st = TrackState { id: Some(t.id), gains: t.gains, peak: 0.0 };
            }
        }
    }

    fn process(&mut self, ctx: &ProcessContext, offset: usize) {
        let n = ctx.frames.min(self.frames.saturating_sub(offset));
        let Graph { current, devices, state, left, right, master, events, ramp, .. } = self;
        let [ml, mr] = master;
        let (ml, mr) = (&mut ml[offset..offset + n], &mut mr[offset..offset + n]);
        let ctx = &ProcessContext { frames: n, ..*ctx };
        for t in &current.tracks {
            let (l, r) = (&mut left[..n], &mut right[..n]);
            l.fill(0.0);
            r.fill(0.0);
            events.clear();
            // Devices run even on a muted track, so its sequencer and its
            // plugins stay in time.
            for &slot in t.source.iter().chain(&t.chain) {
                if let Some(d) = devices.get_mut(slot).and_then(Option::as_mut) {
                    d.process(ctx, events, Buffers { left: &mut *l, right: &mut *r });
                }
            }
            let st = &mut state[t.slot];
            let mut peak = st.peak;
            if st.gains == t.gains {
                let [gl, gr] = st.gains;
                for i in 0..n {
                    let (a, b) = (l[i] * gl, r[i] * gr);
                    ml[i] += a;
                    mr[i] += b;
                    peak = peak.max(a.abs()).max(b.abs());
                }
            } else {
                for i in 0..n {
                    st.gains[0] = approach(st.gains[0], t.gains[0], *ramp);
                    st.gains[1] = approach(st.gains[1], t.gains[1], *ramp);
                    let (a, b) = (l[i] * st.gains[0], r[i] * st.gains[1]);
                    ml[i] += a;
                    mr[i] += b;
                    peak = peak.max(a.abs()).max(b.abs());
                }
            }
            st.peak = peak;
        }
    }
}

impl Drop for Graph {
    /// Runs where the output callback is dropped, never inside it: installs
    /// what was still waiting, then sends every device home to the mixer.
    fn drop(&mut self) {
        if let Some(mut next) = self.mailbox.take() {
            for Install { slot, device } in std::mem::take(&mut next.installs) {
                if let Some(place) = self.devices.get_mut(slot) {
                    *place = device;
                }
            }
        }
        if let Some(home) = self.home.take() {
            let _ = home.send(std::mem::take(&mut self.devices));
        }
    }
}

fn approach(current: f32, target: f32, step: f32) -> f32 {
    if current < target {
        (current + step).min(target)
    } else if current > target {
        (current - step).max(target)
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::realtime;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const RATE: u32 = 48_000;

    /// Adds a constant to both sides: a stand-in instrument.
    struct Dc(f32);
    impl Device for Dc {
        fn process(&mut self, _: &ProcessContext, _: &mut Events, audio: Buffers<'_>) {
            audio.left.iter_mut().for_each(|x| *x += self.0);
            audio.right.iter_mut().for_each(|x| *x += self.0);
        }
    }

    /// Scales both sides: a stand-in effect.
    struct Gain(f32);
    impl Device for Gain {
        fn process(&mut self, _: &ProcessContext, _: &mut Events, audio: Buffers<'_>) {
            audio.left.iter_mut().for_each(|x| *x *= self.0);
            audio.right.iter_mut().for_each(|x| *x *= self.0);
        }
    }

    /// Notes where it was dropped and how often it was prepared.
    struct Probe {
        dropped: Arc<AtomicBool>,
        prepared: Arc<AtomicUsize>,
    }
    impl Device for Probe {
        fn prepare(&mut self, _: u32, _: usize) {
            self.prepared.fetch_add(1, Ordering::Relaxed);
        }
        fn process(&mut self, _: &ProcessContext, _: &mut Events, _: Buffers<'_>) {}
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Relaxed);
        }
    }

    struct Rig {
        mixer: Mixer,
        graph: Graph,
        clock: Transport,
        shared: TransportShared,
    }

    impl Rig {
        fn new(mixer: Mixer) -> Self {
            let mut mixer = mixer;
            let graph = mixer.start(RATE);
            Self { mixer, graph, clock: Transport::new(RATE), shared: TransportShared::default() }
        }

        /// Runs `frames` frames and returns the last frame of the master.
        fn run(&mut self, frames: usize) -> (f32, f32) {
            self.graph.run(&mut self.clock, &self.shared, frames);
            let (l, r) = self.graph.master();
            (l[frames - 1], r[frames - 1])
        }
    }

    fn two_tracks() -> (Mixer, TrackId, TrackId) {
        let mut m = Mixer::new();
        let a = m.add_track("A").unwrap();
        let b = m.add_track("B").unwrap();
        m.set_source(a, Some(Box::new(Dc(0.25)))).unwrap();
        m.set_source(b, Some(Box::new(Dc(0.5)))).unwrap();
        (m, a, b)
    }

    #[test]
    fn two_tracks_sum_into_the_master() {
        let (m, _, _) = two_tracks();
        let mut rig = Rig::new(m);
        assert_eq!(rig.run(256), (0.75, 0.75));
        let (l, _) = rig.graph.master();
        assert!(l.iter().all(|&x| x == 0.75), "from the first frame: a new track does not fade in");
    }

    #[test]
    fn mute_and_solo_pick_what_is_heard() {
        let (m, a, b) = two_tracks();
        let mut rig = Rig::new(m);
        let solo = TrackMix { solo: true, ..Default::default() };
        let mute = TrackMix { mute: true, ..Default::default() };

        rig.mixer.set_mix(b, mute).unwrap();
        assert_eq!(rig.run(1_000), (0.25, 0.25), "B muted");
        rig.mixer.set_mix(b, solo).unwrap();
        assert_eq!(rig.run(1_000), (0.5, 0.5), "B soloed: only B");
        rig.mixer.set_mix(a, solo).unwrap();
        assert_eq!(rig.run(1_000), (0.75, 0.75), "both soloed");
        rig.mixer.set_mix(a, TrackMix { mute: true, solo: true, ..Default::default() }).unwrap();
        assert_eq!(rig.run(1_000), (0.5, 0.5), "a muted track stays silent, soloed or not");
        rig.mixer.set_mix(a, TrackMix::default()).unwrap();
        rig.mixer.set_mix(b, TrackMix::default()).unwrap();
        assert_eq!(rig.run(1_000), (0.75, 0.75), "nothing soloed: all heard");
    }

    #[test]
    fn volume_and_pan_ramp_to_their_level_without_a_jump() {
        let (m, a, _) = two_tracks();
        let mut rig = Rig::new(m);
        rig.run(100);
        rig.mixer.set_mix(a, TrackMix { volume: 0.0, pan: 1.0, ..Default::default() }).unwrap();
        rig.run(1_000);
        let (l, _) = rig.graph.master();
        let max_step = l.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(max_step < 0.001, "step {max_step}");
        assert_eq!((l[999], rig.graph.master().1[999]), (0.5, 0.5), "A is silent after the ramp");
    }

    #[test]
    fn the_chain_runs_in_order_after_the_source() {
        let mut m = Mixer::new();
        let t = m.add_track("T").unwrap();
        m.set_source(t, Some(Box::new(Dc(1.0)))).unwrap();
        m.insert(t, 0, Box::new(Gain(2.0))).unwrap();
        m.insert(t, 1, Box::new(Dc(0.25))).unwrap();
        let mut rig = Rig::new(m);
        assert_eq!(rig.run(64), (2.25, 2.25), "doubled, then 0.25 added");
        rig.mixer.remove(t, 0).unwrap();
        assert_eq!(rig.run(64), (1.25, 1.25));
        assert!(rig.mixer.remove(t, 5).is_err());
    }

    #[test]
    fn meters_read_each_track_after_its_level() {
        let (mut m, a, b) = two_tracks();
        m.set_mix(b, TrackMix { volume: 0.5, ..Default::default() }).unwrap();
        let (ma, mb) = (m.meter(a).unwrap(), m.meter(b).unwrap());
        let mut rig = Rig::new(m);
        rig.run(512);
        assert_eq!((ma.take_peak(), mb.take_peak()), (0.25, 0.25));
        assert_eq!(ma.take_peak(), 0.0, "taken");
    }

    #[test]
    fn removed_devices_are_dropped_on_the_main_thread() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut m = Mixer::new();
        let t = m.add_track("T").unwrap();
        let mut rig = Rig::new(m);
        let probe = Probe { dropped: dropped.clone(), prepared: Arc::default() };
        rig.mixer.set_source(t, Some(Box::new(probe))).unwrap();
        rig.run(64);
        rig.mixer.set_source(t, Some(Box::new(Dc(0.1)))).unwrap();
        let (_, allocs) = realtime(|| rig.run(64));
        assert_eq!(allocs, 0);
        assert!(!dropped.load(Ordering::Relaxed), "the audio thread handed it back, not dropped it");
        rig.mixer.collect();
        assert!(dropped.load(Ordering::Relaxed), "the main thread dropped it");
        assert_eq!(rig.run(64), (0.1, 0.1));
    }

    #[test]
    fn devices_come_home_when_the_graph_stops_and_are_prepared_again() {
        let prepared = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let mut m = Mixer::new();
        let t = m.add_track("T").unwrap();
        m.set_source(t, Some(Box::new(Probe { dropped: dropped.clone(), prepared: prepared.clone() }))).unwrap();
        m.insert(t, 0, Box::new(Dc(0.5))).unwrap();
        let graph = m.start(RATE);
        assert_eq!(prepared.load(Ordering::Relaxed), 1);
        drop(graph);
        assert!(m.stopped(), "the devices came back");
        assert!(!dropped.load(Ordering::Relaxed));
        let mut rig = Rig::new(m);
        assert_eq!(prepared.load(Ordering::Relaxed), 2, "prepared for the new output");
        assert_eq!(rig.run(32), (0.5, 0.5), "the chain survived too");
        assert_eq!(rig.mixer.track(t).map(|i| (i.has_source, i.chain)), Some((true, 1)));
    }

    #[test]
    fn devices_lost_with_their_graph_leave_the_tracks_empty() {
        let mut m = Mixer::new();
        let t = m.add_track("T").unwrap();
        m.set_source(t, Some(Box::new(Dc(0.5)))).unwrap();
        let graph = m.start(RATE);
        std::mem::forget(graph); // a stream that never drops its callback
        let r = m.revision();
        assert!(!m.stopped());
        assert_eq!(m.track(t).map(|i| i.has_source), Some(false));
        m.restore_source(t, Box::new(Dc(0.5))).unwrap();
        assert_eq!(m.track(t).map(|i| i.has_source), Some(true));
        assert_eq!(m.revision(), r, "losing and restoring devices is not an edit");
    }

    #[test]
    fn tracks_get_ids_and_removing_one_frees_its_slot() {
        let mut m = Mixer::new();
        let a = m.add_track("A").unwrap();
        m.add_track_as(TrackId(10), "Ten").unwrap();
        assert!(m.add_track_as(TrackId(10), "Again").is_err());
        let next = m.add_track("Next").unwrap();
        assert_eq!(next, TrackId(11), "new ids follow the highest");
        m.remove_track(a).unwrap();
        assert!(m.track(a).is_none());
        assert!(m.remove_track(a).is_err());
        assert_eq!(m.tracks().iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["Ten", "Next"]);
        let r = m.revision();
        m.rename(next, "Renamed").unwrap();
        assert!(m.revision() > r);
        assert_eq!(m.track(next).map(|t| t.kind), Some(TrackKind::Audio), "audio until said otherwise");
        let r = m.revision();
        m.set_kind(next, TrackKind::Instrument).unwrap();
        assert_eq!(m.track(next).map(|t| t.kind), Some(TrackKind::Instrument));
        assert!(m.revision() > r, "saved, so an edit");
        assert!(m.set_kind(TrackId(99), TrackKind::Audio).is_err());
        m.set_next_track_id(TrackId(20));
        assert_eq!(m.next_track_id(), TrackId(20));
        assert_eq!(m.add_track("After").unwrap(), TrackId(20));
        m.set_next_track_id(TrackId(0));
        assert_eq!(m.next_track_id(), TrackId(21), "never at or below a track there is");
    }

    #[test]
    fn snapshot_swaps_and_device_changes_never_allocate_on_the_audio_thread() {
        let (m, a, b) = two_tracks();
        let mut rig = Rig::new(m);
        rig.shared.play();
        let mut total = 0;
        for i in 0..200 {
            // The main thread edits between blocks, as the UI does.
            match i % 5 {
                0 => rig.mixer.set_mix(a, TrackMix { volume: (i % 7) as f32 / 7.0, ..Default::default() }).unwrap(),
                1 => rig.mixer.set_mix(b, TrackMix { solo: i % 2 == 0, pan: -0.5, ..Default::default() }).unwrap(),
                2 => rig.mixer.insert(a, 0, Box::new(Gain(0.9))).unwrap(),
                3 => rig.mixer.remove(a, 0).unwrap(),
                _ => rig.mixer.set_source(b, Some(Box::new(Dc(0.5)))).unwrap(),
            }
            let (_, allocs) = realtime(|| rig.graph.run(&mut rig.clock, &rig.shared, 333));
            total += allocs;
        }
        assert_eq!(total, 0, "the audio thread allocated or freed {total} times");
        rig.mixer.collect();
    }

    #[test]
    fn a_burst_of_edits_in_one_block_lands_whole() {
        let mut m = Mixer::new();
        let first = m.add_track("first").unwrap();
        let mut rig = Rig::new(m);
        // Far more edits than any queue would hold, all before the next block.
        let mut ids = vec![first];
        for i in 1..40 {
            ids.push(rig.mixer.add_track(&format!("t{i}")).unwrap());
        }
        for (i, &id) in ids.iter().enumerate() {
            rig.mixer.set_source(id, Some(Box::new(Dc(0.01)))).unwrap();
            for v in 0..5 {
                rig.mixer.set_mix(id, TrackMix { volume: if i % 2 == 0 { 1.0 } else { v as f32 / 4.0 }, ..Default::default() }).unwrap();
            }
        }
        let (l, _) = rig.run(64);
        // 20 tracks at full level and 20 at the last volume set, 1.0: all 40 at 0.01.
        assert!((l - 0.40).abs() < 1e-5, "{l}");
        assert_eq!(rig.mixer.tracks().len(), 40);
    }

    #[test]
    fn a_slot_freed_and_used_again_in_one_block_never_plays_the_old_device() {
        let mut m = Mixer::new();
        let a = m.add_track("A").unwrap();
        m.set_source(a, Some(Box::new(Dc(0.25)))).unwrap();
        let mut rig = Rig::new(m);
        assert_eq!(rig.run(64), (0.25, 0.25));
        // Remove A (its device slot frees) and give a new track a device, which
        // takes the same slot, all before the next block.
        rig.mixer.remove_track(a).unwrap();
        let b = rig.mixer.add_track("B").unwrap();
        rig.mixer.set_source(b, Some(Box::new(Dc(0.5)))).unwrap();
        rig.graph.run(&mut rig.clock, &rig.shared, 64);
        let (l, _) = rig.graph.master();
        assert!(l.iter().all(|&x| x == 0.5), "only B, from its first frame");
    }
}
