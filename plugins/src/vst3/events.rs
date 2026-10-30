//! The event lists and parameter queues `process` hands to a plugin, built to be filled and read
//! on the audio thread without allocating or locking.
//!
//! Derived from soundCraft's `vst3-host` (`ffi.rs`), MIT OR Apache-2.0; see
//! `third_party/soundcraft/`. What changed: no mutexes (these objects belong to one thread at a
//! time: the audio thread, filling them before `process` and reading them after, while the plugin
//! reads or fills them inside it); a parameter queue holds many points so automation can ramp
//! inside a block, kept in time order and always ending on the latest value; note-offs have
//! room kept for them so a full list cannot leave notes stuck; and everything dropped is
//! counted.

use crate::plugin::{Event, EventKind};
use std::cell::{Cell, UnsafeCell};
use vst3::Steinberg::Vst::{self as sv, IEventListTrait, IParamValueQueueTrait, IParameterChangesTrait};
use vst3::Steinberg::{self as sb};
use vst3::{Class, ComWrapper};

/// VST3's numbers for the MIDI controllers that aren't CCs, as `IMidiMapping` takes them.
pub(crate) const AFTERTOUCH: usize = sv::ControllerNumbers_::kAfterTouch as usize;
pub(crate) const PITCH_BEND: usize = sv::ControllerNumbers_::kPitchBend as usize;

// ---- events --------------------------------------------------------------------------------

/// Room in each event list kept for note-offs only, so that a block full of notes can always
/// still end the ones it started.
const NOTE_OFF_RESERVE: usize = 128;

/// A fixed-capacity `IEventList`.
///
/// Not thread-safe by design: the audio thread fills it, calls `process` (the plugin reads it,
/// or for the output list writes it, on that same thread), then reads it back. The COM methods
/// are the only other users, and they run inside that call.
pub(crate) struct EventList {
    events: UnsafeCell<Vec<sv::Event>>,
    cap: usize,
    dropped: Cell<u32>,
}

impl Class for EventList {
    type Interfaces = (sv::IEventList,);
}

impl EventList {
    pub fn new(cap: usize) -> EventList {
        EventList { events: UnsafeCell::new(Vec::with_capacity(cap)), cap, dropped: Cell::new(0) }
    }

    /// # Safety
    /// The caller is the one thread using the list right now (see the type's notes), and does
    /// not keep the reference across a call into the plugin.
    #[allow(clippy::mut_from_ref)]
    unsafe fn list(&self) -> &mut Vec<sv::Event> {
        unsafe { &mut *self.events.get() }
    }

    pub fn clear(&self) {
        // SAFETY: single-threaded use, see the type's notes.
        unsafe { self.list() }.clear();
    }

    /// Adds an event; `false` when there is no room (the event is dropped and counted). The
    /// last [`NOTE_OFF_RESERVE`] places take note-offs only.
    pub fn push(&self, e: sv::Event) -> bool {
        // SAFETY: as above.
        let l = unsafe { self.list() };
        let is_off = u32::from(e.r#type) == sv::Event_::EventTypes_::kNoteOffEvent as u32;
        let room = if is_off { self.cap } else { self.cap.saturating_sub(NOTE_OFF_RESERVE) };
        if l.len() < room {
            l.push(e);
            true
        } else {
            self.dropped.set(self.dropped.get().saturating_add(1));
            false
        }
    }

    /// Events dropped since the last call.
    pub fn take_dropped(&self) -> u32 {
        self.dropped.replace(0)
    }

    pub fn len(&self) -> usize {
        // SAFETY: as above.
        unsafe { self.list() }.len()
    }

    pub fn get(&self, i: usize) -> Option<sv::Event> {
        // SAFETY: as above.
        unsafe { self.list() }.get(i).copied()
    }
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> sb::int32 {
        i32::try_from(self.len()).unwrap_or(i32::MAX)
    }
    unsafe fn getEvent(&self, index: sb::int32, e: *mut sv::Event) -> sb::tresult {
        // SAFETY: out-pointer from the plugin (or null, handled).
        let Some(out) = (unsafe { e.as_mut() }) else { return sb::kInvalidArgument };
        let Ok(i) = usize::try_from(index) else { return sb::kInvalidArgument };
        match self.get(i) {
            Some(ev) => {
                *out = ev;
                sb::kResultOk
            }
            None => sb::kInvalidArgument,
        }
    }
    unsafe fn addEvent(&self, e: *mut sv::Event) -> sb::tresult {
        // SAFETY: in-pointer from the plugin (or null, handled).
        let Some(&ev) = (unsafe { e.as_ref() }) else { return sb::kInvalidArgument };
        if self.push(ev) { sb::kResultOk } else { sb::kResultFalse }
    }
}

/// Our [`Event`] as VST3's, for the note events. `None` for the MIDI messages that go through
/// parameters (control change, pitch bend, channel pressure).
pub(crate) fn to_vst3_event(e: &Event) -> Option<sv::Event> {
    // SAFETY: `Event` is plain data (numbers and a union of plain structs); all-zero is valid.
    let mut out: sv::Event = unsafe { std::mem::zeroed() };
    out.busIndex = 0;
    out.sampleOffset = i32::try_from(e.offset).unwrap_or(0);
    out.flags = sv::Event_::EventFlags_::kIsLive as u16;
    match e.kind {
        EventKind::NoteOn { channel, key, velocity, note_id } => {
            out.r#type = sv::Event_::EventTypes_::kNoteOnEvent as u16;
            out.__field0.noteOn = sv::NoteOnEvent { channel: i16::from(channel), pitch: i16::from(key), tuning: 0.0, velocity, length: 0, noteId: note_id };
        }
        EventKind::NoteOff { channel, key, velocity, note_id } => {
            out.r#type = sv::Event_::EventTypes_::kNoteOffEvent as u16;
            out.__field0.noteOff = sv::NoteOffEvent { channel: i16::from(channel), pitch: i16::from(key), velocity, noteId: note_id, tuning: 0.0 };
        }
        EventKind::PolyPressure { channel, key, pressure, note_id } => {
            out.r#type = sv::Event_::EventTypes_::kPolyPressureEvent as u16;
            out.__field0.polyPressure = sv::PolyPressureEvent { channel: i16::from(channel), pitch: i16::from(key), pressure, noteId: note_id };
        }
        EventKind::ChannelPressure { .. } | EventKind::ControlChange { .. } | EventKind::PitchBend { .. } => return None,
    }
    Some(out)
}

/// A note event a plugin sent out, as ours. Other kinds of output event are ignored.
pub(crate) fn from_vst3_event(e: &sv::Event) -> Option<Event> {
    use sv::Event_::EventTypes_ as t;
    let offset = u32::try_from(e.sampleOffset).unwrap_or(0);
    let kind = match u32::from(e.r#type) {
        x if x == t::kNoteOnEvent as u32 => {
            // SAFETY: the type tag says which union member the plugin wrote.
            let n = unsafe { e.__field0.noteOn };
            EventKind::NoteOn { channel: n.channel.clamp(0, 15) as u8, key: n.pitch.clamp(0, 127) as u8, velocity: n.velocity, note_id: n.noteId }
        }
        x if x == t::kNoteOffEvent as u32 => {
            // SAFETY: as above.
            let n = unsafe { e.__field0.noteOff };
            EventKind::NoteOff { channel: n.channel.clamp(0, 15) as u8, key: n.pitch.clamp(0, 127) as u8, velocity: n.velocity, note_id: n.noteId }
        }
        x if x == t::kPolyPressureEvent as u32 => {
            // SAFETY: as above.
            let n = unsafe { e.__field0.polyPressure };
            EventKind::PolyPressure { channel: n.channel.clamp(0, 15) as u8, key: n.pitch.clamp(0, 127) as u8, pressure: n.pressure, note_id: n.noteId }
        }
        _ => return None,
    };
    Some(Event { offset, kind })
}

// ---- parameter changes ---------------------------------------------------------------------

/// Points one parameter can have in one block.
pub(crate) const POINTS_PER_QUEUE: usize = 32;

#[derive(Clone, Copy, Default)]
struct Point {
    offset: i32,
    value: f64,
}

/// One parameter's points for one block (`IParamValueQueue`). Same single-thread rule as
/// [`EventList`].
pub(crate) struct ParamQueue {
    id: Cell<u32>,
    count: Cell<usize>,
    points: UnsafeCell<[Point; POINTS_PER_QUEUE]>,
}

impl Class for ParamQueue {
    type Interfaces = (sv::IParamValueQueue,);
}

impl ParamQueue {
    fn new() -> ParamQueue {
        ParamQueue { id: Cell::new(0), count: Cell::new(0), points: UnsafeCell::new([Point::default(); POINTS_PER_QUEUE]) }
    }

    /// Adds a point in time order. A point at an offset already present replaces it (the later
    /// write wins). When the queue is full, a point after the last one replaces the last one, so
    /// a ramp always ends on its final value; any other point is dropped. Returns the index the
    /// point went to, and whether a point was lost.
    fn add(&self, offset: i32, value: f64) -> (Option<usize>, bool) {
        let n = self.count.get();
        // SAFETY: single-threaded use, see `EventList`.
        let pts = unsafe { &mut *self.points.get() };
        // Where it goes: after every point at or before `offset`.
        let at = pts[..n].partition_point(|p| p.offset <= offset);
        if at > 0 && pts[at - 1].offset == offset {
            pts[at - 1].value = value;
            return (Some(at - 1), false);
        }
        if n >= POINTS_PER_QUEUE {
            if at == n {
                pts[n - 1] = Point { offset, value };
                return (Some(n - 1), true);
            }
            return (None, true);
        }
        pts.copy_within(at..n, at + 1);
        pts[at] = Point { offset, value };
        self.count.set(n + 1);
        (Some(at), false)
    }

    fn point(&self, i: usize) -> Option<Point> {
        if i < self.count.get() {
            // SAFETY: as above.
            Some(unsafe { &*self.points.get() }[i])
        } else {
            None
        }
    }
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> sv::ParamID {
        self.id.get()
    }
    unsafe fn getPointCount(&self) -> sb::int32 {
        i32::try_from(self.count.get()).unwrap_or(0)
    }
    unsafe fn getPoint(&self, index: sb::int32, offset: *mut sb::int32, value: *mut sv::ParamValue) -> sb::tresult {
        let Some(p) = usize::try_from(index).ok().and_then(|i| self.point(i)) else { return sb::kInvalidArgument };
        // SAFETY: out-pointers from the plugin (or null, handled).
        if let (Some(o), Some(v)) = (unsafe { offset.as_mut() }, unsafe { value.as_mut() }) {
            *o = p.offset;
            *v = p.value;
            sb::kResultOk
        } else {
            sb::kInvalidArgument
        }
    }
    unsafe fn addPoint(&self, offset: sb::int32, value: sv::ParamValue, index: *mut sb::int32) -> sb::tresult {
        match self.add(offset, value).0 {
            Some(i) => {
                // SAFETY: out-pointer from the plugin (or null, handled).
                if let Some(out) = unsafe { index.as_mut() } {
                    *out = i32::try_from(i).unwrap_or(0);
                }
                sb::kResultOk
            }
            None => sb::kResultFalse,
        }
    }
}

/// A fixed pool of parameter queues (`IParameterChanges`), reused every block.
pub(crate) struct ParamChanges {
    queues: Vec<ComWrapper<ParamQueue>>,
    count: Cell<usize>,
    dropped: Cell<u32>,
}

impl Class for ParamChanges {
    type Interfaces = (sv::IParameterChanges,);
}

impl ParamChanges {
    pub fn new(queues: usize) -> ParamChanges {
        ParamChanges { queues: (0..queues).map(|_| ComWrapper::new(ParamQueue::new())).collect(), count: Cell::new(0), dropped: Cell::new(0) }
    }

    pub fn clear(&self) {
        self.count.set(0);
        for q in &self.queues {
            q.count.set(0);
        }
    }

    /// Finds or starts the queue for `id`; `None` when the pool is full.
    fn queue_for(&self, id: u32) -> Option<(usize, &ParamQueue)> {
        let n = self.count.get();
        for (i, q) in self.queues.iter().enumerate().take(n) {
            if q.id.get() == id {
                return Some((i, q));
            }
        }
        let q = self.queues.get(n)?;
        q.id.set(id);
        q.count.set(0);
        self.count.set(n + 1);
        Some((n, q))
    }

    /// Whether a point for `id` would find a queue (its own, or a free one).
    pub fn can_take(&self, id: u32) -> bool {
        self.count.get() < self.queues.len() || self.queues.iter().take(self.count.get()).any(|q| q.id.get() == id)
    }

    /// Host side: one point for `id` at `offset`, in time order (see [`ParamQueue::add`]).
    /// Returns whether it went in; anything lost is counted.
    pub fn add(&self, id: u32, offset: u32, value: f64) -> bool {
        let (placed, lost) = match self.queue_for(id) {
            Some((_, q)) => {
                let (at, lost) = q.add(i32::try_from(offset).unwrap_or(0), value);
                (at.is_some(), lost)
            }
            None => (false, true),
        };
        if lost {
            self.dropped.set(self.dropped.get().saturating_add(1));
        }
        placed
    }

    /// Points dropped since the last call.
    pub fn take_dropped(&self) -> u32 {
        self.dropped.replace(0)
    }

    pub fn queue_count(&self) -> usize {
        self.count.get()
    }

    /// Host side, after `process`: every (id, offset, value) the plugin wrote.
    pub fn for_each(&self, mut f: impl FnMut(u32, i32, f64)) {
        for q in self.queues.iter().take(self.count.get()) {
            for i in 0..q.count.get() {
                if let Some(p) = q.point(i) {
                    f(q.id.get(), p.offset, p.value);
                }
            }
        }
    }

    fn queue_ptr(&self, index: usize) -> *mut sv::IParamValueQueue {
        self.queues.get(index).and_then(|q| q.as_com_ref::<sv::IParamValueQueue>()).map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> sb::int32 {
        i32::try_from(self.count.get()).unwrap_or(0)
    }
    unsafe fn getParameterData(&self, index: sb::int32) -> *mut sv::IParamValueQueue {
        match usize::try_from(index) {
            Ok(i) if i < self.count.get() => self.queue_ptr(i),
            _ => std::ptr::null_mut(),
        }
    }
    unsafe fn addParameterData(&self, id: *const sv::ParamID, index: *mut sb::int32) -> *mut sv::IParamValueQueue {
        // SAFETY: in-pointer from the plugin (or null, handled).
        let Some(&id) = (unsafe { id.as_ref() }) else { return std::ptr::null_mut() };
        let Some((i, _)) = self.queue_for(id) else { return std::ptr::null_mut() };
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(out) = unsafe { index.as_mut() } {
            *out = i32::try_from(i).unwrap_or(0);
        }
        self.queue_ptr(i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queue_collects_ramp_points_and_replaces_a_point_at_the_same_offset() {
        let pc = ComWrapper::new(ParamChanges::new(4));
        assert!(pc.add(7, 0, 0.1));
        assert!(pc.add(7, 64, 0.5));
        assert!(pc.add(9, 0, 0.9));
        assert!(pc.add(7, 64, 0.6), "same offset replaces");
        assert_eq!(pc.queue_count(), 2);
        let mut seen = Vec::new();
        pc.for_each(|id, off, v| seen.push((id, off, v)));
        assert_eq!(seen, vec![(7, 0, 0.1), (7, 64, 0.6), (9, 0, 0.9)]);
        // SAFETY: the host calling its own COM methods.
        unsafe {
            assert_eq!(pc.getParameterCount(), 2);
            assert!(pc.getParameterData(2).is_null());
        }
        pc.clear();
        assert_eq!(pc.queue_count(), 0);
        assert!(pc.add(7, 5, 0.3));
        let mut seen = Vec::new();
        pc.for_each(|id, off, v| seen.push((id, off, v)));
        assert_eq!(seen, vec![(7, 5, 0.3)], "a reused queue starts empty");
    }

    #[test]
    fn full_pools_drop_instead_of_growing_and_a_full_ramp_keeps_its_last_value() {
        let pc = ComWrapper::new(ParamChanges::new(1));
        assert!(pc.add(1, 0, 0.0));
        assert!(!pc.add(2, 0, 0.0), "no queue left");
        assert_eq!(pc.take_dropped(), 1);
        for i in 1..POINTS_PER_QUEUE as u32 {
            assert!(pc.add(1, i, 0.5));
        }
        assert!(pc.add(1, 1000, 0.75), "a later point replaces the last one");
        assert!(!pc.add(1, 500, 0.1), "a point before the last one is dropped when full");
        assert_eq!(pc.take_dropped(), 2);
        let mut last = None;
        let mut n = 0;
        pc.for_each(|_, off, v| {
            n += 1;
            last = Some((off, v));
        });
        assert_eq!((n, last), (POINTS_PER_QUEUE, Some((1000, 0.75))), "the ramp ends on its final value");
    }

    #[test]
    fn points_go_in_time_order_whatever_order_they_come_in() {
        let pc = ComWrapper::new(ParamChanges::new(2));
        // Automation at 0 and 100, then a MIDI-mapped controller at 50 on the same parameter.
        pc.add(7, 0, 0.0);
        pc.add(7, 100, 1.0);
        pc.add(7, 50, 0.4);
        let mut seen = Vec::new();
        pc.for_each(|_, off, v| seen.push((off, v)));
        assert_eq!(seen, vec![(0, 0.0), (50, 0.4), (100, 1.0)]);
    }

    #[test]
    fn note_offs_have_room_kept_for_them() {
        let l = ComWrapper::new(EventList::new(NOTE_OFF_RESERVE + 2));
        let on = to_vst3_event(&Event { offset: 0, kind: EventKind::NoteOn { channel: 0, key: 60, velocity: 0.8, note_id: -1 } }).unwrap();
        let off = to_vst3_event(&Event { offset: 1, kind: EventKind::NoteOff { channel: 0, key: 60, velocity: 0.0, note_id: -1 } }).unwrap();
        assert!(l.push(on) && l.push(on));
        assert!(!l.push(on), "notes stop where the reserve starts");
        for _ in 0..NOTE_OFF_RESERVE {
            assert!(l.push(off), "note-offs still fit");
        }
        assert!(!l.push(off));
        assert_eq!(l.take_dropped(), 2);
        assert_eq!(l.take_dropped(), 0);
    }

    #[test]
    fn the_event_list_has_a_fixed_capacity() {
        let l = ComWrapper::new(EventList::new(NOTE_OFF_RESERVE + 2));
        let e = to_vst3_event(&Event { offset: 3, kind: EventKind::NoteOn { channel: 0, key: 60, velocity: 0.8, note_id: -1 } }).unwrap();
        assert!(l.push(e) && l.push(e));
        assert!(!l.push(e));
        assert_eq!(l.len(), 2);
        l.clear();
        assert_eq!(l.len(), 0);
    }

    #[test]
    fn note_events_round_trip_and_midi_goes_through_parameters() {
        let on = Event { offset: 5, kind: EventKind::NoteOn { channel: 2, key: 64, velocity: 0.5, note_id: 9 } };
        assert_eq!(from_vst3_event(&to_vst3_event(&on).unwrap()), Some(on));
        let off = Event { offset: 6, kind: EventKind::NoteOff { channel: 2, key: 64, velocity: 0.0, note_id: 9 } };
        assert_eq!(from_vst3_event(&to_vst3_event(&off).unwrap()), Some(off));
        let pp = Event { offset: 7, kind: EventKind::PolyPressure { channel: 0, key: 1, pressure: 0.25, note_id: -1 } };
        assert_eq!(from_vst3_event(&to_vst3_event(&pp).unwrap()), Some(pp));
        assert!(to_vst3_event(&Event { offset: 0, kind: EventKind::PitchBend { channel: 0, value: 0.0 } }).is_none());
    }
}
