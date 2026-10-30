//! The Timeline (Phase 4, idevkore/dk-tunekore#36): tracks of audio clips on
//! the transport's beat grid, in the main window beside the Sampler.
//!
//! The tracks themselves are the engine's ([`tunekore_engine::Mixer`]); the
//! project's `tracks` section saves their names and mix. This module keeps
//! the rest: each track's colour and height, every clip, the view and the
//! loop, with an undo history, and saves them as the project's `timeline`
//! section, with clips keyed by track id and file paths relative to the
//! project like the Sampler's samples.
//!
//! Every track but the Sampler's (track 1) is an audio track, whose source is
//! a [`tunekore_engine::ClipPlayer`]. After each edit the track's clips go to
//! it as a new [`ClipSet`], their audio decoded with the Analyzer's decoder
//! and resampled to the output's rate here, off the audio thread. Decoded
//! files stay in memory (long files are not streamed from disk yet), with
//! their waveform peaks, which the UI fetches with `timeline_peaks` as bytes.
//!
//! A thread sends the playhead and the track meters to the UI about 30 times
//! a second while they change, and puts the clips back on their players when
//! the output reopens at another rate or loses its devices.
//!
//! Lock order: the document, then the feeds, then any of the sources, the
//! meters and the audio engine, which take no other lock while held.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::async_runtime::spawn_blocking;
use tauri::{AppHandle, Emitter, Manager};
use tunekore_analyzer::decode_file;
use tunekore_engine::transport::EDGE_FRAMES;
use tunekore_engine::{clip, resample, ClipFeed, ClipSet, ClipSpec, Peaks, Track, TrackId, TrackMeter, TrackMix, TransportShared, SAMPLER_TRACK};

use crate::project::{self, Apply, Section};
use crate::Audio;

/// Undo steps kept.
const UNDO_LIMIT: usize = 200;
/// Edits of the same kind this close together undo as one (a fader drag).
const MERGE_WINDOW: Duration = Duration::from_millis(700);
/// How often the playhead and meters go to the UI.
const TICK: Duration = Duration::from_millis(33);
/// Track colours: an index into the UI's palette.
const COLOURS: u8 = 12;
const MIN_HEIGHT: u32 = 28;
const MAX_HEIGHT: u32 = 320;
const DEFAULT_HEIGHT: u32 = 72;
const MIN_GAIN_DB: f32 = -60.0;
const MAX_GAIN_DB: f32 = 6.0;
/// The shortest clip and the shortest content, in seconds.
const MIN_LENGTH: f64 = 0.005;
/// The longest clip: a day.
const MAX_LENGTH: f64 = 86_400.0;
/// Splits are worked out at this rate while no output is open.
const FALLBACK_RATE: u32 = 48_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn finite(x: f64, default: f64) -> f64 {
    if x.is_finite() { x } else { default }
}

fn db_to_gain(db: f32) -> f32 {
    if db <= MIN_GAIN_DB { 0.0 } else { 10f32.powf(db.min(MAX_GAIN_DB) / 20.0) }
}

fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Audio".into())
}

// ---- The document ----

/// One clip. `start` is in beats; the rest is in seconds of its file (see
/// [`ClipSpec`]). The file is played from `offset + phase`, looping over
/// `content` when `phase + length` is longer.
#[derive(Debug, Clone, PartialEq)]
struct Clip {
    id: u64,
    path: PathBuf,
    start: f64,
    offset: f64,
    content: f64,
    phase: f64,
    length: f64,
    fade_in: f64,
    fade_out: f64,
    gain_db: f32,
}

impl Clip {
    fn spec(&self) -> ClipSpec {
        ClipSpec {
            start: self.start,
            offset: self.offset,
            content: self.content,
            phase: self.phase,
            length: self.length,
            fade_in: self.fade_in,
            fade_out: self.fade_out,
            gain: db_to_gain(self.gain_db),
        }
    }

    fn looped(&self) -> bool {
        self.phase + self.length > self.content + 1e-9
    }

    /// Where it ends, in beats, at `tempo`.
    fn end(&self, tempo: f64) -> f64 {
        self.start + self.length * tempo / 60.0
    }

    /// Clamped to sane values, and to its file's length when that is known.
    fn sane(mut self, seconds: Option<f64>) -> Self {
        self.start = finite(self.start, 0.0).max(0.0);
        self.length = finite(self.length, 1.0).clamp(MIN_LENGTH, MAX_LENGTH);
        self.offset = finite(self.offset, 0.0).max(0.0);
        self.content = finite(self.content, self.length).max(MIN_LENGTH);
        if let Some(d) = seconds {
            let d = d.max(MIN_LENGTH);
            self.offset = self.offset.min(d - MIN_LENGTH).max(0.0);
            self.content = self.content.min(d - self.offset).max(MIN_LENGTH.min(d - self.offset));
        }
        self.phase = finite(self.phase, 0.0).max(0.0) % self.content;
        self.fade_in = finite(self.fade_in, 0.0).clamp(0.0, self.length);
        self.fade_out = finite(self.fade_out, 0.0).clamp(0.0, self.length - self.fade_in);
        self.gain_db = if self.gain_db.is_finite() { self.gain_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB) } else { 0.0 };
        self
    }
}

/// What the Timeline keeps about a track besides its name and mix.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackMeta {
    id: TrackId,
    colour: u8,
    height: u32,
}

/// What the grid snaps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Snap {
    Bar,
    #[default]
    Beat,
    Eighth,
    Sixteenth,
    Off,
}

/// The view and the loop, saved with the project.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ViewState {
    /// Pixels per beat.
    zoom: f64,
    /// The beat at the left edge.
    scroll: f64,
    snap: Snap,
    /// Whether the Library shows beside the tracks.
    library: bool,
    loop_on: bool,
    loop_start: f64,
    loop_end: f64,
}

impl Default for ViewState {
    fn default() -> Self {
        Self { zoom: 24.0, scroll: 0.0, snap: Snap::Beat, library: true, loop_on: false, loop_start: 0.0, loop_end: 16.0 }
    }
}

impl ViewState {
    fn sane(mut self) -> Self {
        self.zoom = finite(self.zoom, 24.0).clamp(1.0, 800.0);
        self.scroll = finite(self.scroll, 0.0).max(0.0);
        self.loop_start = finite(self.loop_start, 0.0).max(0.0);
        self.loop_end = finite(self.loop_end, 16.0);
        if self.loop_end <= self.loop_start {
            self.loop_end = self.loop_start + 4.0;
        }
        self
    }
}

/// What undo puts back: the clips, the tracks and their names and mix.
#[derive(Clone)]
struct Snapshot {
    tracks: Vec<TrackMeta>,
    clips: BTreeMap<TrackId, Vec<Clip>>,
    mixes: Vec<(TrackId, String, TrackMix)>,
}

#[derive(Default)]
struct History {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last: Option<(String, Instant)>,
}

struct Doc {
    /// In the order the Timeline shows them; the Sampler's first.
    tracks: Vec<TrackMeta>,
    clips: BTreeMap<TrackId, Vec<Clip>>,
    next_clip: u64,
    view: ViewState,
    history: History,
    /// Bumped on every change the project saves.
    revision: u64,
}

impl Default for Doc {
    fn default() -> Self {
        Self {
            tracks: vec![TrackMeta { id: SAMPLER_TRACK, colour: 0, height: DEFAULT_HEIGHT }],
            clips: BTreeMap::new(),
            next_clip: 1,
            view: ViewState::default(),
            history: History::default(),
            revision: 0,
        }
    }
}

impl Doc {
    /// Records `before` as an undo step for an edit of kind `key`, unless it continues a run of the same kind.
    fn checkpoint(&mut self, key: &str, before: Snapshot) {
        let now = Instant::now();
        let merge = matches!(&self.history.last, Some((k, t)) if k == key && now.duration_since(*t) < MERGE_WINDOW);
        self.history.last = Some((key.to_string(), now));
        self.revision += 1;
        if merge {
            return;
        }
        self.history.undo.push(before);
        if self.history.undo.len() > UNDO_LIMIT {
            self.history.undo.remove(0);
        }
        self.history.redo.clear();
    }

    fn find(&self, id: u64) -> Option<(TrackId, &Clip)> {
        self.clips.iter().find_map(|(t, list)| list.iter().find(|c| c.id == id).map(|c| (*t, c)))
    }

    fn is_audio_track(&self, id: TrackId) -> bool {
        id != SAMPLER_TRACK && self.tracks.iter().any(|t| t.id == id)
    }

    fn next_colour(&self) -> u8 {
        (self.tracks.len() as u8) % COLOURS
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_clip;
        self.next_clip += 1;
        id
    }
}

// ---- Decoded files ----

/// A decoded file, its peaks, and its audio at each output rate asked for.
struct Source {
    rate: u32,
    data: Track,
    peaks: Peaks,
    at: Mutex<HashMap<u32, Track>>,
}

impl Source {
    fn frames(&self) -> usize {
        self.data.first().map_or(0, Vec::len)
    }

    fn seconds(&self) -> f64 {
        self.frames() as f64 / f64::from(self.rate.max(1))
    }

    /// The audio at `rate`, resampled once and kept. Slow the first time.
    fn at_rate(&self, rate: u32) -> Result<Track, String> {
        if rate == self.rate {
            return Ok(self.data.clone());
        }
        if let Some(t) = lock(&self.at).get(&rate) {
            return Ok(t.clone());
        }
        let t = Arc::new(resample(&self.data, self.rate, rate)?);
        lock(&self.at).insert(rate, t.clone());
        Ok(t)
    }
}

/// A track's clip player, as the main thread holds it.
struct Fed {
    feed: ClipFeed,
    /// The rate of the last set published to it; 0 before the first.
    rate: u32,
}

/// Which tracks [`TimelineState::sync`] sends their clips to again.
enum Resend<'a> {
    All,
    /// Those whose clips differ from these.
    Changed(&'a BTreeMap<TrackId, Vec<Clip>>),
    /// Only those whose player was replaced or changed rate.
    Stale,
}

// ---- What the UI sees ----

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClipView {
    id: u64,
    path: String,
    name: String,
    start: f64,
    offset: f64,
    content: f64,
    phase: f64,
    length: f64,
    fade_in: f64,
    fade_out: f64,
    gain_db: f32,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TrackView {
    id: TrackId,
    name: String,
    /// "sampler" or "audio".
    kind: &'static str,
    colour: u8,
    height: u32,
    mix: TrackMix,
    clips: Vec<ClipView>,
}

/// A file the clips play: its length, for drawing, and whether it is missing.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceView {
    path: String,
    seconds: f64,
    channels: usize,
    /// Frames of the file, for the peaks' buckets.
    frames: usize,
    rate: u32,
    missing: bool,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct View {
    tracks: Vec<TrackView>,
    sources: Vec<SourceView>,
    tempo: f64,
    signature: [u32; 2],
    zoom: f64,
    scroll: f64,
    snap: Snap,
    library: bool,
    loop_on: bool,
    loop_start: f64,
    loop_end: f64,
    can_undo: bool,
    can_redo: bool,
}

// ---- Edits from the UI ----

/// A clip as the UI sends it: a new one (no `id`) or an existing one changed.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ClipIn {
    #[serde(default)]
    id: Option<u64>,
    /// 0 puts it on a new track.
    track: u32,
    path: String,
    start: f64,
    offset: f64,
    content: f64,
    #[serde(default)]
    phase: f64,
    length: f64,
    #[serde(default)]
    fade_in: f64,
    #[serde(default)]
    fade_out: f64,
    #[serde(default)]
    gain_db: f32,
}

/// One undoable edit: clips to add or change, and clips to delete.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Edit {
    /// The kind of edit, so a run of the same kind undoes as one step.
    key: String,
    clips: Vec<ClipIn>,
    delete: Vec<u64>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TrackChange {
    name: Option<String>,
    colour: Option<u8>,
    height: Option<u32>,
    mix: Option<TrackMix>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ViewChange {
    zoom: Option<f64>,
    scroll: Option<f64>,
    snap: Option<Snap>,
    library: Option<bool>,
}

// ---- The state ----

/// The Timeline's document, decoded files and clip players.
pub struct TimelineState {
    doc: Mutex<Doc>,
    sources: Mutex<HashMap<PathBuf, Arc<Source>>>,
    feeds: Mutex<HashMap<TrackId, Fed>>,
    meters: Mutex<Vec<(TrackId, TrackMeter)>>,
    /// The document's revision, readable without its lock.
    revision: AtomicU64,
    audio: &'static Mutex<Audio>,
    transport: Arc<TransportShared>,
}

impl TimelineState {
    pub fn new(audio: &'static Mutex<Audio>) -> Self {
        let transport = lock(audio).engine.transport();
        Self {
            doc: Mutex::default(),
            sources: Mutex::default(),
            feeds: Mutex::default(),
            meters: Mutex::default(),
            revision: AtomicU64::new(0),
            audio,
            transport,
        }
    }

    fn note_revision(&self, doc: &Doc) {
        self.revision.store(doc.revision, Ordering::Relaxed);
    }

    /// Decodes `path`, or finds it decoded already. Slow the first time: don't hold the document.
    fn decode(&self, path: &Path) -> Result<Arc<Source>, String> {
        if let Some(s) = lock(&self.sources).get(path) {
            return Ok(s.clone());
        }
        let song = decode_file(path)?;
        if song.frames() < 2 {
            return Err(format!("{} has no audio", file_stem(path)));
        }
        let peaks = Peaks::build(&song.channels);
        let s = Arc::new(Source { rate: song.sample_rate, data: song.channels, peaks, at: Mutex::default() });
        Ok(lock(&self.sources).entry(path.to_path_buf()).or_insert(s).clone())
    }

    fn source(&self, path: &Path) -> Option<Arc<Source>> {
        lock(&self.sources).get(path).cloned()
    }

    /// The rate the clip players run at, once an output has opened.
    fn live_rate(&self) -> Option<u32> {
        let r = lock(&self.feeds).values().map(|f| f.feed.rate()).find(|&r| r > 0);
        r.or_else(|| Some(lock(self.audio).engine.output_rate()).filter(|&r| r > 0))
    }

    /// The rate clips are worked out at: the players', or 48 kHz before an output opens.
    fn rate(&self) -> u32 {
        self.live_rate().unwrap_or(FALLBACK_RATE)
    }

    /// A track's clips as the player wants them, at `rate`.
    fn build(&self, clips: &[Clip], rate: u32) -> ClipSet {
        let list: Vec<(ClipSpec, Track)> = clips
            .iter()
            .filter_map(|c| {
                let s = self.source(&c.path)?;
                let audio = s.at_rate(rate).ok()?;
                Some((c.spec(), audio))
            })
            .collect();
        ClipSet::new(rate, list)
    }

    fn snapshot(&self, doc: &Doc) -> Snapshot {
        let tracks = lock(self.audio).engine.tracks();
        let mixes = doc
            .tracks
            .iter()
            .filter_map(|m| tracks.iter().find(|t| t.id == m.id).map(|t| (t.id, t.name.clone(), t.mix)))
            .collect();
        Snapshot { tracks: doc.tracks.clone(), clips: doc.clips.clone(), mixes }
    }

    /// Gives every audio track a clip player, and sends the tracks `what`
    /// names their clips at the player's rate.
    fn sync(&self, doc: &Doc, what: Resend<'_>) {
        let ids: Vec<TrackId> = doc.tracks.iter().map(|t| t.id).filter(|&id| id != SAMPLER_TRACK).collect();
        let mut feeds = lock(&self.feeds);
        feeds.retain(|id, _| ids.contains(id));
        let lost: Vec<TrackId> = ids.iter().copied().filter(|id| feeds.get(id).is_none_or(|f| !f.feed.is_attached())).collect();
        if !lost.is_empty() {
            let mut a = lock(self.audio);
            let mixer = a.engine.mixer();
            for id in lost {
                if mixer.track(id).is_none() {
                    // Gone from the engine (a project opening): nothing to feed.
                    feeds.remove(&id);
                    continue;
                }
                let (player, feed) = clip::player();
                // Not an edit: the project saves clips, not players.
                if mixer.restore_source(id, Box::new(player)).is_ok() {
                    feeds.insert(id, Fed { feed, rate: 0 });
                }
            }
        }
        for id in &ids {
            let Some(f) = feeds.get_mut(id) else { continue };
            f.feed.collect();
            let rate = f.feed.rate();
            let changed = match what {
                Resend::All => true,
                Resend::Changed(before) => before.get(id) != doc.clips.get(id),
                Resend::Stale => false,
            };
            if rate > 0 && (changed || f.rate != rate) {
                f.feed.publish(self.build(doc.clips.get(id).map_or(&[][..], Vec::as_slice), rate));
                f.rate = rate;
            }
        }
        drop(feeds);
        let mut meters = lock(&self.meters);
        if meters.iter().map(|(id, _)| *id).ne(doc.tracks.iter().map(|t| t.id)) {
            let a = lock(self.audio);
            *meters = doc.tracks.iter().filter_map(|t| a.engine.track_meter(t.id).map(|m| (t.id, m))).collect();
        }
    }

    /// Lists the engine's tracks the document doesn't know yet (made by the
    /// project's `tracks` section), as audio tracks.
    fn adopt(&self, doc: &mut Doc) {
        let tracks = lock(self.audio).engine.tracks();
        doc.tracks.retain(|m| tracks.iter().any(|t| t.id == m.id));
        for t in tracks {
            if doc.tracks.iter().all(|m| m.id != t.id) {
                let meta = TrackMeta { id: t.id, colour: doc.next_colour(), height: DEFAULT_HEIGHT };
                // The Sampler's track leads.
                if t.id == SAMPLER_TRACK {
                    doc.tracks.insert(0, meta);
                } else {
                    doc.tracks.push(meta);
                }
            }
        }
        doc.clips.retain(|id, _| doc.tracks.iter().any(|t| t.id == *id && t.id != SAMPLER_TRACK));
    }

    fn view(&self, doc: &Doc) -> View {
        let tracks = lock(self.audio).engine.tracks();
        let sources = lock(&self.sources);
        let mut paths: Vec<&PathBuf> = doc.clips.values().flatten().map(|c| &c.path).collect();
        paths.sort();
        paths.dedup();
        let sig = self.transport.time_signature();
        View {
            tracks: doc
                .tracks
                .iter()
                .filter_map(|m| {
                    let t = tracks.iter().find(|t| t.id == m.id)?;
                    Some(TrackView {
                        id: m.id,
                        name: t.name.clone(),
                        kind: if m.id == SAMPLER_TRACK { "sampler" } else { "audio" },
                        colour: m.colour,
                        height: m.height,
                        mix: t.mix,
                        clips: doc.clips.get(&m.id).into_iter().flatten().map(clip_view).collect(),
                    })
                })
                .collect(),
            sources: paths
                .into_iter()
                .map(|p| match sources.get(p) {
                    Some(s) => SourceView {
                        path: p.display().to_string(),
                        seconds: s.seconds(),
                        channels: s.data.len(),
                        frames: s.frames(),
                        rate: s.rate,
                        missing: false,
                    },
                    None => SourceView { path: p.display().to_string(), seconds: 0.0, channels: 0, frames: 0, rate: 0, missing: true },
                })
                .collect(),
            tempo: self.transport.tempo(),
            signature: [sig.numerator, sig.denominator],
            zoom: doc.view.zoom,
            scroll: doc.view.scroll,
            snap: doc.view.snap,
            library: doc.view.library,
            loop_on: doc.view.loop_on,
            loop_start: doc.view.loop_start,
            loop_end: doc.view.loop_end,
            can_undo: !doc.history.undo.is_empty(),
            can_redo: !doc.history.redo.is_empty(),
        }
    }

    /// The document as the UI sees it.
    pub fn current(&self) -> View {
        let doc = lock(&self.doc);
        self.view(&doc)
    }

    /// Runs `f` as an undoable edit of kind `key`, then sends the changed
    /// tracks their clips. `f` checks everything before it changes anything.
    fn edit(&self, key: &str, f: impl FnOnce(&Self, &mut Doc) -> Result<(), String>) -> Result<View, String> {
        let mut doc = lock(&self.doc);
        let before = self.snapshot(&doc);
        f(self, &mut doc)?;
        doc.checkpoint(key, before.clone());
        self.sync(&doc, Resend::Changed(&before.clips));
        self.note_revision(&doc);
        Ok(self.view(&doc))
    }

    /// Adds an empty audio track to the engine and the document.
    fn new_track(&self, doc: &mut Doc, name: &str) -> Result<TrackId, String> {
        let id = lock(self.audio).engine.mixer().add_track(name)?;
        let colour = doc.next_colour();
        doc.tracks.push(TrackMeta { id, colour, height: DEFAULT_HEIGHT });
        Ok(id)
    }

    /// A name for a new, empty audio track: "Audio 2", "Audio 3"...
    fn audio_name(&self, doc: &Doc) -> String {
        let names: Vec<String> = lock(self.audio).engine.tracks().into_iter().map(|t| t.name).collect();
        let n = (doc.tracks.len()..).find(|n| !names.contains(&format!("Audio {n}"))).unwrap_or(1);
        format!("Audio {n}")
    }

    /// Puts files on the Timeline from `start` (beats): one after another on
    /// `track`, or each on a new track of its own when `track` is `None` or
    /// the Sampler's. Files that can't be read are skipped and named in the error.
    pub fn add_files(&self, files: &[PathBuf], track: Option<TrackId>, start: f64) -> Result<View, String> {
        let mut read = Vec::new();
        let mut failed = Vec::new();
        let rate = self.live_rate();
        for f in files {
            match self.decode(f) {
                Ok(s) => {
                    // Ready for the player before the document is held.
                    if let Some(r) = rate {
                        let _ = s.at_rate(r);
                    }
                    read.push((f.clone(), s));
                }
                Err(e) => failed.push(format!("{} ({e})", file_stem(f))),
            }
        }
        let view = if read.is_empty() {
            None
        } else {
            Some(self.edit("add", |s, doc| {
                let tempo = s.transport.tempo();
                let on = track.filter(|&t| doc.is_audio_track(t));
                let mut at = finite(start, 0.0).max(0.0);
                for (path, src) in &read {
                    let track = match on {
                        Some(t) => t,
                        None => s.new_track(doc, &file_stem(path))?,
                    };
                    let seconds = src.seconds();
                    let id = doc.take_id();
                    let clip = Clip { id, path: path.clone(), start: at, offset: 0.0, content: seconds, phase: 0.0, length: seconds, fade_in: 0.0, fade_out: 0.0, gain_db: 0.0 };
                    let list = doc.clips.entry(track).or_default();
                    list.push(clip.sane(Some(seconds)));
                    list.sort_by(|a, b| a.start.total_cmp(&b.start));
                    if on.is_some() {
                        at += seconds * tempo / 60.0;
                    }
                }
                Ok(())
            })?)
        };
        if !failed.is_empty() {
            return Err(format!("{} couldn't be read: {}", if failed.len() == 1 { "A file" } else { "Some files" }, failed.join(", ")));
        }
        view.ok_or_else(|| "nothing to add".into())
    }

    /// Adds, changes and deletes clips as one undoable step.
    pub fn apply_edit(&self, edit: Edit) -> Result<View, String> {
        // Files first, before the document is held: a copy of a clip names a file already decoded.
        let mut seconds: HashMap<PathBuf, Option<f64>> = HashMap::new();
        for c in &edit.clips {
            let p = PathBuf::from(&c.path);
            if !seconds.contains_key(&p) {
                let s = self.decode(&p).ok().map(|s| s.seconds());
                seconds.insert(p, s);
            }
        }
        let key = if edit.key.is_empty() { "edit".to_string() } else { edit.key.clone() };
        self.edit(&key, move |s, doc| {
            for c in &edit.clips {
                if c.track != 0 && !doc.is_audio_track(TrackId(c.track)) {
                    return Err(if c.track == SAMPLER_TRACK.0 { "the Sampler's track holds the Sampler; put clips on an audio track".into() } else { format!("there is no track {}", c.track) });
                }
                if let Some(id) = c.id {
                    if doc.find(id).is_none() {
                        return Err(format!("there is no clip {id}"));
                    }
                }
                let p = PathBuf::from(&c.path);
                // A file that can't be read can still be moved or trimmed, as long as a clip already plays it.
                if seconds.get(&p).copied().flatten().is_none() && !doc.clips.values().flatten().any(|x| x.path == p) {
                    return Err(format!("{} can't be read", file_stem(&p)));
                }
            }
            let new_track = match edit.clips.iter().find(|c| c.track == 0) {
                Some(c) => Some(s.new_track(doc, &file_stem(Path::new(&c.path)))?),
                None => None,
            };
            let gone: Vec<u64> = edit.delete.iter().copied().chain(edit.clips.iter().filter_map(|c| c.id)).collect();
            for list in doc.clips.values_mut() {
                list.retain(|c| !gone.contains(&c.id));
            }
            for c in &edit.clips {
                let track = if c.track == 0 { new_track.unwrap_or(TrackId(0)) } else { TrackId(c.track) };
                let id = match c.id {
                    Some(id) => id,
                    None => doc.take_id(),
                };
                let path = PathBuf::from(&c.path);
                let clip = Clip {
                    id,
                    start: c.start,
                    offset: c.offset,
                    content: c.content,
                    phase: c.phase,
                    length: c.length,
                    fade_in: c.fade_in,
                    fade_out: c.fade_out,
                    gain_db: c.gain_db,
                    path: path.clone(),
                }
                .sane(seconds.get(&path).copied().flatten());
                doc.clips.entry(track).or_default().push(clip);
            }
            for list in doc.clips.values_mut() {
                list.sort_by(|a, b| a.start.total_cmp(&b.start));
            }
            doc.clips.retain(|_, l| !l.is_empty());
            Ok(())
        })
    }

    /// Cuts clips at `beat`: the ones listed, or every clip there when none
    /// is. The two halves meet on the same frame at the output's rate, so
    /// they play through without a seam.
    pub fn split(&self, ids: &[u64], beat: f64) -> Result<View, String> {
        let rate = f64::from(self.rate());
        let tempo = self.transport.tempo();
        let fpb = rate * 60.0 / tempo;
        let frame = |b: f64| (b * fpb - EDGE_FRAMES).ceil();
        self.edit("split", |_, doc| {
            let mut cuts = Vec::new();
            for (track, list) in &doc.clips {
                for c in list {
                    if (ids.is_empty() || ids.contains(&c.id)) && c.start < beat && beat < c.end(tempo) {
                        cuts.push((*track, c.id));
                    }
                }
            }
            if cuts.is_empty() {
                return Err("there is no clip to split at the playhead".into());
            }
            let mut done = 0;
            for (track, id) in cuts {
                let new_id = doc.next_clip;
                let list = doc.clips.get_mut(&track).expect("listed above");
                let Some(at) = list.iter().position(|c| c.id == id) else { continue };
                let c = list[at].clone();
                let r = |s: f64| (s * rate).round();
                let (o, p, k, l) = (r(c.offset), r(c.phase), r(c.content).max(1.0), r(c.length));
                let n = frame(beat) - frame(c.start);
                if n < 1.0 || n >= l {
                    continue;
                }
                let mut left = Clip { offset: o / rate, phase: p / rate, content: k / rate, length: n / rate, fade_out: 0.0, ..c.clone() };
                let mut right = Clip { id: new_id, start: beat, offset: o / rate, content: k / rate, length: (l - n) / rate, fade_in: 0.0, ..c.clone() };
                if c.looped() {
                    right.phase = ((p + n) % k) / rate;
                } else {
                    left.content = (p + n) / rate;
                    right.offset = (o + p + n) / rate;
                    right.content = (k - p - n).max(1.0) / rate;
                    right.phase = 0.0;
                }
                left.fade_in = left.fade_in.min(left.length);
                right.fade_out = right.fade_out.min(right.length);
                list[at] = left;
                list.insert(at + 1, right);
                doc.next_clip += 1;
                done += 1;
            }
            if done == 0 {
                return Err("there is no clip to split at the playhead".into());
            }
            Ok(())
        })
    }

    pub fn add_track(&self) -> Result<View, String> {
        self.edit("track:add", |s, doc| {
            let name = s.audio_name(doc);
            s.new_track(doc, &name).map(|_| ())
        })
    }

    pub fn remove_track(&self, id: TrackId) -> Result<View, String> {
        self.edit("track:remove", |s, doc| {
            if !doc.is_audio_track(id) {
                return Err(if id == SAMPLER_TRACK { "the Sampler's track stays".into() } else { format!("there is no track {}", id.0) });
            }
            lock(s.audio).engine.mixer().remove_track(id)?;
            doc.tracks.retain(|t| t.id != id);
            doc.clips.remove(&id);
            Ok(())
        })
    }

    /// Renames, recolours, resizes or remixes a track. Height is the view's,
    /// not an edit; the rest undoes, a fader drag as one step.
    pub fn set_track(&self, id: TrackId, change: TrackChange) -> Result<View, String> {
        let only_height = change.name.is_none() && change.colour.is_none() && change.mix.is_none();
        if only_height {
            let mut doc = lock(&self.doc);
            let t = doc.tracks.iter_mut().find(|t| t.id == id).ok_or(format!("there is no track {}", id.0))?;
            if let Some(h) = change.height {
                t.height = h.clamp(MIN_HEIGHT, MAX_HEIGHT);
            }
            return Ok(self.view(&doc));
        }
        let key = match (&change.mix, &change.name) {
            (Some(_), _) => format!("mix:{}", id.0),
            (_, Some(_)) => format!("name:{}", id.0),
            _ => format!("colour:{}", id.0),
        };
        self.edit(&key, |s, doc| {
            let t = doc.tracks.iter_mut().find(|t| t.id == id).ok_or(format!("there is no track {}", id.0))?;
            if let Some(c) = change.colour {
                t.colour = c % COLOURS;
            }
            if let Some(h) = change.height {
                t.height = h.clamp(MIN_HEIGHT, MAX_HEIGHT);
            }
            let mut a = lock(s.audio);
            let mixer = a.engine.mixer();
            if let Some(name) = &change.name {
                let name = name.trim();
                if !name.is_empty() {
                    mixer.rename(id, &name.chars().take(64).collect::<String>())?;
                }
            }
            if let Some(mix) = change.mix {
                mixer.set_mix(id, mix)?;
            }
            Ok(())
        })
    }

    /// Zoom, scroll, snap and the Library: the view's, saved but not an edit.
    pub fn set_view(&self, change: ViewChange) -> View {
        let mut doc = lock(&self.doc);
        let v = &mut doc.view;
        if let Some(z) = change.zoom {
            v.zoom = z;
        }
        if let Some(s) = change.scroll {
            v.scroll = s;
        }
        if let Some(s) = change.snap {
            v.snap = s;
        }
        if let Some(l) = change.library {
            v.library = l;
        }
        doc.view = doc.view.sane();
        self.view(&doc)
    }

    /// Sets the transport's loop. Saved with the project, not undone.
    pub fn set_loop(&self, on: bool, start: f64, end: f64) -> Result<View, String> {
        if !(start.is_finite() && end.is_finite() && start >= 0.0 && end > start) {
            return Err("the loop needs an end after its start".into());
        }
        let mut doc = lock(&self.doc);
        doc.view.loop_on = on;
        doc.view.loop_start = start;
        doc.view.loop_end = end;
        self.apply_loop(&doc.view);
        doc.revision += 1;
        self.note_revision(&doc);
        Ok(self.view(&doc))
    }

    fn apply_loop(&self, v: &ViewState) {
        self.transport.set_loop(v.loop_start, v.loop_end);
        self.transport.set_looping(v.loop_on);
    }

    /// Puts `snap` back: the tracks it had (made again with their ids, or
    /// removed), their names and mix, and the clips.
    fn restore(&self, doc: &mut Doc, snap: Snapshot) -> Result<(), String> {
        {
            let mut a = lock(self.audio);
            let mixer = a.engine.mixer();
            for t in &doc.tracks {
                if t.id != SAMPLER_TRACK && snap.tracks.iter().all(|s| s.id != t.id) {
                    mixer.remove_track(t.id)?;
                }
            }
            for (id, name, mix) in &snap.mixes {
                if mixer.track(*id).is_none() {
                    mixer.add_track_as(*id, name)?;
                }
                mixer.rename(*id, name)?;
                mixer.set_mix(*id, *mix)?;
            }
        }
        let before = std::mem::replace(&mut doc.clips, snap.clips);
        doc.tracks = snap.tracks;
        doc.revision += 1;
        doc.history.last = None;
        self.sync(doc, Resend::Changed(&before));
        Ok(())
    }

    pub fn undo(&self) -> Result<View, String> {
        let mut doc = lock(&self.doc);
        let Some(snap) = doc.history.undo.pop() else { return Ok(self.view(&doc)) };
        let now = self.snapshot(&doc);
        doc.history.redo.push(now);
        self.restore(&mut doc, snap)?;
        self.note_revision(&doc);
        Ok(self.view(&doc))
    }

    pub fn redo(&self) -> Result<View, String> {
        let mut doc = lock(&self.doc);
        let Some(snap) = doc.history.redo.pop() else { return Ok(self.view(&doc)) };
        let now = self.snapshot(&doc);
        doc.history.undo.push(now);
        self.restore(&mut doc, snap)?;
        self.note_revision(&doc);
        Ok(self.view(&doc))
    }

    /// The thread's work: frees what the players handed back, and puts the
    /// clips back on players that were lost or changed rate.
    fn maintain(&self) {
        let stale = lock(&self.feeds).values_mut().fold(false, |stale, f| {
            f.feed.collect();
            stale || !f.feed.is_attached() || f.feed.rate() != f.rate
        });
        if stale {
            let doc = lock(&self.doc);
            self.sync(&doc, Resend::Stale);
        }
    }

    fn peaks(&self, path: &Path, level: usize) -> Result<Vec<u8>, String> {
        let s = self.source(path).ok_or_else(|| format!("{} isn't loaded", file_stem(path)))?;
        Ok(s.peaks.bytes(level))
    }
}

fn clip_view(c: &Clip) -> ClipView {
    ClipView {
        id: c.id,
        path: c.path.display().to_string(),
        name: file_stem(&c.path),
        start: c.start,
        offset: c.offset,
        content: c.content,
        phase: c.phase,
        length: c.length,
        fade_in: c.fade_in,
        fade_out: c.fade_out,
        gain_db: c.gain_db,
    }
}

// ---- The project file ----

/// The `timeline` section.
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct TimelineFile {
    tracks: Vec<TrackMeta>,
    /// Each audio track's clips, by track id.
    clips: BTreeMap<String, Vec<ClipFile>>,
    view: ViewState,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ClipFile {
    id: u64,
    /// Relative to the project file when it sits under the project's folder.
    file: String,
    start: f64,
    offset: f64,
    content: f64,
    #[serde(default)]
    phase: f64,
    length: f64,
    #[serde(default)]
    fade_in: f64,
    #[serde(default)]
    fade_out: f64,
    #[serde(default)]
    gain_db: f32,
}

fn relative(path: &Path, base: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(rel) if !base.as_os_str().is_empty() => rel.to_string_lossy().replace('\\', "/"),
        _ => path.display().to_string(),
    }
}

/// What [`TimelineState::read_file`] read, decoded, ready to apply.
struct Read {
    tracks: Vec<TrackMeta>,
    clips: BTreeMap<TrackId, Vec<Clip>>,
    view: ViewState,
}

impl TimelineState {
    fn save_file(&self, file: &Path) -> Value {
        let doc = lock(&self.doc);
        let base = file.parent().unwrap_or(Path::new(""));
        let clips = doc
            .clips
            .iter()
            .filter(|(_, l)| !l.is_empty())
            .map(|(id, list)| {
                let list = list
                    .iter()
                    .map(|c| ClipFile {
                        id: c.id,
                        file: relative(&c.path, base),
                        start: c.start,
                        offset: c.offset,
                        content: c.content,
                        phase: c.phase,
                        length: c.length,
                        fade_in: c.fade_in,
                        fade_out: c.fade_out,
                        gain_db: c.gain_db,
                    })
                    .collect();
                (id.0.to_string(), list)
            })
            .collect();
        serde_json::to_value(TimelineFile { tracks: doc.tracks.clone(), clips, view: doc.view }).unwrap_or(Value::Null)
    }

    /// Reads and checks the section, and decodes its files (slow; no lock
    /// held). A file that can't be read leaves its clips in place, silent.
    fn read_file(&self, value: Option<&Value>, file: &Path) -> Result<Read, String> {
        let f: TimelineFile = match value {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the project's Timeline is damaged ({e})"))?,
            None => TimelineFile::default(),
        };
        let base = file.parent().unwrap_or(Path::new(""));
        let rate = self.live_rate();
        let mut clips: BTreeMap<TrackId, Vec<Clip>> = BTreeMap::new();
        let mut ids = std::collections::HashSet::new();
        for (key, list) in f.clips {
            let track = key.parse::<u32>().ok().filter(|&t| t != 0).map(TrackId).ok_or(format!("the project's Timeline has clips on a track called {key}"))?;
            for c in list {
                if !ids.insert(c.id) {
                    return Err(format!("the project's Timeline has two clips numbered {}", c.id));
                }
                let p = Path::new(&c.file);
                let path = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
                let seconds = match self.decode(&path) {
                    Ok(s) => {
                        if let Some(r) = rate {
                            let _ = s.at_rate(r);
                        }
                        Some(s.seconds())
                    }
                    Err(_) => None,
                };
                let clip = Clip {
                    id: c.id,
                    path,
                    start: c.start,
                    offset: c.offset,
                    content: c.content,
                    phase: c.phase,
                    length: c.length,
                    fade_in: c.fade_in,
                    fade_out: c.fade_out,
                    gain_db: c.gain_db,
                }
                .sane(seconds);
                clips.entry(track).or_default().push(clip);
            }
        }
        for list in clips.values_mut() {
            list.sort_by(|a, b| a.start.total_cmp(&b.start));
        }
        let tracks = f.tracks.into_iter().map(|t| TrackMeta { colour: t.colour % COLOURS, height: t.height.clamp(MIN_HEIGHT, MAX_HEIGHT), ..t }).collect();
        Ok(Read { tracks, clips, view: f.view.sane() })
    }

    /// Replaces the document with what was read. The tracks are already the
    /// file's (the `tracks` section applied first). Clips on a track the file
    /// doesn't have are left out and named in the error.
    fn apply_file(&self, read: Read) -> Result<(), String> {
        let mut doc = lock(&self.doc);
        let engine: Vec<TrackId> = lock(self.audio).engine.tracks().iter().map(|t| t.id).collect();
        let mut tracks: Vec<TrackMeta> = read.tracks.into_iter().filter(|t| engine.contains(&t.id)).collect();
        tracks.dedup_by_key(|t| t.id);
        let mut clips = read.clips;
        let orphans: Vec<u32> = clips.keys().filter(|id| **id == SAMPLER_TRACK || !engine.contains(id)).map(|id| id.0).collect();
        let lost: usize = orphans.iter().map(|id| clips.remove(&TrackId(*id)).map_or(0, |l| l.len())).sum();
        let next_clip = clips.values().flatten().map(|c| c.id).max().unwrap_or(0) + 1;
        let revision = doc.revision + 1;
        *doc = Doc { tracks, clips, next_clip, view: read.view, history: History::default(), revision };
        self.adopt(&mut doc);
        self.apply_loop(&doc.view);
        self.sync(&doc, Resend::All);
        self.note_revision(&doc);
        if lost > 0 {
            return Err(format!("{lost} clip(s) were on tracks the project doesn't have"));
        }
        Ok(())
    }

    /// Empties the Timeline for a new project.
    fn clear(&self) {
        let mut doc = lock(&self.doc);
        let revision = doc.revision + 1;
        *doc = Doc { revision, ..Doc::default() };
        self.adopt(&mut doc);
        self.apply_loop(&doc.view);
        self.sync(&doc, Resend::All);
        self.note_revision(&doc);
        lock(&self.sources).clear();
    }
}

/// The Timeline's section of the project file. `notify` tells the windows
/// after an open or a new project.
pub struct TimelineSection {
    state: Arc<TimelineState>,
    notify: Arc<dyn Fn(&View) + Send + Sync>,
}

impl<C: 'static> Section<C> for TimelineSection {
    fn key(&self) -> &'static str {
        "timeline"
    }

    fn save(&self, _: &C, file: &Path) -> Result<Value, String> {
        Ok(self.state.save_file(file))
    }

    fn read(&self, _: &C, value: Option<&Value>, file: &Path) -> Result<Apply<C>, String> {
        let read = self.state.read_file(value, file)?;
        let (state, notify) = (self.state.clone(), self.notify.clone());
        Ok(Box::new(move |_| {
            let r = state.apply_file(read);
            notify(&state.current());
            r
        }))
    }

    fn clear(&self, _: &C) -> Result<(), String> {
        self.state.clear();
        (self.notify)(&self.state.current());
        Ok(())
    }

    fn revision(&self, _: &C) -> u64 {
        self.state.revision.load(Ordering::Relaxed)
    }
}

// ---- In the app ----

/// The Timeline's state in the app.
pub struct Timeline(pub Arc<TimelineState>);

fn shared(app: &AppHandle) -> Arc<TimelineState> {
    app.state::<Timeline>().0.clone()
}

/// Sends the document to every window, and returns it.
fn publish(app: &AppHandle, v: View) -> View {
    let _ = app.emit("timeline:state", &v);
    v
}

#[tauri::command]
pub fn timeline_state(app: AppHandle) -> View {
    shared(&app).current()
}

/// Decodes `files` and puts them on the Timeline at `start` (beats): on
/// `track` one after another, or each on a new track when `track` is null.
#[tauri::command]
pub async fn timeline_add_files(files: Vec<String>, track: Option<u32>, start: f64, app: AppHandle) -> Result<View, String> {
    let s = shared(&app);
    let files: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
    let r = spawn_blocking(move || s.add_files(&files, track.map(TrackId), start)).await.map_err(|e| e.to_string())?;
    // Even with a file skipped, the windows show what was added.
    let v = publish(&app, shared(&app).current());
    r.map(|_| v)
}

#[tauri::command]
pub async fn timeline_edit(edit: Edit, app: AppHandle) -> Result<View, String> {
    let s = shared(&app);
    let v = spawn_blocking(move || s.apply_edit(edit)).await.map_err(|e| e.to_string())??;
    Ok(publish(&app, v))
}

/// Splits clips at `beat`: those in `ids`, or every clip there when it is empty.
#[tauri::command]
pub fn timeline_split(ids: Vec<u64>, beat: f64, app: AppHandle) -> Result<View, String> {
    let v = shared(&app).split(&ids, beat)?;
    Ok(publish(&app, v))
}

#[tauri::command]
pub fn timeline_undo(app: AppHandle) -> Result<View, String> {
    let v = shared(&app).undo()?;
    Ok(publish(&app, v))
}

#[tauri::command]
pub fn timeline_redo(app: AppHandle) -> Result<View, String> {
    let v = shared(&app).redo()?;
    Ok(publish(&app, v))
}

#[tauri::command]
pub fn timeline_track_add(app: AppHandle) -> Result<View, String> {
    let v = shared(&app).add_track()?;
    Ok(publish(&app, v))
}

#[tauri::command]
pub fn timeline_track_remove(id: u32, app: AppHandle) -> Result<View, String> {
    let v = shared(&app).remove_track(TrackId(id))?;
    Ok(publish(&app, v))
}

#[tauri::command]
pub fn timeline_track_set(id: u32, change: TrackChange, app: AppHandle) -> Result<View, String> {
    let v = shared(&app).set_track(TrackId(id), change)?;
    Ok(publish(&app, v))
}

/// Zoom, scroll, snap and whether the Library shows. Not sent to other windows.
#[tauri::command]
pub fn timeline_view(change: ViewChange, app: AppHandle) -> View {
    shared(&app).set_view(change)
}

#[tauri::command]
pub fn timeline_loop(on: bool, start: f64, end: f64, app: AppHandle) -> Result<View, String> {
    let v = shared(&app).set_loop(on, start, end)?;
    Ok(publish(&app, v))
}

/// Moves the playhead: where play starts, and a jump there while playing.
#[tauri::command]
pub fn timeline_seek(beat: f64, app: AppHandle) {
    shared(&app).transport.seek(beat);
}

#[tauri::command]
pub async fn timeline_play(playing: bool, app: AppHandle) -> Result<(), String> {
    let s = shared(&app);
    if playing {
        let audio = s.audio;
        spawn_blocking(move || lock(audio).ensure_open()).await.map_err(|e| e.to_string())??;
    }
    s.transport.set_playing(playing);
    Ok(())
}

/// A file's waveform peaks at `level` (see `tunekore_engine::PEAK_LEVELS`):
/// min then max of each bucket, as signed bytes.
#[tauri::command]
pub fn timeline_peaks(path: String, level: usize, app: AppHandle) -> Result<tauri::ipc::Response, String> {
    Ok(tauri::ipc::Response::new(shared(&app).peaks(Path::new(&path), level)?))
}

#[derive(Serialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Tick {
    playing: bool,
    /// The playhead: where it plays now, or where play starts while stopped.
    beat: f64,
    tempo: f64,
    signature: [u32; 2],
    /// Each track's peak since the last tick, `[id, peak]`.
    levels: Vec<(u32, f32)>,
}

/// Sends the playhead and meters to the UI about 30 times a second while
/// they change, and keeps the clip players fed.
fn spawn_tick(app: AppHandle, s: Arc<TimelineState>) {
    std::thread::spawn(move || {
        let mut last: Option<Tick> = None;
        loop {
            std::thread::sleep(TICK);
            s.maintain();
            let t = &s.transport;
            let playing = t.is_playing();
            let sig = t.time_signature();
            let levels: Vec<(u32, f32)> = lock(&s.meters).iter().map(|(id, m)| (id.0, m.take_peak())).collect();
            let now = Tick { playing, beat: if playing { t.position_beats() } else { t.start() }, tempo: t.tempo(), signature: [sig.numerator, sig.denominator], levels };
            if last.as_ref() != Some(&now) {
                let _ = app.emit("timeline:tick", &now);
            }
            last = Some(now);
        }
    });
}

/// The Timeline's commands.
pub fn commands() -> crate::Commands {
    crate::commands![
        timeline_state,
        timeline_add_files,
        timeline_edit,
        timeline_split,
        timeline_undo,
        timeline_redo,
        timeline_track_add,
        timeline_track_remove,
        timeline_track_set,
        timeline_view,
        timeline_loop,
        timeline_seek,
        timeline_play,
        timeline_peaks,
    ]
}

/// The Timeline's part of the app: its state, its section of the project
/// file, and the thread that sends the playhead.
pub fn setup(app: &AppHandle) {
    let state = Arc::new(TimelineState::new(crate::audio(app)));
    {
        let mut doc = lock(&state.doc);
        state.adopt(&mut doc);
        state.sync(&doc, Resend::All);
    }
    app.manage(Timeline(state.clone()));
    let a = app.clone();
    // After an open or a new project, the windows take the view's zoom and scroll too.
    let notify = move |v: &View| {
        let _ = a.emit("timeline:state", v);
        let _ = a.emit("timeline:opened", v);
    };
    project::register(app, TimelineSection { state: state.clone(), notify: Arc::new(notify) });
    spawn_tick(app.clone(), state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Project, TracksSection, TransportSection};
    use tunekore_analyzer::export_wav;

    struct Rig {
        state: Arc<TimelineState>,
        project: Project<()>,
        audio: &'static Mutex<Audio>,
        dir: tempfile::TempDir,
    }

    /// A project with the core sections and the Timeline's on a real engine
    /// (no output open), and a folder with a one-second and a half-second WAV.
    fn rig() -> Rig {
        let audio: &'static Mutex<Audio> = Box::leak(Box::new(Mutex::new(Audio::new(tunekore_engine::Engine::new()))));
        let state = Arc::new(TimelineState::new(audio));
        let project = Project::new();
        project.register(TimelineSection { state: state.clone(), notify: Arc::new(|_: &View| {}) });
        project.register_core(TransportSection(lock(audio).engine.transport()));
        project.register_core(TracksSection::new(audio));
        project.mark_saved(&());
        let dir = tempfile::tempdir().unwrap();
        export_wav(&[vec![0.25; 48_000]], 48_000, &dir.path().join("kick.wav")).unwrap();
        export_wav(&[vec![0.5; 22_050], vec![-0.5; 22_050]], 44_100, &dir.path().join("snare.wav")).unwrap();
        Rig { state, project, audio, dir }
    }

    impl Rig {
        fn file(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn engine_tracks(&self) -> Vec<u32> {
            lock(self.audio).engine.tracks().iter().map(|t| t.id.0).collect()
        }
    }

    #[test]
    fn files_dropped_below_the_tracks_get_a_track_each_and_on_a_track_play_in_a_row() {
        let rig = rig();
        let v = rig.state.add_files(&[rig.file("kick.wav"), rig.file("snare.wav")], None, 4.0).unwrap();
        assert_eq!(v.tracks.iter().map(|t| (t.name.as_str(), t.kind)).collect::<Vec<_>>(), [("Sampler", "sampler"), ("kick", "audio"), ("snare", "audio")]);
        assert_eq!(v.tracks[1].clips[0].start, 4.0);
        assert_eq!(v.tracks[2].clips[0].start, 4.0, "stems line up");
        assert!((v.tracks[2].clips[0].length - 0.5).abs() < 1e-9);
        assert_eq!(rig.engine_tracks(), [1, 2, 3]);
        // On one track: the second starts where the first ends (1 s at 120 BPM is 2 beats).
        let v = rig.state.add_files(&[rig.file("kick.wav"), rig.file("snare.wav")], Some(TrackId(2)), 8.0).unwrap();
        let starts: Vec<f64> = v.tracks[1].clips.iter().map(|c| c.start).collect();
        assert_eq!(starts, [4.0, 8.0, 10.0]);
        // The Sampler's track takes no clips: a new track.
        let v = rig.state.add_files(&[rig.file("kick.wav")], Some(SAMPLER_TRACK), 0.0).unwrap();
        assert_eq!(v.tracks.len(), 4);
        let err = rig.state.add_files(&[rig.file("nothing.wav")], None, 0.0).unwrap_err();
        assert!(err.contains("nothing"), "{err}");
    }

    #[test]
    fn the_timeline_round_trips_through_the_project_file_with_relative_paths() {
        let rig = rig();
        let outside = tempfile::tempdir().unwrap();
        let far = outside.path().join("far.wav");
        export_wav(&[vec![0.1; 4_800]], 48_000, &far).unwrap();
        rig.state.add_files(&[rig.file("kick.wav"), far.clone()], None, 2.0).unwrap();
        rig.state.set_loop(true, 4.0, 12.0).unwrap();
        rig.state.set_view(ViewChange { zoom: Some(60.0), snap: Some(Snap::Sixteenth), ..Default::default() });
        let file = rig.dir.path().join("song.tunek");
        rig.project.save(&(), Some(file.clone())).unwrap();
        let text: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let tl = &text["timeline"];
        assert_eq!(tl["clips"]["2"][0]["file"], "kick.wav", "relative inside the project's folder");
        assert_eq!(tl["clips"]["3"][0]["file"], far.display().to_string(), "absolute outside it");
        assert_eq!(tl["view"]["snap"], "sixteenth");
        assert_eq!(text["tracks"][1]["name"], "kick");

        let saved = rig.state.current();
        rig.project.new_project(&()).unwrap();
        let empty = rig.state.current();
        assert_eq!(empty.tracks.len(), 1, "only the Sampler");
        assert!(!empty.loop_on);
        assert_eq!(rig.engine_tracks(), [1]);

        rig.project.open(&(), &file, Some(file.clone()), false).unwrap();
        let back = rig.state.current();
        assert_eq!(back.tracks, saved.tracks);
        assert_eq!((back.loop_on, back.loop_start, back.loop_end, back.zoom, back.snap), (true, 4.0, 12.0, 60.0, Snap::Sixteenth));
        assert_eq!(rig.engine_tracks(), [1, 2, 3]);
        assert!(rig.state.transport.looping() && rig.state.transport.loop_range().end == 12.0, "the loop is the transport's again");
        assert!(!rig.project.is_dirty(&()));
        let again: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(rig.state.save_file(&file), again["timeline"], "what opened is what saves");
    }

    #[test]
    fn a_missing_file_keeps_its_clip_silent_and_saves_it_back() {
        let rig = rig();
        let file = rig.dir.path().join("gone.tunek");
        let text = serde_json::json!({
            "format": "tunekore-project", "version": 2,
            "tracks": [{ "id": 1, "name": "Sampler" }, { "id": 5, "name": "Vox" }],
            "timeline": { "tracks": [{ "id": 5, "colour": 3, "height": 90 }],
                "clips": { "5": [{ "id": 7, "file": "Samples/vox.wav", "start": 1.0, "offset": 0.5, "content": 2.0, "length": 2.0 }] } }
        });
        std::fs::write(&file, text.to_string()).unwrap();
        rig.project.open(&(), &file, Some(file.clone()), false).unwrap();
        let v = rig.state.current();
        let vox = v.tracks.iter().find(|t| t.id == TrackId(5)).unwrap();
        assert_eq!((vox.colour, vox.height, vox.clips.len()), (3, 90, 1));
        assert!(v.sources[0].missing);
        assert_eq!(rig.state.save_file(&file)["clips"]["5"][0]["file"], "Samples/vox.wav");
        // It can still be moved.
        let c = &vox.clips[0];
        rig.state
            .apply_edit(Edit { key: "move".into(), clips: vec![ClipIn { id: Some(7), track: 5, path: c.path.clone(), start: 3.0, offset: c.offset, content: c.content, phase: 0.0, length: c.length, fade_in: 0.0, fade_out: 0.0, gain_db: 0.0 }], delete: vec![] })
            .unwrap();
        assert_eq!(rig.state.current().tracks[1].clips[0].start, 3.0);
    }

    #[test]
    fn clips_on_a_track_the_file_lacks_open_the_project_in_part() {
        let rig = rig();
        let text = serde_json::json!({ "format": "tunekore-project", "version": 2, "timeline": { "clips": { "9": [{ "id": 1, "file": "kick.wav", "start": 0.0, "offset": 0.0, "content": 1.0, "length": 1.0 }] } } });
        let err = rig.project.open_text(&(), &text.to_string(), &rig.dir.path().join("a.tunek"), None, false).unwrap_err();
        assert!(err.contains("opened only in part") && err.contains("tracks the project doesn't have"), "{err}");
        let damaged = serde_json::json!({ "format": "tunekore-project", "version": 2, "timeline": { "clips": { "x": [] } } });
        assert!(rig.project.open_text(&(), &damaged.to_string(), Path::new("/a.tunek"), None, false).is_err());
    }

    #[test]
    fn edits_mark_the_project_unsaved_and_undo_and_redo_put_tracks_back() {
        let rig = rig();
        assert!(!rig.project.is_dirty(&()));
        rig.state.add_files(&[rig.file("kick.wav")], None, 0.0).unwrap();
        assert!(rig.project.is_dirty(&()));
        rig.project.save(&(), Some(rig.dir.path().join("a"))).unwrap();
        assert!(!rig.project.is_dirty(&()));
        // Zoom is the view's: not an edit.
        rig.state.set_view(ViewChange { zoom: Some(100.0), ..Default::default() });
        assert!(!rig.project.is_dirty(&()));

        let v = rig.state.undo().unwrap();
        assert_eq!(v.tracks.len(), 1, "the new track went with its clip");
        assert_eq!(rig.engine_tracks(), [1]);
        assert!(rig.project.is_dirty(&()));
        let v = rig.state.redo().unwrap();
        assert_eq!(v.tracks[1].id, TrackId(2), "back with its id");
        assert_eq!(v.tracks[1].clips.len(), 1);
        assert_eq!(rig.engine_tracks(), [1, 2]);

        // A fader drag is one undo step.
        for i in 0..5 {
            rig.state.set_track(TrackId(2), TrackChange { mix: Some(TrackMix { volume: 1.0 - i as f32 * 0.1, ..Default::default() }), ..Default::default() }).unwrap();
        }
        assert!((rig.state.current().tracks[1].mix.volume - 0.6).abs() < 1e-6);
        let v = rig.state.undo().unwrap();
        assert_eq!(v.tracks[1].mix.volume, 1.0);
        assert!(v.can_undo && v.can_redo);
    }

    #[test]
    fn moves_trims_copies_and_deletes_are_one_step_each_and_clamped_to_the_file() {
        let rig = rig();
        let v = rig.state.add_files(&[rig.file("kick.wav")], None, 0.0).unwrap();
        let c = v.tracks[1].clips[0].clone();
        let input = |id: Option<u64>, track: u32, start: f64, offset: f64, length: f64| ClipIn {
            id,
            track,
            path: c.path.clone(),
            start,
            offset,
            content: 1.0,
            phase: 0.0,
            length,
            fade_in: 0.0,
            fade_out: 0.0,
            gain_db: 0.0,
        };
        // Move it, and put a copy on a new track; the copy's offset is past the file's end.
        let v = rig
            .state
            .apply_edit(Edit { key: "move".into(), clips: vec![input(Some(c.id), 2, 2.0, 0.0, 1.0), input(None, 0, 2.0, 5.0, 0.5)], delete: vec![] })
            .unwrap();
        assert_eq!(v.tracks[1].clips[0].start, 2.0);
        let copy = &v.tracks[2].clips[0];
        assert!(copy.offset < 1.0 && copy.offset + copy.content <= 1.0 + 1e-9, "inside the file: {copy:?}");
        assert_ne!(copy.id, c.id);
        // A looped clip: longer than its content.
        let v = rig.state.apply_edit(Edit { key: "loop".into(), clips: vec![input(Some(c.id), 2, 2.0, 0.0, 3.5)], delete: vec![] }).unwrap();
        assert_eq!(v.tracks[1].clips[0].length, 3.5);
        let v = rig.state.apply_edit(Edit { key: "delete".into(), clips: vec![], delete: vec![copy.id] }).unwrap();
        assert!(v.tracks[2].clips.is_empty());
        assert!(rig.state.apply_edit(Edit { clips: vec![input(None, 1, 0.0, 0.0, 1.0)], ..Default::default() }).unwrap_err().contains("Sampler"));
        assert!(rig.state.apply_edit(Edit { clips: vec![input(Some(999), 2, 0.0, 0.0, 1.0)], ..Default::default() }).is_err());
        for _ in 0..3 {
            rig.state.undo().unwrap();
        }
        let v = rig.state.current();
        assert_eq!((v.tracks.len(), v.tracks[1].clips[0].start), (2, 0.0), "back to the drop");
    }

    #[test]
    fn a_split_cuts_on_a_frame_and_the_halves_carry_on_in_the_file() {
        let rig = rig();
        let v = rig.state.add_files(&[rig.file("kick.wav")], None, 1.0).unwrap();
        let id = v.tracks[1].clips[0].id;
        // 1.3 beats at 120 BPM and 48 kHz: frames 24,000 to 31,200, 7,200 frames in.
        let v = rig.state.split(&[], 1.3).unwrap();
        let (a, b) = (&v.tracks[1].clips[0], &v.tracks[1].clips[1]);
        assert_eq!(a.id, id);
        assert!(((a.length * 48_000.0) - 7_200.0).abs() < 1e-6, "{}", a.length * 48_000.0);
        assert_eq!(b.start, 1.3);
        assert!((b.offset * 48_000.0 - 7_200.0).abs() < 1e-6 && b.phase == 0.0);
        assert!(((a.length + b.length) - 1.0).abs() < 1e-9);
        assert!(rig.state.split(&[], 50.0).is_err(), "nothing there");
        rig.state.undo().unwrap();
        assert_eq!(rig.state.current().tracks[1].clips.len(), 1);
    }

    #[test]
    fn the_sampler_track_and_removing_tracks() {
        let rig = rig();
        assert!(rig.state.remove_track(SAMPLER_TRACK).is_err());
        rig.state.add_track().unwrap();
        let v = rig.state.add_track().unwrap();
        assert_eq!(v.tracks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["Sampler", "Audio 1", "Audio 2"]);
        let v = rig.state.remove_track(TrackId(2)).unwrap();
        assert_eq!(v.tracks.len(), 2);
        assert_eq!(rig.engine_tracks(), [1, 3]);
        let v = rig.state.set_track(SAMPLER_TRACK, TrackChange { mix: Some(TrackMix { mute: true, ..Default::default() }), ..Default::default() }).unwrap();
        assert!(v.tracks[0].mix.mute);
        let v = rig.state.set_track(TrackId(3), TrackChange { height: Some(5_000), ..Default::default() }).unwrap();
        assert_eq!(v.tracks[1].height, MAX_HEIGHT);
    }

    #[test]
    fn peaks_come_as_bytes_for_each_level() {
        let rig = rig();
        rig.state.add_files(&[rig.file("snare.wav")], None, 0.0).unwrap();
        let b = rig.state.peaks(&rig.file("snare.wav"), 0).unwrap();
        assert_eq!(b.len(), 22_050usize.div_ceil(16) * 2);
        assert_eq!((b[0] as i8, b[1] as i8), (-64, 64), "both channels: -0.5 and 0.5");
        assert!(rig.state.peaks(&rig.file("kick.wav"), 0).is_err(), "not loaded");
    }

    #[test]
    fn clip_values_are_made_sane() {
        let c = Clip { id: 1, path: "a.wav".into(), start: -3.0, offset: 9.0, content: f64::NAN, phase: 7.0, length: 0.0, fade_in: 9.0, fade_out: 9.0, gain_db: 40.0 }.sane(Some(2.0));
        assert_eq!(c.start, 0.0);
        assert!(c.offset < 2.0 && c.offset + c.content <= 2.0 + 1e-12);
        assert!(c.phase < c.content);
        assert_eq!(c.length, MIN_LENGTH);
        assert!(c.fade_in + c.fade_out <= c.length + 1e-12);
        assert_eq!(c.gain_db, MAX_GAIN_DB);
    }
}
