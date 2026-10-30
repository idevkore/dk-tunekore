//! Tracks as the main thread sees them: an id, a name, mix settings, and a
//! peak meter the UI reads without the engine's lock.
//!
//! The track list itself lives in [`crate::graph::Mixer`], which turns it
//! into the audio thread's snapshot.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// The most tracks the engine runs at once.
pub const TRACK_SLOTS: usize = 64;

/// A track's id: stable for the life of the track, and saved in projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrackId(pub u32);

/// What a track is, so each module knows which tracks are its own: the
/// Sampler's (track 1), an audio track (the Timeline's clips play on it), or
/// an instrument track (a plugin instrument, #37). Saved in projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Sampler,
    #[default]
    Audio,
    Instrument,
}

/// A track's level, pan, mute and solo.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TrackMix {
    /// Linear gain, 0..4 (1 = 0 dB).
    pub volume: f32,
    /// -1 (left) to 1 (right). The middle leaves both sides at full level.
    pub pan: f32,
    pub mute: bool,
    /// While any track is soloed, only soloed tracks are heard.
    pub solo: bool,
}

impl Default for TrackMix {
    fn default() -> Self {
        Self { volume: 1.0, pan: 0.0, mute: false, solo: false }
    }
}

impl TrackMix {
    /// Clamped to range; NaNs read as the defaults.
    pub fn sane(self) -> Self {
        let clean = |v: f32, lo: f32, hi: f32, d: f32| if v.is_nan() { d } else { v.clamp(lo, hi) };
        Self { volume: clean(self.volume, 0.0, 4.0, 1.0), pan: clean(self.pan, -1.0, 1.0, 0.0), ..self }
    }

    /// Left and right gains with the pan applied: the side panned away from
    /// falls, the other stays put, so the middle is exactly the volume.
    pub fn gains(&self) -> [f32; 2] {
        [self.volume * (1.0 - self.pan).min(1.0), self.volume * (1.0 + self.pan).min(1.0)]
    }
}

/// What the UI needs to know about one track.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub id: TrackId,
    pub name: String,
    pub kind: TrackKind,
    pub mix: TrackMix,
    /// Whether something is in the source slot.
    pub has_source: bool,
    /// Devices after the source.
    pub chain: usize,
}

/// Every track's peak since the last read, by slot. The audio thread raises
/// them; the UI takes them. Atomics only.
#[derive(Debug)]
pub struct TrackMeters {
    peaks: [AtomicU32; TRACK_SLOTS],
}

impl Default for TrackMeters {
    fn default() -> Self {
        Self { peaks: std::array::from_fn(|_| AtomicU32::new(0)) }
    }
}

impl TrackMeters {
    pub(crate) fn raise(&self, slot: usize, v: f32) {
        let Some(a) = self.peaks.get(slot) else { return };
        let mut seen = a.load(Ordering::Relaxed);
        while v > f32::from_bits(seen) {
            match a.compare_exchange_weak(seen, v.to_bits(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(s) => seen = s,
            }
        }
    }

    pub(crate) fn take(&self, slot: usize) -> f32 {
        self.peaks.get(slot).map_or(0.0, |a| f32::from_bits(a.swap(0, Ordering::Relaxed)))
    }
}

/// Reads one track's level from any thread, without the engine's lock.
#[derive(Debug, Clone)]
pub struct TrackMeter {
    pub(crate) meters: Arc<TrackMeters>,
    pub(crate) slot: usize,
}

impl TrackMeter {
    /// The track's peak since the last call, after its volume and pan.
    pub fn take_peak(&self) -> f32 {
        self.meters.take(self.slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_middle_is_exactly_the_volume_and_a_pan_turns_one_side_down() {
        assert_eq!(TrackMix::default().gains(), [1.0, 1.0]);
        assert_eq!(TrackMix { volume: 0.5, pan: 1.0, ..Default::default() }.gains(), [0.0, 0.5]);
        assert_eq!(TrackMix { pan: -0.5, ..Default::default() }.gains(), [1.0, 0.5]);
    }

    #[test]
    fn mix_settings_are_clamped() {
        let m = TrackMix { volume: 9.0, pan: f32::NAN, ..Default::default() }.sane();
        assert_eq!((m.volume, m.pan), (4.0, 0.0));
    }

    #[test]
    fn meters_hold_the_peak_until_read() {
        let m = Arc::new(TrackMeters::default());
        let meter = TrackMeter { meters: m.clone(), slot: 3 };
        m.raise(3, 0.25);
        m.raise(3, 0.5);
        m.raise(3, 0.1);
        m.raise(TRACK_SLOTS, 1.0); // ignored
        assert_eq!(meter.take_peak(), 0.5);
        assert_eq!(meter.take_peak(), 0.0);
    }
}
