//! MIDI controller input: notes C1 to D#2 (36 to 51) play pads 1 to 16.
//!
//! A background thread connects to every MIDI input port and looks again
//! every two seconds, so plugging a controller in just works. The midir
//! callback runs on the driver's thread; it only parses the message and
//! touches the Sampler's atomics, so it never blocks.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use midir::{MidiInput, MidiInputConnection};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tunekore_engine::{SamplerHandle, PADS};

/// The note that plays pad 1.
pub const FIRST_NOTE: u8 = 36;
const SCAN_EVERY: Duration = Duration::from_secs(2);
/// The activity event goes out at most this often (about 30 a second).
const ACTIVITY_EVERY: Duration = Duration::from_millis(33);
const NO_PAD: i32 = -1;

/// What a MIDI message does to a pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadEvent {
    /// A note on, with its velocity 1..=127.
    On { pad: usize, velocity: u8 },
    Off { pad: usize },
}

/// Reads one raw MIDI message. Only notes 36 to 51 on any channel count; a
/// note on with velocity 0 is a note off. Everything else is ignored.
pub fn parse(msg: &[u8]) -> Option<PadEvent> {
    let (&status, rest) = msg.split_first()?;
    let (&note, rest) = rest.split_first()?;
    let pad = note.checked_sub(FIRST_NOTE)? as usize;
    if pad >= PADS {
        return None;
    }
    match status & 0xF0 {
        0x90 => match rest.first().copied().unwrap_or(0) {
            0 => Some(PadEvent::Off { pad }),
            velocity => Some(PadEvent::On { pad, velocity }),
        },
        0x80 => Some(PadEvent::Off { pad }),
        _ => None,
    }
}

/// A MIDI input port, as the UI lists it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MidiInputInfo {
    pub name: String,
    pub connected: bool,
}

#[derive(Default)]
pub struct MidiState {
    inputs: Mutex<Vec<MidiInputInfo>>,
    /// The last pad played since the activity thread looked, or `NO_PAD`.
    activity: AtomicI32,
}

impl MidiState {
    fn new() -> Self {
        Self { inputs: Mutex::default(), activity: AtomicI32::new(NO_PAD) }
    }
}

/// Plays and releases pads from a midir callback. Touches only atomics.
fn handle(pads: &SamplerHandle, state: &MidiState, msg: &[u8]) {
    match parse(msg) {
        Some(PadEvent::On { pad, velocity }) => {
            pads.trigger_velocity(pad, velocity as f32 / 127.0);
            state.activity.store(pad as i32, Ordering::Relaxed);
        }
        Some(PadEvent::Off { pad }) => pads.release(pad),
        None => {}
    }
}

/// The ports now present, as (id, name).
fn scan() -> Result<Vec<(String, String)>, String> {
    // A new client each time: CoreMIDI only reports a plugged-in device to a client made after it.
    let input = MidiInput::new("tuneKore scan").map_err(|e| e.to_string())?;
    Ok(input
        .ports()
        .iter()
        .filter_map(|p| Some((p.id(), input.port_name(p).ok()?)))
        .collect())
}

fn connect(id: &str, name: &str, pads: SamplerHandle, state: Arc<MidiState>) -> Result<MidiInputConnection<()>, String> {
    let input = MidiInput::new("tuneKore").map_err(|e| e.to_string())?;
    let port = input.find_port_by_id(id).ok_or("the port is gone")?;
    input
        .connect(&port, name, move |_stamp, msg, _| handle(&pads, &state, msg), ())
        .map_err(|e| e.to_string())
}

/// Connects to every input now and every two seconds after, and sends
/// `midi:activity` (the pad's index) at most 30 times a second.
pub fn spawn(app: AppHandle, pads: SamplerHandle, state: Arc<MidiState>) {
    let st = state.clone();
    std::thread::spawn(move || {
        let mut open: HashMap<String, MidiInputConnection<()>> = HashMap::new();
        loop {
            match scan() {
                Ok(ports) => {
                    // Drop the ones that went away, connect the new ones.
                    open.retain(|id, _| ports.iter().any(|(p, _)| p == id));
                    for (id, name) in &ports {
                        if !open.contains_key(id) {
                            if let Ok(conn) = connect(id, name, pads.clone(), st.clone()) {
                                open.insert(id.clone(), conn);
                            }
                        }
                    }
                    let list = ports
                        .iter()
                        .map(|(id, name)| MidiInputInfo { name: name.clone(), connected: open.contains_key(id) })
                        .collect();
                    *st.inputs.lock().unwrap() = list;
                }
                Err(e) => {
                    open.clear();
                    st.inputs.lock().unwrap().clear();
                    eprintln!("no MIDI: {e}");
                }
            }
            std::thread::sleep(SCAN_EVERY);
        }
    });
    std::thread::spawn(move || loop {
        std::thread::sleep(ACTIVITY_EVERY);
        let pad = state.activity.swap(NO_PAD, Ordering::Relaxed);
        if pad != NO_PAD {
            let _ = app.emit("midi:activity", pad);
        }
    });
}

pub fn state() -> Arc<MidiState> {
    Arc::new(MidiState::new())
}

/// The MIDI inputs found so far, and whether each is connected.
#[tauri::command]
pub fn midi_inputs(state: State<'_, Arc<MidiState>>) -> Vec<MidiInputInfo> {
    state.inputs.lock().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_on_plays_pads_36_to_51() {
        assert_eq!(parse(&[0x90, 36, 100]), Some(PadEvent::On { pad: 0, velocity: 100 }));
        assert_eq!(parse(&[0x90, 51, 1]), Some(PadEvent::On { pad: 15, velocity: 1 }));
        assert_eq!(parse(&[0x90, 35, 100]), None);
        assert_eq!(parse(&[0x90, 52, 100]), None);
    }

    #[test]
    fn any_channel_counts() {
        for ch in 0..16u8 {
            assert_eq!(parse(&[0x90 | ch, 40, 64]), Some(PadEvent::On { pad: 4, velocity: 64 }));
            assert_eq!(parse(&[0x80 | ch, 40, 0]), Some(PadEvent::Off { pad: 4 }));
        }
    }

    #[test]
    fn note_on_with_velocity_zero_is_a_note_off() {
        assert_eq!(parse(&[0x90, 38, 0]), Some(PadEvent::Off { pad: 2 }));
        assert_eq!(parse(&[0x80, 38, 64]), Some(PadEvent::Off { pad: 2 }));
    }

    #[test]
    fn other_messages_are_ignored() {
        assert_eq!(parse(&[0xB0, 36, 100]), None); // control change
        assert_eq!(parse(&[0xE0, 36, 100]), None); // pitch bend
        assert_eq!(parse(&[0xF8]), None); // clock
        assert_eq!(parse(&[0x90]), None); // cut short
        assert_eq!(parse(&[]), None);
    }
}
