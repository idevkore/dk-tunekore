//! The Timeline's MIDI clips (idevkore/dk-tunekore#38): clips of notes on
//! instrument tracks, as the document keeps them, the UI sees them and the
//! project file saves them.
//!
//! Everything is in beats, so the notes follow the tempo. A clip has the
//! same shape as an audio clip with beats of its own notes in place of
//! seconds of a file: it starts at `start` on the Timeline and plays `length`
//! beats of its notes from `offset + phase`, looping over the `content`
//! beats from `offset` when `phase + length` runs past them. Notes are kept
//! in the clip's own time, from 0, so trimming or looping a clip moves no
//! note; a note outside the content is kept and shown, but doesn't play.
//!
//! The engine plays a track's clips with a [`tunekore_engine::clip::MidiPlayer`]
//! in the track's notes slot, ahead of its instrument (`timeline.rs` puts it
//! there and feeds it). Velocities are MIDI's 1 to 127 here and 0..1 there.

use serde::{Deserialize, Serialize};
use tunekore_engine::clip::{midi, MidiClipSet, MidiClipSpec, MidiNote};

/// The shortest note, clip or loop, in beats.
pub const MIN_BEATS: f64 = midi::MIN_BEATS;
/// The longest clip or note: a long day at 120 BPM.
pub const MAX_BEATS: f64 = 200_000.0;
/// The most notes a clip holds.
pub const MAX_NOTES: usize = 20_000;
/// A new note's velocity.
pub const DEFAULT_VELOCITY: u8 = 100;

fn finite(x: f64, default: f64) -> f64 {
    if x.is_finite() { x } else { default }
}

/// One note, in beats of its clip's own time. `id` names it within its clip,
/// for the note editor's selection; it isn't saved.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    #[serde(default)]
    pub id: u32,
    pub key: u8,
    pub start: f64,
    pub length: f64,
    /// 1..127.
    #[serde(default = "default_velocity")]
    pub velocity: u8,
}

fn default_velocity() -> u8 {
    DEFAULT_VELOCITY
}

/// A MIDI clip on an instrument track.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiClip {
    pub id: u64,
    pub name: String,
    pub start: f64,
    pub offset: f64,
    pub content: f64,
    pub phase: f64,
    pub length: f64,
    /// Sorted by start, then key.
    pub notes: Vec<Note>,
}

impl MidiClip {
    /// Where it ends on the Timeline, in beats.
    pub fn end(&self) -> f64 {
        self.start + self.length
    }

    pub fn spec(&self) -> MidiClipSpec {
        MidiClipSpec {
            start: self.start,
            offset: self.offset,
            content: self.content,
            phase: self.phase,
            length: self.length,
            notes: self.notes.iter().map(|n| MidiNote { key: n.key, start: n.start, length: n.length, velocity: f32::from(n.velocity) / 127.0 }).collect(),
        }
    }

    /// This clip with the times and notes of `s`, its notes numbered afresh.
    fn with_spec(&self, id: u64, s: &MidiClipSpec) -> Self {
        let notes = s.notes.iter().map(|n| Note { id: 0, key: n.key, start: n.start, length: n.length, velocity: (n.velocity * 127.0).round().clamp(1.0, 127.0) as u8 }).collect();
        Self { id, start: s.start, offset: s.offset, content: s.content, phase: s.phase, length: s.length, notes, name: self.name.clone() }.sane()
    }

    /// Clamped to sane values: times inside range, notes on MIDI's keys and
    /// velocities, at most [`MAX_NOTES`] of them, each with an id of its own.
    pub fn sane(mut self) -> Self {
        self.start = finite(self.start, 0.0).clamp(0.0, MAX_BEATS);
        self.length = finite(self.length, 4.0).clamp(MIN_BEATS, MAX_BEATS);
        self.offset = finite(self.offset, 0.0).clamp(0.0, MAX_BEATS);
        self.content = finite(self.content, self.length).clamp(MIN_BEATS, MAX_BEATS);
        self.phase = finite(self.phase, 0.0).max(0.0) % self.content;
        let name = self.name.trim();
        self.name = if name.is_empty() { "MIDI".into() } else { name.chars().take(64).collect() };
        self.notes.retain(|n| n.key < 128 && n.start.is_finite() && n.length.is_finite());
        for n in &mut self.notes {
            n.start = n.start.clamp(0.0, MAX_BEATS);
            n.length = n.length.clamp(MIN_BEATS, MAX_BEATS);
            n.velocity = n.velocity.clamp(1, 127);
        }
        // Over the limit, the earliest notes stay.
        self.notes.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.key.cmp(&b.key)).then(a.id.cmp(&b.id)));
        self.notes.truncate(MAX_NOTES);
        // Ids: kept where they are unique, the rest numbered on from the highest.
        let mut seen = std::collections::HashSet::new();
        let mut next = self.notes.iter().map(|n| n.id).max().unwrap_or(0);
        for n in &mut self.notes {
            if n.id == 0 || !seen.insert(n.id) {
                next += 1;
                n.id = next;
                seen.insert(n.id);
            }
        }
        self.notes.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.key.cmp(&b.key)).then(a.id.cmp(&b.id)));
        self
    }

    /// Cuts the clip at `cut` (a beat on the Timeline), as
    /// [`midi::split`] does; the second half gets `id`.
    pub fn split(&self, cut: f64, id: u64) -> Option<(Self, Self)> {
        let (a, b) = midi::split(&self.spec(), cut)?;
        Some((self.with_spec(self.id, &a), self.with_spec(id, &b)))
    }
}

/// A track's MIDI clips as its player wants them.
pub fn build(clips: &[MidiClip]) -> MidiClipSet {
    MidiClipSet::new(clips.iter().map(MidiClip::spec))
}

// ---- What the UI sees, and sends ----

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MidiClipView {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) start: f64,
    pub(super) offset: f64,
    pub(super) content: f64,
    pub(super) phase: f64,
    pub(super) length: f64,
    pub(super) notes: Vec<Note>,
}

pub fn view(c: &MidiClip) -> MidiClipView {
    MidiClipView { id: c.id, name: c.name.clone(), start: c.start, offset: c.offset, content: c.content, phase: c.phase, length: c.length, notes: c.notes.clone() }
}

/// A MIDI clip as the UI sends it: a new one (no `id`) or an existing one
/// changed. With no `notes`, an existing clip keeps its own and a new one is empty.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MidiClipIn {
    #[serde(default)]
    pub id: Option<u64>,
    /// An instrument track.
    pub track: u32,
    #[serde(default)]
    pub name: Option<String>,
    pub start: f64,
    #[serde(default)]
    pub offset: f64,
    pub content: f64,
    #[serde(default)]
    pub phase: f64,
    pub length: f64,
    #[serde(default)]
    pub notes: Option<Vec<Note>>,
}

impl MidiClipIn {
    /// The clip this makes, numbered `id`, from `old` (the clip it changes) where it says nothing.
    pub fn clip(&self, id: u64, old: Option<&MidiClip>, default_name: &str) -> MidiClip {
        let name = self.name.clone().or_else(|| old.map(|o| o.name.clone())).unwrap_or_else(|| default_name.to_string());
        let notes = match (&self.notes, old) {
            (Some(n), _) => n.clone(),
            (None, Some(o)) => o.notes.clone(),
            (None, None) => Vec::new(),
        };
        MidiClip { id, name, start: self.start, offset: self.offset, content: self.content, phase: self.phase, length: self.length, notes }.sane()
    }
}

// ---- The project file ----

/// A note as the project saves it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NoteFile {
    pub key: u8,
    pub start: f64,
    pub length: f64,
    #[serde(default = "default_velocity")]
    pub velocity: u8,
}

/// A MIDI clip as the project saves it, under its track's id in the `timeline` section's `midi`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MidiClipFile {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    pub start: f64,
    #[serde(default)]
    pub offset: f64,
    pub content: f64,
    #[serde(default)]
    pub phase: f64,
    pub length: f64,
    #[serde(default)]
    pub notes: Vec<NoteFile>,
}

pub fn to_file(c: &MidiClip) -> MidiClipFile {
    MidiClipFile {
        id: c.id,
        name: c.name.clone(),
        start: c.start,
        offset: c.offset,
        content: c.content,
        phase: c.phase,
        length: c.length,
        notes: c.notes.iter().map(|n| NoteFile { key: n.key, start: n.start, length: n.length, velocity: n.velocity }).collect(),
    }
}

pub fn from_file(f: MidiClipFile) -> MidiClip {
    let notes = f.notes.into_iter().map(|n| Note { id: 0, key: n.key, start: n.start, length: n.length, velocity: n.velocity }).collect();
    MidiClip { id: f.id, name: f.name, start: f.start, offset: f.offset, content: f.content, phase: f.phase, length: f.length, notes }.sane()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(key: u8, start: f64, length: f64) -> Note {
        Note { id: 0, key, start, length, velocity: 90 }
    }

    #[test]
    fn clips_and_notes_are_made_sane() {
        let c = MidiClip {
            id: 1,
            name: "  ".into(),
            start: -2.0,
            offset: f64::NAN,
            content: 0.0,
            phase: 7.0,
            length: f64::INFINITY,
            notes: vec![n(200, 0.0, 1.0), Note { id: 5, ..n(60, 2.0, 0.0) }, Note { id: 5, velocity: 0, ..n(62, 1.0, 1.0) }, n(64, f64::NAN, 1.0)],
        }
        .sane();
        assert_eq!((c.start, c.offset, c.content, c.length, c.name.as_str()), (0.0, 0.0, MIN_BEATS, 4.0, "MIDI"));
        assert!(c.phase < c.content);
        let notes: Vec<(u32, u8, f64, u8)> = c.notes.iter().map(|x| (x.id, x.key, x.length, x.velocity)).collect();
        assert_eq!(notes, [(5, 62, 1.0, 1), (6, 60, MIN_BEATS, 90)], "sorted by start; the later id 5 renumbered");
    }

    #[test]
    fn over_the_limit_the_earliest_notes_stay() {
        let notes = (0..MAX_NOTES + 10).rev().map(|i| n(60, i as f64, 0.5)).collect();
        let c = MidiClip { id: 1, name: "Keys".into(), start: 0.0, offset: 0.0, content: 4.0, phase: 0.0, length: 4.0, notes }.sane();
        assert_eq!((c.notes.len(), c.notes[0].start, c.notes[MAX_NOTES - 1].start), (MAX_NOTES, 0.0, (MAX_NOTES - 1) as f64));
    }

    #[test]
    fn a_split_cuts_the_notes_of_a_clip_that_doesnt_loop() {
        let c = MidiClip { id: 3, name: "Keys".into(), start: 4.0, offset: 0.0, content: 4.0, phase: 0.0, length: 4.0, notes: vec![n(60, 0.0, 1.0), n(62, 1.5, 1.0), n(64, 3.0, 1.0)] }.sane();
        let (a, b) = c.split(6.0, 9).unwrap();
        assert_eq!((a.id, a.length, b.id, b.start, b.offset, b.length), (3, 2.0, 9, 6.0, 2.0, 2.0));
        assert_eq!(a.notes.iter().map(|x| (x.key, x.start, x.length)).collect::<Vec<_>>(), [(60, 0.0, 1.0), (62, 1.5, 0.5)]);
        assert_eq!(b.notes.iter().map(|x| (x.key, x.start, x.length, x.velocity)).collect::<Vec<_>>(), [(62, 2.0, 0.5, 90), (64, 3.0, 1.0, 90)]);
        assert!(c.split(4.0, 9).is_none() && c.split(8.0, 9).is_none());
    }

    #[test]
    fn the_file_keeps_everything_but_the_note_ids() {
        let c = MidiClip { id: 3, name: "Keys".into(), start: 4.0, offset: 0.5, content: 2.0, phase: 0.25, length: 8.0, notes: vec![n(60, 0.5, 1.0), n(67, 1.0, 0.25)] }.sane();
        let f = to_file(&c);
        let text = serde_json::to_value(&f).unwrap();
        assert_eq!(text["notes"][1], serde_json::json!({ "key": 67, "start": 1.0, "length": 0.25, "velocity": 90 }));
        assert_eq!(from_file(f), c, "numbered the same way again");
        // A file written by hand needs no velocity, phase or offset.
        let hand: MidiClipFile = serde_json::from_value(serde_json::json!({ "id": 1, "start": 0.0, "content": 4.0, "length": 4.0, "notes": [{ "key": 60, "start": 0.0, "length": 1.0 }] })).unwrap();
        let c = from_file(hand);
        assert_eq!((c.notes[0].velocity, c.name.as_str()), (DEFAULT_VELOCITY, "MIDI"));
    }
}
