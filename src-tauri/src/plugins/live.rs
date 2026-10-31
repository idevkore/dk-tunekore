//! A MIDI controller's keys on the selected track's instrument.
//!
//! The UI says which track has the focus (`plugins_focus`): an instrument
//! track while the Timeline shows one selected, otherwise none, and the keys
//! play the Sampler's pads as before. A MIDI input's callback hands every
//! message to [`play_live`] first. A note-off goes wherever its note-on went,
//! so a note held while the focus moves still ends.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tunekore_engine::plugins::EventKind;
use tunekore_engine::{PluginLink, TrackId};

#[derive(Default)]
pub struct LiveRoute {
    focus: Mutex<Option<(TrackId, Arc<PluginLink>)>>,
    /// Where each held note (channel, key) went.
    held: Mutex<HashMap<(u8, u8), Arc<PluginLink>>>,
}

static ROUTE: OnceLock<Arc<LiveRoute>> = OnceLock::new();

/// The one route, shared by the plugins module and the MIDI callbacks.
pub fn route() -> Arc<LiveRoute> {
    ROUTE.get_or_init(Arc::default).clone()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl LiveRoute {
    pub fn focus(&self) -> Option<TrackId> {
        lock(&self.focus).as_ref().map(|(t, _)| *t)
    }

    pub fn has_focus(&self) -> bool {
        lock(&self.focus).is_some()
    }

    pub fn set_focus(&self, to: Option<(TrackId, Arc<PluginLink>)>) {
        *lock(&self.focus) = to;
    }

    /// The focused track's instrument is now `link` (it was replaced or reloaded).
    pub fn focus_if(&self, track: TrackId, link: &Arc<PluginLink>) {
        let mut f = lock(&self.focus);
        if f.as_ref().is_some_and(|(t, _)| *t == track) {
            *f = Some((track, link.clone()));
        }
    }

    /// `old`, on `track`, was replaced by `new`: the focus follows, and notes
    /// held on `old` are forgotten (the new instance holds none).
    pub fn retarget(&self, track: TrackId, old: &Arc<PluginLink>, new: &Arc<PluginLink>) {
        {
            let mut f = lock(&self.focus);
            if f.as_ref().is_some_and(|(t, l)| *t == track && Arc::ptr_eq(l, old)) {
                *f = Some((track, new.clone()));
            }
        }
        lock(&self.held).retain(|_, l| !Arc::ptr_eq(l, old));
    }

    /// A device went away: nothing more goes to it.
    pub fn forget(&self, link: &Arc<PluginLink>) {
        {
            let mut f = lock(&self.focus);
            if f.as_ref().is_some_and(|(_, l)| Arc::ptr_eq(l, link)) {
                *f = None;
            }
        }
        lock(&self.held).retain(|_, l| !Arc::ptr_eq(l, link));
    }

    pub fn unfocus(&self, track: TrackId) {
        let mut f = lock(&self.focus);
        if f.as_ref().is_some_and(|(t, _)| *t == track) {
            *f = None;
        }
    }

    fn focused(&self) -> Option<Arc<PluginLink>> {
        lock(&self.focus).as_ref().map(|(_, l)| l.clone())
    }

    /// Plays one raw MIDI message. True when it went to an instrument (the
    /// pads don't take it then). Control changes are not taken here; see
    /// [`LiveRoute::control`].
    pub fn play(&self, msg: &[u8]) -> bool {
        let Some(kind) = parse(msg) else { return false };
        match kind {
            EventKind::NoteOn { channel, key, .. } => {
                let Some(link) = self.focused() else { return false };
                lock(&self.held).insert((channel, key), link.clone());
                link.send_live(kind);
                true
            }
            // Only a note whose note-on went to an instrument ends there: one
            // that started on a pad before the focus moved ends on the pad.
            EventKind::NoteOff { channel, key, .. } => match lock(&self.held).remove(&(channel, key)) {
                Some(link) => {
                    link.send_live(kind);
                    true
                }
                None => false,
            },
            EventKind::ControlChange { .. } => false,
            _ => match self.focused() {
                Some(link) => {
                    link.send_live(kind);
                    true
                }
                None => false,
            },
        }
    }

    /// A control change MIDI learn doesn't use: it goes to the focused
    /// instrument (the mod wheel, the sustain pedal). True when it did.
    pub fn control(&self, msg: &[u8]) -> bool {
        match (parse_cc(msg), self.focused()) {
            (Some(kind), Some(link)) => {
                link.send_live(kind);
                true
            }
            _ => false,
        }
    }
}

/// From a MIDI input's callback: see [`LiveRoute::play`].
pub fn play_live(msg: &[u8]) -> bool {
    route().play(msg)
}

/// From a MIDI input's callback: see [`LiveRoute::control`].
pub fn control_live(msg: &[u8]) -> bool {
    route().control(msg)
}

/// Whether a track's instrument has the keys.
pub fn has_focus() -> bool {
    route().has_focus()
}

/// Notes, pitch bend and pressure; `None` for anything else.
fn parse(msg: &[u8]) -> Option<EventKind> {
    let (&status, rest) = msg.split_first()?;
    let channel = status & 0x0F;
    let a = rest.first().copied().unwrap_or(0) & 0x7F;
    let b = rest.get(1).copied().unwrap_or(0) & 0x7F;
    match status & 0xF0 {
        0x90 if b > 0 => Some(EventKind::NoteOn { channel, key: a, velocity: f32::from(b) / 127.0, note_id: -1 }),
        0x90 | 0x80 => Some(EventKind::NoteOff { channel, key: a, velocity: f32::from(b) / 127.0, note_id: -1 }),
        0xA0 => Some(EventKind::PolyPressure { channel, key: a, pressure: f32::from(b) / 127.0, note_id: -1 }),
        0xD0 => Some(EventKind::ChannelPressure { channel, pressure: f32::from(a) / 127.0 }),
        0xE0 => {
            let v = (u16::from(b) << 7) | u16::from(a);
            Some(EventKind::PitchBend { channel, value: ((f32::from(v) - 8192.0) / 8192.0).clamp(-1.0, 1.0) })
        }
        0xB0 => Some(EventKind::ControlChange { channel, controller: a, value: f32::from(b) / 127.0 }),
        _ => None,
    }
}

fn parse_cc(msg: &[u8]) -> Option<EventKind> {
    parse(msg).filter(|k| matches!(k, EventKind::ControlChange { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_become_plugin_events() {
        assert_eq!(parse(&[0x91, 60, 127]), Some(EventKind::NoteOn { channel: 1, key: 60, velocity: 1.0, note_id: -1 }));
        assert_eq!(parse(&[0x90, 60, 0]), Some(EventKind::NoteOff { channel: 0, key: 60, velocity: 0.0, note_id: -1 }), "a note-on at 0 is a note-off");
        assert_eq!(parse(&[0x80, 61, 64]).map(|k| matches!(k, EventKind::NoteOff { key: 61, .. })), Some(true));
        assert_eq!(parse(&[0xE0, 0, 64]), Some(EventKind::PitchBend { channel: 0, value: 0.0 }), "the wheel's middle");
        assert_eq!(parse(&[0xE0, 0x7F, 0x7F]).map(|k| matches!(k, EventKind::PitchBend { value, .. } if value > 0.999)), Some(true));
        assert_eq!(parse(&[0xD2, 127]), Some(EventKind::ChannelPressure { channel: 2, pressure: 1.0 }));
        assert_eq!(parse(&[0xB0, 1, 127]), Some(EventKind::ControlChange { channel: 0, controller: 1, value: 1.0 }));
        assert_eq!(parse(&[0xF8]), None, "clock");
        assert_eq!(parse(&[]), None);
    }

    #[test]
    fn without_a_focus_the_pads_keep_the_notes() {
        let r = LiveRoute::default();
        assert!(!r.play(&[0x90, 36, 100]));
        assert!(!r.control(&[0xB0, 1, 10]));
    }

    fn link() -> Arc<PluginLink> {
        tunekore_engine::plugin_device(tunekore_engine::PluginRole::Instrument, Box::new(|| {})).1
    }

    #[test]
    fn a_note_ends_where_it_started_whatever_has_the_focus_now() {
        let r = LiveRoute::default();
        // Held on a pad, then the focus moves to an instrument track: the
        // note-off is the pad's, so the pad lets go.
        assert!(!r.play(&[0x90, 36, 100]), "the pads take it");
        let keys = link();
        r.set_focus(Some((TrackId(2), keys.clone())));
        assert!(!r.play(&[0x80, 36, 0]), "its note-off goes to the pads too");
        // Held on the instrument, then the focus goes: the note-off still reaches it.
        assert!(r.play(&[0x90, 60, 100]));
        r.set_focus(None);
        assert!(r.play(&[0x90, 60, 0]), "a note-on at 0 is a note-off, and it was the instrument's");
        assert!(!r.play(&[0x80, 60, 0]), "already ended");
        // Pitch bend follows the focus.
        assert!(!r.play(&[0xE0, 0, 64]));
        r.set_focus(Some((TrackId(2), keys.clone())));
        assert!(r.play(&[0xE0, 0, 64]));
        assert!(r.control(&[0xB0, 1, 10]));
        // A device that goes takes the focus and its held notes with it.
        assert!(r.play(&[0x90, 62, 100]));
        r.forget(&keys);
        assert!(!r.has_focus());
        assert!(!r.play(&[0x80, 62, 0]));
    }
}
