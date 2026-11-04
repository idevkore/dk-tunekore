//! The events `process` hands a CLAP plugin and the ones it hands back, in lists built to be
//! filled and read on the audio thread without allocating or locking, and the transport.
//!
//! Derived from soundCraft's `clap-host` (`ffi.rs`: `RawEvent`, `transport_event` and the event
//! list callbacks), MIT OR Apache-2.0; see `third_party/soundcraft/`. What changed: notes keep
//! their channel and note id, poly pressure, channel pressure, control changes and pitch bend
//! are sent (as MIDI, or poly pressure as a note expression); parameter changes are sorted in with
//! the notes, at most 32 points per parameter in a block (the last value always kept) and 256
//! parameters; note-offs have room kept for them; the plugin's output keeps notes, MIDI,
//! parameter values and gestures; everything dropped is counted; and the transport carries the
//! loop.

use crate::plugin::{Event, EventKind, Transport};
use clap_sys::events::*;
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use clap_sys::id::clap_id;

/// Room for notes and MIDI in one block, the last [`NOTE_OFF_RESERVE`] for note-offs only.
pub(crate) const NOTE_CAP: usize = 1024;
/// Room kept for note-offs, so a block full of notes can still end the ones it started.
const NOTE_OFF_RESERVE: usize = 128;
/// Parameters one block may change.
pub(crate) const PARAM_CAP: usize = 256;
/// Points one parameter may have in a block; more replace the last one.
pub(crate) const POINTS_PER_PARAM: usize = 32;
/// Parameter points one block may hold.
const POINT_CAP: usize = 2048;
/// Events the plugin may hand back in one block.
pub(crate) const OUT_CAP: usize = 1024;

/// Any CLAP event this host sends or keeps. The header comes first in each, so a pointer to the
/// union is a pointer to the event's header.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union RawEvent {
    pub header: clap_event_header,
    pub note: clap_event_note,
    pub expression: clap_event_note_expression,
    pub param: clap_event_param_value,
    pub gesture: clap_event_param_gesture,
    pub midi: clap_event_midi,
}

impl RawEvent {
    pub fn header(&self) -> &clap_event_header {
        // SAFETY: every member starts with a `clap_event_header`.
        unsafe { &self.header }
    }
}

fn header<T>(type_: u16, time: u32) -> clap_event_header {
    clap_event_header { size: size_of::<T>() as u32, time, space_id: CLAP_CORE_EVENT_SPACE_ID, type_, flags: 0 }
}

/// A parameter value event, `value` in the plugin's own units.
pub(crate) fn param_value(id: clap_id, time: u32, value: f64) -> RawEvent {
    RawEvent {
        param: clap_event_param_value {
            header: header::<clap_event_param_value>(CLAP_EVENT_PARAM_VALUE, time),
            param_id: id,
            cookie: std::ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        },
    }
}

/// How a plugin's note input takes notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Dialects {
    /// CLAP note events (note on, off, expressions).
    pub clap: bool,
    /// MIDI 1.0 messages.
    pub midi: bool,
}

fn seven_bits(x: f32) -> u8 {
    (x.clamp(0.0, 1.0) * 127.0).round() as u8
}

fn midi(status: u8, channel: u8, a: u8, b: u8, time: u32) -> RawEvent {
    RawEvent { midi: clap_event_midi { header: header::<clap_event_midi>(CLAP_EVENT_MIDI, time), port_index: 0, data: [status | (channel & 15), a & 127, b & 127] } }
}

fn note(type_: u16, channel: u8, key: u8, velocity: f32, note_id: i32, time: u32) -> RawEvent {
    RawEvent {
        note: clap_event_note {
            header: header::<clap_event_note>(type_, time),
            note_id,
            port_index: 0,
            channel: i16::from(channel & 15),
            key: i16::from(key & 127),
            velocity: f64::from(velocity.clamp(0.0, 1.0)),
        },
    }
}

/// What an [`Event`] becomes for a plugin that takes `d`: a CLAP note event where it takes those,
/// else MIDI. `None` when it takes neither (an effect), or for what only MIDI carries when it
/// doesn't take MIDI.
pub(crate) fn to_clap(e: &Event, d: Dialects) -> Option<RawEvent> {
    let t = e.offset;
    match e.kind {
        EventKind::NoteOn { channel, key, velocity, note_id } if d.clap => Some(note(CLAP_EVENT_NOTE_ON, channel, key, velocity, note_id, t)),
        // A MIDI note-on of velocity 0 is a note-off: the softest note-on is 1.
        EventKind::NoteOn { channel, key, velocity, .. } if d.midi => Some(midi(0x90, channel, key, seven_bits(velocity).max(1), t)),
        EventKind::NoteOff { channel, key, velocity, note_id } if d.clap => Some(note(CLAP_EVENT_NOTE_OFF, channel, key, velocity, note_id, t)),
        EventKind::NoteOff { channel, key, velocity, .. } if d.midi => Some(midi(0x80, channel, key, seven_bits(velocity), t)),
        EventKind::PolyPressure { channel, key, pressure, note_id } if d.clap => Some(RawEvent {
            expression: clap_event_note_expression {
                header: header::<clap_event_note_expression>(CLAP_EVENT_NOTE_EXPRESSION, t),
                expression_id: CLAP_NOTE_EXPRESSION_PRESSURE,
                note_id,
                port_index: 0,
                channel: i16::from(channel & 15),
                key: i16::from(key & 127),
                value: f64::from(pressure.clamp(0.0, 1.0)),
            },
        }),
        EventKind::PolyPressure { channel, key, pressure, .. } if d.midi => Some(midi(0xA0, channel, key, seven_bits(pressure), t)),
        EventKind::ChannelPressure { channel, pressure } if d.midi => Some(midi(0xD0, channel, seven_bits(pressure), 0, t)),
        EventKind::ControlChange { channel, controller, value } if d.midi => Some(midi(0xB0, channel, controller, seven_bits(value), t)),
        EventKind::PitchBend { channel, value } if d.midi => {
            let v = ((f64::from(value.clamp(-1.0, 1.0)) + 1.0) * 8192.0).round().clamp(0.0, 16_383.0) as u16;
            Some(midi(0xE0, channel, (v & 127) as u8, (v >> 7) as u8, t))
        }
        _ => None,
    }
}

/// Whether an event, once sent, ends a note: it may use the room kept for note-offs.
fn ends_a_note(e: &RawEvent) -> bool {
    let h = e.header();
    match h.type_ {
        CLAP_EVENT_NOTE_OFF | CLAP_EVENT_NOTE_CHOKE => true,
        // SAFETY: the type says it is a MIDI event.
        CLAP_EVENT_MIDI => {
            let d = unsafe { e.midi.data };
            d[0] & 0xF0 == 0x80 || (d[0] & 0xF0 == 0x90 && d[2] == 0)
        }
        _ => false,
    }
}

/// What a note or MIDI event from the plugin is, if it is one we pass on.
pub(crate) fn from_clap(e: &RawEvent) -> Option<Event> {
    let h = e.header();
    let offset = h.time;
    let kind = match h.type_ {
        CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF | CLAP_EVENT_NOTE_CHOKE => {
            // SAFETY: the type says it is a note event (its size was checked when it was kept).
            let n = unsafe { e.note };
            if !(0..128).contains(&n.key) {
                return None;
            }
            let (channel, key) = (n.channel.clamp(0, 15) as u8, n.key as u8);
            let velocity = if n.velocity.is_finite() { n.velocity.clamp(0.0, 1.0) as f32 } else { 0.0 };
            if h.type_ == CLAP_EVENT_NOTE_ON {
                EventKind::NoteOn { channel, key, velocity, note_id: n.note_id }
            } else {
                EventKind::NoteOff { channel, key, velocity, note_id: n.note_id }
            }
        }
        CLAP_EVENT_MIDI => {
            // SAFETY: as above, a MIDI event.
            let d = unsafe { e.midi.data };
            let channel = d[0] & 15;
            let (a, b) = (d[1] & 127, d[2] & 127);
            let unit = |x: u8| f32::from(x) / 127.0;
            match d[0] & 0xF0 {
                0x90 if b > 0 => EventKind::NoteOn { channel, key: a, velocity: unit(b), note_id: -1 },
                0x80 | 0x90 => EventKind::NoteOff { channel, key: a, velocity: unit(b), note_id: -1 },
                0xA0 => EventKind::PolyPressure { channel, key: a, pressure: unit(b), note_id: -1 },
                0xB0 => EventKind::ControlChange { channel, controller: a, value: unit(b) },
                0xD0 => EventKind::ChannelPressure { channel, pressure: unit(a) },
                0xE0 => {
                    let v = i32::from(a) | (i32::from(b) << 7);
                    EventKind::PitchBend { channel, value: ((v - 8192) as f32 / 8192.0).clamp(-1.0, 1.0) }
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(Event { offset, kind })
}

// ---- the input list -------------------------------------------------------------------------

/// Slots in the per-block parameter table: twice [`PARAM_CAP`], so probing stays short.
const TABLE: usize = PARAM_CAP * 2;
const NO_ID: u32 = u32::MAX;

/// A block's events for the plugin, sorted by time when [`InEvents::seal`] is called.
///
/// Not thread-safe by design: the audio thread fills it, the plugin reads it inside `process` on
/// that same thread, and the list is cleared before the next block.
pub(crate) struct InEvents {
    items: Vec<RawEvent>,
    /// `items` indices in time order (ties in the order added).
    order: Vec<u32>,
    notes: usize,
    points: usize,
    /// Per block: which parameters have points, how many, and where the last one is.
    ids: Box<[u32]>,
    counts: Box<[u8]>,
    last: Box<[u32]>,
    used: Vec<u16>,
    pub dropped_events: u32,
    pub dropped_points: u32,
}

impl InEvents {
    pub fn new() -> InEvents {
        let cap = NOTE_CAP + POINT_CAP;
        InEvents {
            items: Vec::with_capacity(cap),
            order: Vec::with_capacity(cap),
            notes: 0,
            points: 0,
            ids: vec![NO_ID; TABLE].into_boxed_slice(),
            counts: vec![0; TABLE].into_boxed_slice(),
            last: vec![0; TABLE].into_boxed_slice(),
            used: Vec::with_capacity(PARAM_CAP),
            dropped_events: 0,
            dropped_points: 0,
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.order.clear();
        self.notes = 0;
        self.points = 0;
        for &i in &self.used {
            self.ids[usize::from(i)] = NO_ID;
            self.counts[usize::from(i)] = 0;
        }
        self.used.clear();
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// The table slot for parameter `id` this block, claiming one if `claim`. `None` when every
    /// one of the block's [`PARAM_CAP`] parameters is taken by others.
    fn slot(&mut self, id: clap_id, claim: bool) -> Option<usize> {
        let mut i = (id.wrapping_mul(0x9E37_79B1) as usize) % TABLE;
        for _ in 0..TABLE {
            if self.ids[i] == id {
                return Some(i);
            }
            if self.ids[i] == NO_ID {
                if !claim || self.used.len() >= PARAM_CAP {
                    return None;
                }
                self.ids[i] = id;
                self.used.push(i as u16);
                return Some(i);
            }
            i = (i + 1) % TABLE;
        }
        None
    }

    /// Whether a point for `id` would be taken (for the mailbox: what is refused waits).
    pub fn can_take(&mut self, id: clap_id) -> bool {
        self.points < POINT_CAP && (self.slot(id, false).is_some() || self.used.len() < PARAM_CAP)
    }

    /// Adds a parameter point. Past [`POINTS_PER_PARAM`] for one parameter, the new point takes
    /// the place of its last one (counted as dropped), so the block still ends on the latest
    /// value. `false` (and counted) when the block's parameters or points are all taken.
    pub fn add_param(&mut self, id: clap_id, time: u32, value: f64) -> bool {
        let Some(s) = self.slot(id, true) else {
            self.dropped_points = self.dropped_points.saturating_add(1);
            return false;
        };
        if usize::from(self.counts[s]) >= POINTS_PER_PARAM {
            let i = self.last[s] as usize;
            // SAFETY: `last` names a parameter event this block added.
            let p = unsafe { &mut self.items[i].param };
            p.value = value;
            p.header.time = p.header.time.max(time);
            self.dropped_points = self.dropped_points.saturating_add(1);
            return true;
        }
        if self.points >= POINT_CAP {
            self.dropped_points = self.dropped_points.saturating_add(1);
            return false;
        }
        self.last[s] = self.items.len() as u32;
        self.counts[s] += 1;
        self.points += 1;
        self.items.push(param_value(id, time, value));
        true
    }

    /// Adds a note or MIDI event; the last [`NOTE_OFF_RESERVE`] places take ones that end notes.
    pub fn add_event(&mut self, e: RawEvent) -> bool {
        let room = if ends_a_note(&e) { NOTE_CAP } else { NOTE_CAP - NOTE_OFF_RESERVE };
        if self.notes >= room {
            self.dropped_events = self.dropped_events.saturating_add(1);
            return false;
        }
        self.notes += 1;
        self.items.push(e);
        true
    }

    /// Puts the events in time order (in place: no allocation). Call once, after adding.
    pub fn seal(&mut self) {
        self.order.clear();
        self.order.extend(0..self.items.len() as u32);
        let items = &self.items;
        self.order.sort_unstable_by_key(|&i| (items[i as usize].header().time, i));
    }

    /// Event `i` in time order.
    pub fn get(&self, i: usize) -> Option<&RawEvent> {
        self.order.get(i).and_then(|&k| self.items.get(k as usize))
    }

    pub fn as_clap(&self) -> clap_input_events {
        clap_input_events { ctx: (self as *const InEvents).cast_mut().cast(), size: Some(in_size), get: Some(in_get) }
    }
}

unsafe extern "C" fn in_size(list: *const clap_input_events) -> u32 {
    // SAFETY: `list` is the struct `as_clap` made, whose `ctx` is the `InEvents`, alive and not
    // changed while the plugin reads it.
    unsafe { list.as_ref().and_then(|l| (l.ctx as *const InEvents).as_ref()) }.map_or(0, |l| l.order.len() as u32)
}

unsafe extern "C" fn in_get(list: *const clap_input_events, index: u32) -> *const clap_event_header {
    // SAFETY: as in `in_size`.
    let l = unsafe { list.as_ref().and_then(|l| (l.ctx as *const InEvents).as_ref()) };
    l.and_then(|l| l.get(index as usize)).map_or(std::ptr::null(), |e| e.header() as *const clap_event_header)
}

// ---- the output list ------------------------------------------------------------------------

/// What the plugin hands back in a block: notes, MIDI, parameter values and gestures, up to
/// [`OUT_CAP`]. Same threading as [`InEvents`].
pub(crate) struct OutEvents {
    items: Vec<RawEvent>,
    pub dropped: u32,
}

impl OutEvents {
    pub fn new() -> OutEvents {
        OutEvents { items: Vec::with_capacity(OUT_CAP), dropped: 0 }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn items(&self) -> &[RawEvent] {
        &self.items
    }

    /// The list for the plugin. The pointer is only valid while `self` is neither moved nor
    /// touched; the caller keeps it to one plugin call.
    pub fn as_clap(&mut self) -> clap_output_events {
        clap_output_events { ctx: (self as *mut OutEvents).cast(), try_push: Some(out_push) }
    }

    fn push(&mut self, h: &clap_event_header, event: *const clap_event_header) -> bool {
        let fits = |n: usize| h.size as usize >= n;
        // SAFETY (each read): the header's type and size say the whole struct is there.
        let kept = match h.type_ {
            CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF | CLAP_EVENT_NOTE_CHOKE if fits(size_of::<clap_event_note>()) => RawEvent { note: unsafe { *event.cast::<clap_event_note>() } },
            CLAP_EVENT_MIDI if fits(size_of::<clap_event_midi>()) => RawEvent { midi: unsafe { *event.cast::<clap_event_midi>() } },
            CLAP_EVENT_PARAM_VALUE if fits(size_of::<clap_event_param_value>()) => RawEvent { param: unsafe { *event.cast::<clap_event_param_value>() } },
            CLAP_EVENT_PARAM_GESTURE_BEGIN | CLAP_EVENT_PARAM_GESTURE_END if fits(size_of::<clap_event_param_gesture>()) => {
                RawEvent { gesture: unsafe { *event.cast::<clap_event_param_gesture>() } }
            }
            // Note ends, modulation, expressions and the rest: accepted, not kept.
            _ => return true,
        };
        if self.items.len() >= OUT_CAP {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.items.push(kept);
        true
    }
}

unsafe extern "C" fn out_push(list: *const clap_output_events, event: *const clap_event_header) -> bool {
    if event.is_null() {
        return false;
    }
    // SAFETY: `list` is the struct `as_clap` made, whose `ctx` is the `OutEvents`, used by nothing
    // else during the plugin call that hands us this event; the header is valid for the call.
    let (Some(out), Some(h)) = (unsafe { list.as_ref().and_then(|l| (l.ctx as *mut OutEvents).as_mut()) }, unsafe { event.as_ref() }) else { return false };
    if h.space_id != CLAP_CORE_EVENT_SPACE_ID {
        return true;
    }
    out.push(h, event)
}

// ---- the transport --------------------------------------------------------------------------

fn fixed(x: f64, factor: i64) -> i64 {
    if x.is_finite() { (x * factor as f64).round() as i64 } else { 0 }
}

/// The transport event for a block's start, at `sample_rate`. Seconds are worked out at the
/// transport's tempo (the engine's tempo is constant across a block and, so far, a project).
pub(crate) fn transport(t: &Transport, sample_rate: f64) -> clap_event_transport {
    let mut flags = CLAP_TRANSPORT_HAS_TEMPO | CLAP_TRANSPORT_HAS_BEATS_TIMELINE | CLAP_TRANSPORT_HAS_SECONDS_TIMELINE | CLAP_TRANSPORT_HAS_TIME_SIGNATURE;
    if t.playing {
        flags |= CLAP_TRANSPORT_IS_PLAYING;
    }
    if t.recording {
        flags |= CLAP_TRANSPORT_IS_RECORDING;
    }
    if t.looping {
        flags |= CLAP_TRANSPORT_IS_LOOP_ACTIVE;
    }
    let tempo = if t.tempo.is_finite() && t.tempo > 0.0 { t.tempo } else { 120.0 };
    let seconds = |beats: f64| beats * 60.0 / tempo;
    let bar_beats = f64::from(t.time_sig_numerator.max(1)) * 4.0 / f64::from(t.time_sig_denominator.max(1));
    let bar = (t.bar_start_beats / bar_beats).round();
    clap_event_transport {
        header: header::<clap_event_transport>(CLAP_EVENT_TRANSPORT, 0),
        flags,
        song_pos_beats: fixed(t.project_beats, CLAP_BEATTIME_FACTOR),
        song_pos_seconds: fixed(t.project_samples as f64 / sample_rate.max(1.0), CLAP_SECTIME_FACTOR),
        tempo,
        tempo_inc: 0.0,
        loop_start_beats: fixed(t.loop_start_beats, CLAP_BEATTIME_FACTOR),
        loop_end_beats: fixed(t.loop_end_beats, CLAP_BEATTIME_FACTOR),
        loop_start_seconds: fixed(seconds(t.loop_start_beats), CLAP_SECTIME_FACTOR),
        loop_end_seconds: fixed(seconds(t.loop_end_beats), CLAP_SECTIME_FACTOR),
        bar_start: fixed(t.bar_start_beats, CLAP_BEATTIME_FACTOR),
        bar_number: if bar.is_finite() { bar.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32 } else { 0 },
        tsig_num: u16::try_from(t.time_sig_numerator.max(1)).unwrap_or(4),
        tsig_denom: u16::try_from(t.time_sig_denominator.max(1)).unwrap_or(4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: Dialects = Dialects { clap: true, midi: true };
    const MIDI: Dialects = Dialects { clap: false, midi: true };

    fn ev(offset: u32, kind: EventKind) -> Event {
        Event { offset, kind }
    }

    #[test]
    fn notes_go_as_clap_notes_or_midi_and_come_back() {
        let on = ev(3, EventKind::NoteOn { channel: 2, key: 60, velocity: 0.5, note_id: 9 });
        let c = to_clap(&on, BOTH).unwrap();
        assert_eq!(c.header().type_, CLAP_EVENT_NOTE_ON);
        // SAFETY: a note event.
        let n = unsafe { c.note };
        assert_eq!((n.channel, n.key, n.velocity, n.note_id, n.header.time), (2, 60, 0.5, 9, 3));
        assert_eq!(from_clap(&c), Some(on), "exactly, through CLAP notes");
        let m = to_clap(&on, MIDI).unwrap();
        // SAFETY: a MIDI event.
        assert_eq!(unsafe { m.midi.data }, [0x92, 60, 64]);
        let back = from_clap(&m).unwrap();
        assert!(matches!(back.kind, EventKind::NoteOn { channel: 2, key: 60, note_id: -1, .. }));
        // The softest MIDI note-on is velocity 1, never a note-off.
        let quiet = to_clap(&ev(0, EventKind::NoteOn { channel: 0, key: 1, velocity: 0.0, note_id: -1 }), MIDI).unwrap();
        assert_eq!(unsafe { quiet.midi.data }[2], 1);
        assert!(to_clap(&on, Dialects::default()).is_none(), "an effect takes no notes");
    }

    #[test]
    fn controllers_bend_and_pressure_go_as_midi() {
        let cc = to_clap(&ev(0, EventKind::ControlChange { channel: 1, controller: 7, value: 1.0 }), BOTH).unwrap();
        assert_eq!(unsafe { cc.midi.data }, [0xB1, 7, 127]);
        let centre = to_clap(&ev(0, EventKind::PitchBend { channel: 0, value: 0.0 }), BOTH).unwrap();
        assert_eq!(unsafe { centre.midi.data }, [0xE0, 0, 64], "8192 is the centre");
        assert_eq!(from_clap(&centre).map(|e| e.kind), Some(EventKind::PitchBend { channel: 0, value: 0.0 }));
        let top = to_clap(&ev(0, EventKind::PitchBend { channel: 0, value: 1.0 }), BOTH).unwrap();
        assert_eq!(unsafe { top.midi.data }, [0xE0, 127, 127]);
        let poly = to_clap(&ev(0, EventKind::PolyPressure { channel: 0, key: 60, pressure: 0.25, note_id: 4 }), BOTH).unwrap();
        assert_eq!(poly.header().type_, CLAP_EVENT_NOTE_EXPRESSION, "a note expression where CLAP notes are taken");
        let poly = to_clap(&ev(0, EventKind::PolyPressure { channel: 0, key: 60, pressure: 1.0, note_id: 4 }), MIDI).unwrap();
        assert_eq!(unsafe { poly.midi.data }, [0xA0, 60, 127]);
        let cp = to_clap(&ev(0, EventKind::ChannelPressure { channel: 3, pressure: 0.0 }), MIDI).unwrap();
        assert_eq!(unsafe { cp.midi.data }, [0xD3, 0, 0]);
        assert!(to_clap(&ev(0, EventKind::ControlChange { channel: 0, controller: 1, value: 0.5 }), Dialects { clap: true, midi: false }).is_none());
    }

    #[test]
    fn the_list_sorts_by_time_with_parameters_first_at_a_time() {
        let mut l = InEvents::new();
        l.add_param(7, 10, 0.5);
        l.add_event(to_clap(&ev(10, EventKind::NoteOn { channel: 0, key: 60, velocity: 1.0, note_id: 1 }), BOTH).unwrap());
        l.add_event(to_clap(&ev(2, EventKind::NoteOff { channel: 0, key: 61, velocity: 0.0, note_id: 2 }), BOTH).unwrap());
        l.seal();
        let types: Vec<(u32, u16)> = (0..l.len()).map(|i| l.get(i).unwrap().header()).map(|h| (h.time, h.type_)).collect();
        assert_eq!(types, vec![(2, CLAP_EVENT_NOTE_OFF), (10, CLAP_EVENT_PARAM_VALUE), (10, CLAP_EVENT_NOTE_ON)]);
        let raw = l.as_clap();
        // SAFETY: the list's own callbacks.
        assert_eq!(unsafe { (raw.size.unwrap())(&raw) }, 3);
        assert!(unsafe { (raw.get.unwrap())(&raw, 3) }.is_null());
        l.clear();
        assert_eq!(l.len(), 0);
    }

    #[test]
    fn a_parameter_keeps_its_last_value_when_its_points_run_out() {
        let mut l = InEvents::new();
        for i in 0..40u32 {
            assert!(l.add_param(7, i, f64::from(i)));
        }
        assert_eq!(l.len(), POINTS_PER_PARAM);
        assert_eq!(l.dropped_points, 40 - POINTS_PER_PARAM as u32);
        l.seal();
        // SAFETY: parameter events.
        let last = unsafe { l.get(l.len() - 1).unwrap().param };
        assert_eq!((last.header.time, last.value), (39, 39.0), "the block ends on the latest value");
        // More parameters than a block takes: the rest are refused.
        let mut l = InEvents::new();
        for id in 0..300 {
            l.add_param(id, 0, 0.0);
        }
        assert_eq!((l.len(), l.dropped_points), (PARAM_CAP, 300 - PARAM_CAP as u32));
        assert!(!l.can_take(1000) && l.can_take(5), "a parameter already in the block still fits");
        l.clear();
        assert!(l.can_take(1000), "cleared for the next block");
    }

    #[test]
    fn note_offs_have_room_kept_for_them() {
        let mut l = InEvents::new();
        let on = to_clap(&ev(0, EventKind::NoteOn { channel: 0, key: 60, velocity: 1.0, note_id: 1 }), BOTH).unwrap();
        let off = to_clap(&ev(0, EventKind::NoteOff { channel: 0, key: 60, velocity: 0.0, note_id: 1 }), BOTH).unwrap();
        while l.add_event(on) {}
        assert_eq!(l.len(), NOTE_CAP - NOTE_OFF_RESERVE);
        for _ in 0..NOTE_OFF_RESERVE {
            assert!(l.add_event(off));
        }
        assert!(!l.add_event(off), "full");
        assert_eq!(l.dropped_events, 2);
    }

    #[test]
    fn the_plugin_output_keeps_what_the_host_uses_and_counts_what_doesnt_fit() {
        let mut out = OutEvents::new();
        let raw = out.as_clap();
        let pv = param_value(7, 4, 1.5);
        let end = clap_event_note { header: header::<clap_event_note>(CLAP_EVENT_NOTE_END, 0), note_id: 1, port_index: 0, channel: 0, key: 60, velocity: 0.0 };
        let mut short = pv;
        short.header.size = 8;
        // SAFETY: the list's own callback, with valid events.
        unsafe {
            assert!((raw.try_push.unwrap())(&raw, pv.header()));
            assert!((raw.try_push.unwrap())(&raw, &end.header), "note ends are accepted but not kept");
            assert!((raw.try_push.unwrap())(&raw, short.header()), "a truncated event is ignored");
            assert!(!(raw.try_push.unwrap())(&raw, std::ptr::null()));
        }
        assert_eq!(out.items().len(), 1);
        // SAFETY: a parameter event.
        assert_eq!(unsafe { out.items()[0].param.value }, 1.5);
        let raw = out.as_clap();
        for _ in 0..OUT_CAP + 5 {
            // SAFETY: as above.
            unsafe { (raw.try_push.unwrap())(&raw, pv.header()) };
        }
        assert_eq!((out.items().len(), out.dropped), (OUT_CAP, 6));
    }

    #[test]
    fn the_transport_reaches_clap_in_fixed_point() {
        let t = Transport {
            playing: true,
            looping: true,
            tempo: 120.0,
            time_sig_numerator: 3,
            time_sig_denominator: 4,
            project_samples: 96_000,
            project_beats: 4.0,
            bar_start_beats: 3.0,
            loop_start_beats: 2.0,
            loop_end_beats: 6.0,
            ..Transport::default()
        };
        let c = transport(&t, 48_000.0);
        assert_eq!(c.flags & CLAP_TRANSPORT_IS_PLAYING, CLAP_TRANSPORT_IS_PLAYING);
        assert_eq!(c.flags & CLAP_TRANSPORT_IS_LOOP_ACTIVE, CLAP_TRANSPORT_IS_LOOP_ACTIVE);
        assert_eq!(c.flags & CLAP_TRANSPORT_IS_RECORDING, 0);
        assert_eq!(c.song_pos_beats, 4 * CLAP_BEATTIME_FACTOR);
        assert_eq!(c.song_pos_seconds, 2 * CLAP_SECTIME_FACTOR, "96,000 samples at 48 kHz");
        assert_eq!((c.loop_start_seconds, c.loop_end_seconds), (CLAP_SECTIME_FACTOR, 3 * CLAP_SECTIME_FACTOR), "beats at 120 bpm");
        assert_eq!((c.bar_start, c.bar_number), (3 * CLAP_BEATTIME_FACTOR, 1), "3/4: the second bar starts on beat 3");
        assert_eq!((c.tsig_num, c.tsig_denom, c.tempo), (3, 4, 120.0));
        let odd = transport(&Transport { time_sig_numerator: 0, time_sig_denominator: 0, ..t }, 48_000.0);
        assert_eq!((odd.tsig_num, odd.tsig_denom), (1, 1), "never a zero the plugin divides by");
    }
}
