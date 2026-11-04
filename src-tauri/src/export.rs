//! Project export (idevkore/dk-tunekore#44, part 1): the whole project, every
//! track through its source and devices (plugins included), then the master
//! level and the limiter, rendered offline to WAV at the project's rate.
//!
//! What it renders: the whole song (from the start to the last clip on a
//! heard track, or the Sampler song's end, whichever is later; failing both,
//! one pass of the Sampler's pattern) or the Timeline's loop range, with a
//! tail of up to [`TAIL_SECONDS`] that ends once everything has gone quiet.
//! What it writes: the master, and a stem per heard track (after its volume,
//! pan, mute and solo, before the master level and the limiter), all the same
//! length and starting on the same frame, as 16-bit (dithered), 24-bit or
//! 32-bit float WAV.
//!
//! The engine does the rendering (`tunekore_engine::Render`, see the engine's
//! `graph::render`): it takes the graph off the open output while it runs,
//! and hands it back after. An export runs on a thread of its own inside
//! [`project::exclusive`], so no project opens, starts or saves under it, and
//! it tells the UI how far it has got (`export:progress`) and how it ended
//! (`export:done`, `export:cancelled`, `export:failed`). Each file is written
//! next to where it goes as a `.part` file; only once every file is finished
//! are they moved into place, so a cancel or a failure leaves nothing half
//! written and every file that was there before as it was.
//!
//! The menu item, File > Export Project… (⇧⌘R), asks the main window to open
//! the dialog (`export:open`).

mod wav;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::async_runtime::spawn_blocking;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tunekore_engine::{range_frames, Render, RenderPhase, RenderProgress, RenderSpec, SamplerShared, TrackId, TrackInfo, PADS, SAMPLER_TRACK};

use crate::project;
pub use wav::Format;

/// The longest tail after the song's end: room for a long reverb or a slow
/// delay to die away. The tail stops sooner once everything is quiet.
pub const TAIL_SECONDS: f64 = 10.0;
/// How often the dialog hears how far an export has got.
const PROGRESS_EVERY: Duration = Duration::from_millis(80);
/// The menu item's id and shortcut. ⇧⌘R is free everywhere: the Sampler has
/// ⌘E (its own export) and the Timeline ⌘E (split); nothing uses R.
const MENU_ID: &str = "export-project";
const SHORTCUT: &str = "CmdOrCtrl+Shift+R";

/// The export running now, by its cancel flag.
static JOB: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

fn job() -> std::sync::MutexGuard<'static, Option<Arc<AtomicBool>>> {
    JOB.lock().unwrap_or_else(|e| e.into_inner())
}

/// What to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Range {
    /// From the start to the song's end.
    Song,
    /// The Timeline's loop range, on or not.
    Loop,
}

/// What the dialog asks for.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub range: Range,
    pub format: Format,
    pub master: bool,
    pub stems: bool,
    /// Let reverbs and delays ring out after the end.
    pub tail: bool,
    /// The master's file with `master`; with stems only, the folder they go in.
    pub path: String,
}

/// A track as the dialog lists it.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TrackOut {
    id: TrackId,
    name: String,
    /// Not muted, and soloed when anything is: it gets a stem.
    heard: bool,
}

/// What the dialog shows before an export.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    project: String,
    /// The rate the files are at: the rate the tracks run at.
    rate: u32,
    tempo: f64,
    /// Beats per bar and the note value of a beat, as in 4/4.
    signature: [u32; 2],
    /// The song's end, in beats; `None` when nothing sets one.
    song_end: Option<f64>,
    song_seconds: f64,
    loop_start: f64,
    loop_end: f64,
    loop_on: bool,
    loop_seconds: f64,
    tail_seconds: f64,
    tracks: Vec<TrackOut>,
    /// An export is running.
    busy: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressEvent {
    /// `settling`, `playing` or `tail`.
    phase: &'static str,
    /// Seconds every file has, of the range's.
    seconds: f64,
    range: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoneEvent {
    files: Vec<String>,
    /// Seconds in each file.
    seconds: f64,
    /// How long the export took.
    took: f64,
    /// Files an integer format clipped (a stem above full scale).
    clipped: Vec<String>,
}

// ---- Where the files go ----

/// A file name with the characters macOS and Windows reject replaced.
fn safe(name: &str) -> String {
    let s: String = name.chars().map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '-' } else { c }).collect();
    s.trim().trim_start_matches('.').to_string()
}

/// Whether each track is heard: not muted, and soloed while any track is.
fn heard(tracks: &[TrackInfo]) -> Vec<bool> {
    let any_solo = tracks.iter().any(|t| t.mix.solo);
    tracks.iter().map(|t| !t.mix.mute && (!any_solo || t.mix.solo)).collect()
}

/// Every file an export writes, and the render that fills them.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    spec: RenderSpec,
    format: Format,
    master: Option<PathBuf>,
    /// One per stem in `spec.stems`.
    stems: Vec<PathBuf>,
    /// The folder the stems go in.
    folder: Option<PathBuf>,
}

/// The files for `req`, rendering `start..end` (beats) of a project named
/// `project` with `tracks`: the master at `req.path`, and the stems as
/// `<name> - NN <track>.wav` (NN the track's place) in a `<name> stems`
/// folder beside it, or in the folder `req.path` names when there is no master.
fn plan(req: &Request, tracks: &[TrackInfo], start: f64, end: f64, project: &str) -> Result<Plan, String> {
    if !req.master && !req.stems {
        return Err("Choose the master, the stems or both.".into());
    }
    let path = PathBuf::from(&req.path);
    if req.path.trim().is_empty() {
        return Err("Choose where the files go.".into());
    }
    let master = req.master.then(|| path.with_extension("wav"));
    let name = match &master {
        Some(m) => m.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        None => safe(project),
    };
    let name = if name.is_empty() { "Export".to_string() } else { name };
    let folder = req.stems.then(|| match &master {
        Some(m) => m.with_file_name(format!("{name} stems")),
        None => path.clone(),
    });
    let mut ids = Vec::new();
    let mut stems = Vec::new();
    if let Some(folder) = &folder {
        for ((i, t), heard) in tracks.iter().enumerate().zip(heard(tracks)) {
            if heard {
                let track = safe(&t.name);
                let track = if track.is_empty() { format!("Track {}", i + 1) } else { track };
                ids.push(t.id);
                stems.push(folder.join(format!("{name} - {:02} {track}.wav", i + 1)));
            }
        }
        if ids.is_empty() {
            return Err("Every track is muted, so there are no stems to export.".into());
        }
    }
    let tail_seconds = if req.tail { TAIL_SECONDS } else { 0.0 };
    let spec = RenderSpec { start, end, tail_seconds, stems: ids, block: tunekore_engine::graph::MAX_FRAMES };
    Ok(Plan { spec, format: req.format, master, stems, folder })
}

// ---- How long the song is ----

/// The end of the last clip on a heard track, in beats, from the Timeline's
/// view (`timeline_state`): a clip's `start` is in beats and its `length` in
/// seconds, as the Timeline plays clips at their files' own speed.
fn clips_end(view: &Value, tempo: f64, heard: impl Fn(u32) -> bool) -> Option<f64> {
    view.get("tracks")?
        .as_array()?
        .iter()
        .filter(|t| t.get("id").and_then(Value::as_u64).is_some_and(|id| heard(id as u32)))
        .flat_map(|t| t.get("clips").and_then(Value::as_array).into_iter().flatten())
        .filter_map(|c| Some(c.get("start")?.as_f64()? + c.get("length")?.as_f64()? * tempo / 60.0))
        .filter(|e| e.is_finite() && *e > 0.0)
        .reduce(f64::max)
}

/// The Sampler's song (its chain played through once, from beat 0) and one
/// pass of its pattern, in beats, when its track is heard.
fn sampler_lengths(pads: &SamplerShared, heard: bool) -> (Option<f64>, Option<f64>) {
    if !heard {
        return (None, None);
    }
    let pass = pads.steps() as f64 / 4.0;
    let chain = pads.chain().len();
    let song = (pads.song_mode() && chain > 0).then_some(chain as f64 * pass);
    let pattern = (0..PADS).any(|p| pads.pattern(p) != 0).then_some(pass);
    (song, pattern)
}

/// The song's end: the later of the last clip's and the Sampler song's;
/// failing both, one pass of the Sampler's pattern.
fn song_end(clips: Option<f64>, song: Option<f64>, pattern: Option<f64>) -> Option<f64> {
    match (clips, song) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b).or(pattern),
    }
}

/// The range `range` covers now, in beats.
fn range_of(app: &AppHandle, range: Range, tracks: &[TrackInfo]) -> Result<(f64, f64), String> {
    let transport = crate::audio(app).lock().unwrap_or_else(|e| e.into_inner()).engine.transport();
    match range {
        Range::Loop => {
            let l = transport.loop_range();
            Ok((l.start, l.end))
        }
        Range::Song => {
            let heard_ids: Vec<u32> = tracks.iter().zip(heard(tracks)).filter(|(_, h)| *h).map(|(t, _)| t.id.0).collect();
            let view = serde_json::to_value(crate::timeline::timeline_state(app.clone())).unwrap_or(Value::Null);
            let clips = clips_end(&view, transport.tempo(), |id| heard_ids.contains(&id));
            let pads = app.state::<tunekore_engine::SamplerHandle>();
            let (song, pattern) = sampler_lengths(&pads, heard_ids.contains(&SAMPLER_TRACK.0));
            song_end(clips, song, pattern)
                .map(|end| (0.0, end))
                .ok_or_else(|| "Nothing sets the song's end yet: put a clip on the Timeline or make a song in the Sampler, or export the loop range.".into())
        }
    }
}

fn info(app: &AppHandle) -> Info {
    let (tracks, transport, rate) = {
        let a = crate::audio(app).lock().unwrap_or_else(|e| e.into_inner());
        (a.engine.tracks(), a.engine.transport(), a.engine.render_rate())
    };
    let map = transport.tempo_map(rate);
    let seconds = |a: f64, b: f64| range_frames(&map, a, b) as f64 / f64::from(rate);
    let song_end = range_of(app, Range::Song, &tracks).ok().map(|(_, e)| e);
    let l = transport.loop_range();
    let heard = heard(&tracks);
    let sig = transport.time_signature();
    Info {
        project: project::name(app),
        rate,
        tempo: transport.tempo(),
        signature: [sig.numerator, sig.denominator],
        song_end,
        song_seconds: song_end.map_or(0.0, |e| seconds(0.0, e)),
        loop_start: l.start,
        loop_end: l.end,
        loop_on: transport.looping(),
        loop_seconds: seconds(l.start, l.end),
        tail_seconds: TAIL_SECONDS,
        tracks: tracks.iter().zip(heard).map(|(t, heard)| TrackOut { id: t.id, name: t.name.clone(), heard }).collect(),
        busy: job().is_some(),
    }
}

// ---- Writing ----

/// Why an export stopped short.
#[derive(Debug, PartialEq)]
enum Stop {
    /// The user cancelled.
    Cancelled,
    /// The audio output closed or changed, and took the graph back.
    Interrupted,
    Failed(String),
}

/// What an export wrote.
#[derive(Debug)]
struct Written {
    files: Vec<PathBuf>,
    clipped: Vec<PathBuf>,
    frames: u64,
}

/// Finished part files not yet in place: removed if the export stops first.
struct Parts(Vec<PathBuf>);

impl Drop for Parts {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// A folder the export made for the stems: removed again if it stops first
/// and the folder is still empty.
struct Folder(Option<PathBuf>);

impl Drop for Folder {
    fn drop(&mut self) {
        if let Some(f) = &self.0 {
            let _ = std::fs::remove_dir(f); // only succeeds when empty
        }
    }
}

fn failed(what: &Path, e: impl std::fmt::Display) -> Stop {
    Stop::Failed(format!("Couldn't write {}: {e}", what.display()))
}

/// Runs `render` to its end, writing `plan`'s files as it goes, then moves
/// them into place. `progress` hears after each block. Stopped short
/// (`cancel`, the output closing, an error), it removes every file it
/// started and leaves what was there before as it was.
fn write(render: &mut Render, plan: &Plan, cancel: &AtomicBool, mut progress: impl FnMut(RenderProgress)) -> Result<Written, Stop> {
    let mut folder = Folder(None);
    if let Some(f) = &plan.folder {
        if !f.is_dir() {
            std::fs::create_dir_all(f).map_err(|e| failed(f, e))?;
            folder.0 = Some(f.clone());
        }
    }
    let rate = render.rate();
    let create = |p: &PathBuf| wav::Writer::create(p, rate, plan.format).map_err(|e| failed(p, e));
    let mut master = plan.master.as_ref().map(create).transpose()?;
    let mut stems = plan.stems.iter().map(create).collect::<Result<Vec<_>, _>>()?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        if !render.next() {
            break;
        }
        if let Some(w) = &mut master {
            let m = render.master();
            w.write(m.left, m.right).map_err(|e| failed(w.path(), e))?;
        }
        for (i, w) in stems.iter_mut().enumerate() {
            let s = render.stem(i);
            w.write(s.left, s.right).map_err(|e| failed(w.path(), e))?;
        }
        progress(render.progress());
    }
    if render.cancelled() {
        return Err(Stop::Interrupted);
    }
    let frames = render.length().ok_or_else(|| Stop::Failed("the render stopped before its end".into()))?;
    let mut parts = Parts(Vec::new());
    let mut done = Vec::new();
    let mut clipped = Vec::new();
    for w in master.into_iter().chain(stems) {
        let path = w.path().to_path_buf();
        if w.clipped() > 0 {
            clipped.push(path.clone());
        }
        parts.0.push(w.finish(frames).map_err(|e| failed(&path, e))?);
        done.push(path);
    }
    // Every file is finished: now they go in place.
    for (part, path) in parts.0.iter().zip(&done) {
        std::fs::rename(part, path).map_err(|e| failed(path, e))?;
    }
    parts.0.clear();
    folder.0 = None;
    Ok(Written { files: done, clipped, frames })
}

// ---- The job ----

fn phase(p: RenderPhase) -> &'static str {
    match p {
        RenderPhase::Settling => "settling",
        RenderPhase::Playing => "playing",
        RenderPhase::Tail | RenderPhase::Done => "tail",
    }
}

/// The export thread: plans, takes the graph, writes, gives it back and says
/// how it went. `started` hears once the render has begun, or why it couldn't.
fn run(app: AppHandle, req: Request, cancel: Arc<AtomicBool>, started: mpsc::SyncSender<Result<(), String>>) {
    let began = Instant::now();
    let ended = project::exclusive(&app, || {
        let prepared = (|| {
            let tracks = crate::audio(&app).lock().unwrap_or_else(|e| e.into_inner()).engine.tracks();
            let (start, end) = range_of(&app, req.range, &tracks)?;
            let plan = plan(&req, &tracks, start, end, &project::name(&app))?;
            let render = crate::audio(&app).lock().unwrap_or_else(|e| e.into_inner()).engine.start_render(plan.spec.clone())?;
            Ok::<_, String>((plan, render))
        })();
        let (plan, mut render) = match prepared {
            Ok(p) => p,
            Err(e) => {
                let _ = started.send(Err(e));
                return None;
            }
        };
        let _ = started.send(Ok(()));
        let rate = f64::from(render.rate());
        let range = render.range() as f64 / rate;
        let mut last: Option<Instant> = None;
        let result = write(&mut render, &plan, &cancel, |p| {
            if last.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                last = Some(Instant::now());
                let _ = app.emit("export:progress", ProgressEvent { phase: phase(p.phase), seconds: p.frames.min(p.range) as f64 / rate, range });
            }
        });
        // The graph goes back to the output before anything else waits on the engine.
        drop(render);
        crate::audio(&app).lock().unwrap_or_else(|e| e.into_inner()).engine.finish_render();
        Some((result, rate))
    });
    *job() = None;
    let shown = |p: &[PathBuf]| p.iter().map(|f| f.display().to_string()).collect::<Vec<_>>();
    match ended {
        None => {}
        Some((Ok(w), rate)) => {
            let done = DoneEvent { files: shown(&w.files), seconds: w.frames as f64 / rate, took: began.elapsed().as_secs_f64(), clipped: shown(&w.clipped) };
            let _ = app.emit("export:done", done);
        }
        Some((Err(Stop::Cancelled), _)) => {
            let _ = app.emit("export:cancelled", "Export cancelled. Nothing was written.");
        }
        Some((Err(Stop::Interrupted), _)) => {
            let _ = app.emit("export:cancelled", "The export stopped because the audio output closed. Nothing was written.");
        }
        Some((Err(Stop::Failed(e)), _)) => {
            let _ = app.emit("export:failed", format!("{e} Nothing was written."));
        }
    }
}

// ---- Commands ----

/// What the dialog shows: the rate, the song's and the loop's lengths, the tracks.
#[tauri::command]
pub async fn export_info(app: AppHandle) -> Result<Info, String> {
    spawn_blocking(move || info(&app)).await.map_err(|e| e.to_string())
}

/// Starts an export. Returns once it has begun (the render has the graph),
/// or with why it couldn't; the events say how it goes from there.
#[tauri::command]
pub async fn export_start(request: Request, app: AppHandle) -> Result<(), String> {
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut j = job();
        if j.is_some() {
            return Err("An export is already running.".into());
        }
        *j = Some(cancel.clone());
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new().name("tunekore-export".into()).spawn(move || run(app, request, cancel, tx));
    if let Err(e) = spawned {
        *job() = None;
        return Err(e.to_string());
    }
    spawn_blocking(move || rx.recv().unwrap_or_else(|_| Err("the export stopped before it began".into()))).await.map_err(|e| e.to_string())?
}

/// Stops the running export; it removes what it wrote.
#[tauri::command]
pub fn export_cancel() {
    if let Some(c) = job().as_ref() {
        c.store(true, Ordering::Relaxed);
    }
}

/// The export commands.
pub fn commands() -> crate::Commands {
    crate::commands![export_info, export_start, export_cancel]
}

// ---- The menu ----

/// Adds File > Export Project… (⇧⌘R) to `menu`, at the top of File.
pub fn menu<R: Runtime>(app: &AppHandle<R>, menu: &Menu<R>) -> tauri::Result<()> {
    let item = MenuItem::with_id(app, MENU_ID, "Export Project…", true, Some(SHORTCUT))?;
    let sep = PredefinedMenuItem::separator(app)?;
    let file = menu.items()?.into_iter().filter_map(|i| i.as_submenu().cloned()).find(|s| s.text().map(|t| t == "File").unwrap_or(false));
    match file {
        Some(f) => f.insert_items(&[&item, &sep], 0)?,
        None => menu.insert(&Submenu::with_items(app, "File", true, &[&item])?, 1)?,
    }
    Ok(())
}

/// Opens the export dialog in the main window, from the menu.
pub fn on_menu<R: Runtime>(app: &AppHandle<R>, id: &str) {
    if id != MENU_ID {
        return;
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
    let _ = app.emit_to("main", "export:open", ());
}

#[cfg(test)]
mod tests;
