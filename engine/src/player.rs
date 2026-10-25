//! Plays a set of tracks in sync (the Analyzer's stems), with level, mute
//! and solo per track.
//!
//! The control side prepares a [`Clip`] at the device's rate and hands it to
//! the audio thread through a lock-free queue. The audio thread hands the old
//! clip back through a second queue, so it never frees memory. Everything else
//! the control side changes is an atomic.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// One track's audio, `channels[c][frame]`. Shared, so the app and the
/// player hold the same samples when no resampling is needed.
pub type Track = Arc<Vec<Vec<f32>>>;

/// The most tracks a clip can have.
pub const MAX_TRACKS: usize = 8;
const NO_SEEK: u64 = u64::MAX;
/// A full-scale level change takes about this long, so mute and solo don't click.
const RAMP_SECONDS: f32 = 0.008;

/// Audio ready to play, at the device's rate. Each track has one or two
/// channels of at least `frames` frames.
#[derive(Debug, Default)]
pub struct Clip {
    pub tracks: Vec<Track>,
    pub frames: usize,
}

#[derive(Debug)]
struct TrackCtl {
    gain: AtomicU32, // f32 bits
    mute: AtomicBool,
    solo: AtomicBool,
}

impl Default for TrackCtl {
    fn default() -> Self {
        Self { gain: AtomicU32::new(1.0f32.to_bits()), mute: AtomicBool::new(false), solo: AtomicBool::new(false) }
    }
}

/// State shared by the control side and the audio thread. Atomics only.
#[derive(Debug)]
pub struct PlayerShared {
    playing: AtomicBool,
    position: AtomicU64, // frames, written by the audio thread
    seek: AtomicU64,     // a frame to jump to, or NO_SEEK
    frames: AtomicU64,   // the loaded clip's length
    rate: AtomicU32,     // the loaded clip's rate (the device's)
    tracks: [TrackCtl; MAX_TRACKS],
}

impl Default for PlayerShared {
    fn default() -> Self {
        Self {
            playing: AtomicBool::new(false),
            position: AtomicU64::new(0),
            seek: AtomicU64::new(NO_SEEK),
            frames: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            tracks: Default::default(),
        }
    }
}

impl PlayerShared {
    pub fn set_playing(&self, on: bool) {
        self.playing.store(on, Ordering::Release);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    /// Jumps to `seconds` from the start. Takes effect on the next audio block.
    pub fn seek_seconds(&self, seconds: f64) {
        let rate = self.rate.load(Ordering::Relaxed) as f64;
        let frame = (seconds.max(0.0) * rate).round() as u64;
        self.position.store(frame.min(self.frames.load(Ordering::Relaxed)), Ordering::Relaxed);
        self.seek.store(frame, Ordering::Release);
    }

    pub fn position_seconds(&self) -> f64 {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return 0.0;
        }
        self.position.load(Ordering::Relaxed) as f64 / rate as f64
    }

    pub fn duration_seconds(&self) -> f64 {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return 0.0;
        }
        self.frames.load(Ordering::Relaxed) as f64 / rate as f64
    }

    /// Level (linear gain), mute and solo for one track. Ignores a bad index.
    pub fn set_track(&self, index: usize, gain: f32, mute: bool, solo: bool) {
        let Some(t) = self.tracks.get(index) else { return };
        t.gain.store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
        t.mute.store(mute, Ordering::Relaxed);
        t.solo.store(solo, Ordering::Relaxed);
    }

    pub(crate) fn loaded(&self, frames: usize, rate: u32) {
        self.frames.store(frames as u64, Ordering::Relaxed);
        self.rate.store(rate, Ordering::Relaxed);
    }
}

/// The audio-thread half. Lives in the stream callback.
pub struct Voice {
    clip: Option<Box<Clip>>,
    incoming: rtrb::Consumer<Box<Clip>>,
    garbage: rtrb::Producer<Box<Clip>>,
    /// A clip waiting to go back to the control side because its queue was full.
    held: Option<Box<Clip>>,
    gains: [f32; MAX_TRACKS],
    level: f32, // master fade for play and pause
    step: f32,
    pos: usize,
}

/// The control side's ends of the two queues.
pub struct VoiceLink {
    pub(crate) send: rtrb::Producer<Box<Clip>>,
    pub(crate) returned: rtrb::Consumer<Box<Clip>>,
}

impl VoiceLink {
    /// Frees clips the audio thread has finished with. Control thread only.
    pub fn collect(&mut self) {
        while self.returned.pop().is_ok() {}
    }
}

/// A new voice for a stream at `sample_rate`, and the control side's link to it.
pub fn voice(sample_rate: u32) -> (Voice, VoiceLink) {
    let (send, incoming) = rtrb::RingBuffer::new(4);
    let (garbage, returned) = rtrb::RingBuffer::new(8);
    let step = 1.0 / (sample_rate as f32 * RAMP_SECONDS).max(1.0);
    let voice =
        Voice { clip: None, incoming, garbage, held: None, gains: [0.0; MAX_TRACKS], level: 0.0, step, pos: 0 };
    (voice, VoiceLink { send, returned })
}

impl Voice {
    /// Adds the playing tracks into `out` (interleaved, `channels` wide).
    /// Real-time safe: no allocation, locks, logging or I/O.
    pub fn render(&mut self, shared: &PlayerShared, out: &mut [f32], channels: usize) {
        if let Some(old) = self.held.take() {
            self.retire(old);
        }
        if let Ok(new) = self.incoming.pop() {
            self.pos = self.pos.min(new.frames);
            if let Some(old) = self.clip.replace(new) {
                self.retire(old);
            }
        }
        let seek = shared.seek.swap(NO_SEEK, Ordering::Acquire);
        let Some(clip) = self.clip.as_deref() else { return };
        if seek != NO_SEEK {
            self.pos = (seek as usize).min(clip.frames);
        }

        let playing = shared.playing.load(Ordering::Acquire);
        if !playing && self.level == 0.0 {
            return;
        }
        let channels = channels.max(1);
        let frames = out.len() / channels;
        let target_level = if playing { 1.0 } else { 0.0 };

        let ntracks = clip.tracks.len().min(MAX_TRACKS);
        let any_solo = shared.tracks[..ntracks].iter().any(|t| t.solo.load(Ordering::Relaxed));
        let mut targets = [0.0f32; MAX_TRACKS];
        for (t, target) in targets.iter_mut().enumerate().take(ntracks) {
            let ctl = &shared.tracks[t];
            let audible = !ctl.mute.load(Ordering::Relaxed) && (!any_solo || ctl.solo.load(Ordering::Relaxed));
            *target = if audible { f32::from_bits(ctl.gain.load(Ordering::Relaxed)) } else { 0.0 };
        }

        for frame in 0..frames {
            if self.pos >= clip.frames {
                // The end: stop, and start from the top next time.
                shared.playing.store(false, Ordering::Release);
                self.pos = 0;
                self.level = 0.0;
                break;
            }
            self.level = approach(self.level, target_level, self.step);
            let mut left = 0.0f32;
            let mut right = 0.0f32;
            for t in 0..ntracks {
                self.gains[t] = approach(self.gains[t], targets[t], self.step);
                let g = self.gains[t];
                if g == 0.0 {
                    continue;
                }
                let track = &clip.tracks[t];
                let l = track[0][self.pos];
                let r = if track.len() > 1 { track[1][self.pos] } else { l };
                left += g * l;
                right += g * r;
            }
            left *= self.level;
            right *= self.level;
            let o = &mut out[frame * channels..(frame + 1) * channels];
            if channels == 1 {
                o[0] += 0.5 * (left + right);
            } else {
                o[0] += left;
                o[1] += right;
            }
            self.pos += 1;
            if !playing && self.level == 0.0 {
                break; // paused: hold the position where the fade ended
            }
        }
        shared.position.store(self.pos as u64, Ordering::Relaxed);
    }

    fn retire(&mut self, clip: Box<Clip>) {
        if let Err(rtrb::PushError::Full(clip)) = self.garbage.push(clip) {
            self.held = Some(clip); // try again next block rather than free it here
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

    fn clip(tracks: &[f32], frames: usize) -> Box<Clip> {
        Box::new(Clip { tracks: tracks.iter().map(|&v| Arc::new(vec![vec![v; frames], vec![-v; frames]])).collect(), frames })
    }

    fn run(voice: &mut Voice, shared: &PlayerShared, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        voice.render(shared, &mut out, 2);
        out
    }

    fn setup(tracks: &[f32], frames: usize) -> (Voice, VoiceLink, PlayerShared) {
        let (mut voice, mut link, shared) = {
            let (v, l) = voice(48_000);
            (v, l, PlayerShared::default())
        };
        link.send.push(clip(tracks, frames)).unwrap();
        shared.loaded(frames, 48_000);
        shared.set_playing(true);
        run(&mut voice, &shared, 1000); // past the fade-in
        (voice, link, shared)
    }

    #[test]
    fn sums_tracks_into_left_and_right() {
        let (mut v, _l, s) = setup(&[0.1, 0.2], 48_000);
        let out = run(&mut v, &s, 100);
        assert!((out[0] - 0.3).abs() < 1e-6 && (out[1] + 0.3).abs() < 1e-6, "{:?}", &out[..2]);
    }

    #[test]
    fn solo_and_mute_pick_what_is_heard() {
        let (mut v, _l, s) = setup(&[0.1, 0.2, 0.4], 48_000);
        s.set_track(1, 1.0, false, true);
        run(&mut v, &s, 1000);
        assert!((run(&mut v, &s, 10)[0] - 0.2).abs() < 1e-6, "solo plays only track 1");
        s.set_track(1, 1.0, false, false);
        s.set_track(2, 1.0, true, false);
        run(&mut v, &s, 1000);
        assert!((run(&mut v, &s, 10)[0] - 0.3).abs() < 1e-6, "mute drops track 2");
    }

    #[test]
    fn level_changes_ramp_without_a_jump() {
        let (mut v, _l, s) = setup(&[0.5], 48_000);
        s.set_track(0, 1.0, true, false);
        let out = run(&mut v, &s, 1000);
        let max_step = out.chunks(2).collect::<Vec<_>>().windows(2).map(|w| (w[1][0] - w[0][0]).abs()).fold(0.0, f32::max);
        assert!(max_step < 0.01, "step {max_step}");
        assert_eq!(out[1998], 0.0, "muted after the ramp");
    }

    #[test]
    fn position_seek_and_end() {
        let (mut v, _l, s) = setup(&[0.1], 4_800);
        assert!((s.position_seconds() - 1000.0 / 48_000.0).abs() < 1e-9);
        s.seek_seconds(0.05);
        run(&mut v, &s, 100);
        assert!((s.position_seconds() - (2_400.0 + 100.0) / 48_000.0).abs() < 1e-9);
        run(&mut v, &s, 4_800);
        assert!(!s.is_playing(), "stops at the end");
        assert_eq!(s.position_seconds(), 0.0, "and goes back to the start");
    }

    #[test]
    fn pause_fades_out_and_holds_position() {
        let (mut v, _l, s) = setup(&[0.5], 48_000);
        s.set_playing(false);
        let out = run(&mut v, &s, 1000);
        let p = s.position_seconds();
        assert_eq!(out[1998], 0.0);
        run(&mut v, &s, 1000);
        assert_eq!(s.position_seconds(), p, "no movement while paused");
    }

    #[test]
    fn old_clips_go_back_to_the_control_side() {
        let (mut v, mut l, s) = setup(&[0.1], 4_800);
        l.send.push(clip(&[0.2], 4_800)).unwrap();
        run(&mut v, &s, 10);
        assert_eq!(l.returned.slots(), 1, "one clip came back");
        l.collect();
        assert!(l.returned.is_empty());
    }
}
