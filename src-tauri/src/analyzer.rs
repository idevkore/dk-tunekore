//! The Analyzer window's commands: open a song, separate it in the stem
//! worker, play and mix the stems, export them.
//!
//! The player gets five tracks: the original song (track 0, muted unless
//! the UI asks to hear it) and the four stems, in `STEM_NAMES` order.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tunekore_analyzer::{
    check, decode_file, export_wav, list_backends, peaks, rms_db, separate, BackendInfo, Cancel, Checks, JobError,
    Song, Stems, WavFormat, WorkerConfig, STEM_NAMES,
};
use tunekore_engine::{Player, Source};
use serde::Serialize;
use tauri::async_runtime::spawn_blocking;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

use crate::Audio;

/// Waveform resolution sent to the UI, per song or stem.
const PEAKS: usize = 1200;

#[derive(Default)]
pub struct Analyzer(Mutex<Inner>);

impl Analyzer {
    /// Cancels the running job, if there is one.
    pub fn cancel_job(&self) {
        if let Some((_, cancel)) = self.0.lock().unwrap().job.as_ref() {
            cancel.cancel();
        }
    }
}

#[derive(Default)]
struct Inner {
    song: Option<Arc<Loaded>>,
    stems: Option<Arc<Stems>>,
    job: Option<(u64, Cancel)>,
    next_job: u64,
}

struct Loaded {
    name: String,
    song: Song,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SongInfo {
    name: String,
    path: String,
    seconds: f64,
    sample_rate: u32,
    channels: usize,
    codec: String,
    bits: Option<u32>,
    peaks: Vec<f32>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StemInfo {
    name: String,
    peaks: Vec<f32>,
    rms_db: f64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Progress {
    job: u64,
    stage: String,
    fraction: f64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Finished {
    job: u64,
    backend: String,
    mode: String,
    stems: Vec<StemInfo>,
    /// Wall time for the whole job, as the user waited for it.
    seconds: f64,
    peak_rss_mb: u64,
    peak_gpu_mb: u64,
    checks: Checks,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Failed {
    job: u64,
    message: String,
    cancelled: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Position {
    position: f64,
    duration: f64,
    playing: bool,
}

type AudioState<'a> = State<'a, &'static Mutex<Audio>>;

/// Opens the Analyzer window, or brings it to the front.
#[tauri::command]
pub fn open_analyzer(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("analyzer") {
        w.unminimize().ok();
        return w.set_focus().map_err(|e| e.to_string());
    }
    let window = WebviewWindowBuilder::new(&app, "analyzer", WebviewUrl::App("analyzer.html".into()))
        .title("tuneKore Analyzer")
        .inner_size(1200.0, 760.0)
        .min_inner_size(900.0, 600.0)
        .background_color(tauri::window::Color(18, 18, 23, 255))
        .build()
        .map_err(|e| e.to_string())?;
    // Closing the window stops the stems playing and any job in progress.
    let handle = app.clone();
    window.on_window_event(move |e| {
        if let tauri::WindowEvent::Destroyed = e {
            handle.state::<Player>().set_playing(false);
            handle.state::<Analyzer>().cancel_job();
        }
    });
    Ok(())
}

#[tauri::command]
pub async fn analyzer_backends() -> Result<Vec<BackendInfo>, String> {
    spawn_blocking(|| list_backends(&WorkerConfig::locate()?)).await.map_err(|e| e.to_string())?
}

/// Decodes a song and loads it into the player on its own.
#[tauri::command]
pub async fn analyzer_open(path: String, state: State<'_, Analyzer>, audio: AudioState<'_>) -> Result<SongInfo, String> {
    let audio: &'static Mutex<Audio> = *audio.inner();
    if let Some((_, cancel)) = state.0.lock().unwrap().job.take() {
        cancel.cancel();
    }
    let p = PathBuf::from(&path);
    let (loaded, info) = spawn_blocking(move || -> Result<_, String> {
        let song = decode_file(&p)?;
        let name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Song".into());
        let info = SongInfo {
            name: name.clone(),
            path: p.display().to_string(),
            seconds: song.seconds(),
            sample_rate: song.sample_rate,
            channels: song.channels.len(),
            codec: song.codec.clone(),
            bits: song.bits,
            peaks: peaks(&song.channels, PEAKS),
        };
        let source = Source { sample_rate: song.sample_rate, tracks: vec![song.channels.clone()] };
        audio.lock().unwrap().engine.load_clip(Arc::new(source))?;
        Ok((Loaded { name, song }, info))
    })
    .await
    .map_err(|e| e.to_string())??;
    let mut inner = state.0.lock().unwrap();
    inner.song = Some(Arc::new(loaded));
    inner.stems = None;
    Ok(info)
}

/// Starts a separation job and returns its id. Progress, the result and
/// any failure arrive as `analyzer:progress`, `analyzer:done` and
/// `analyzer:failed` events carrying that id.
#[tauri::command]
pub fn analyzer_separate(
    backend: String,
    mode: String,
    app: AppHandle,
    state: State<'_, Analyzer>,
    audio: AudioState<'_>,
) -> Result<u64, String> {
    let audio: &'static Mutex<Audio> = *audio.inner();
    let cfg = WorkerConfig::locate()?;
    let mut inner = state.0.lock().unwrap();
    let loaded = inner.song.clone().ok_or("open a song first")?;
    if let Some((_, old)) = inner.job.take() {
        old.cancel();
    }
    inner.next_job += 1;
    let job = inner.next_job;
    let cancel = Cancel::default();
    inner.job = Some((job, cancel.clone()));
    drop(inner);

    let job_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?.join("analyzer").join(format!("job-{job}"));
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let progress = |stage: &str, fraction: f64| {
            let _ = app.emit("analyzer:progress", Progress { job, stage: stage.into(), fraction });
        };
        let result = separate(&cfg, &loaded.song, &backend, &mode, &job_dir, &cancel, progress);
        let state = app.state::<Analyzer>();
        let current = || state.0.lock().unwrap().job.as_ref().map(|(j, _)| *j) == Some(job);
        match result {
            Ok((stems, done)) => {
                let _ = std::fs::remove_dir_all(&job_dir);
                if !current() {
                    return; // a newer job or song replaced this one
                }
                let checks = check(&loaded.song, &stems);
                let mut tracks = vec![loaded.song.channels.clone()];
                tracks.extend(stems.stems.iter().map(|(_, s)| s.clone()));
                let source = Source { sample_rate: stems.sample_rate, tracks };
                if let Err(e) = audio.lock().unwrap().engine.load_clip(Arc::new(source)) {
                    let _ = app.emit("analyzer:failed", Failed { job, message: e, cancelled: false });
                    return;
                }
                let info = stems
                    .stems
                    .iter()
                    .map(|(name, s)| StemInfo { name: name.clone(), peaks: peaks(s, PEAKS), rms_db: finite(rms_db(s)) })
                    .collect();
                {
                    let mut inner = state.0.lock().unwrap();
                    inner.stems = Some(Arc::new(stems));
                    inner.job = None;
                }
                let _ = app.emit(
                    "analyzer:done",
                    Finished {
                        job,
                        backend,
                        mode,
                        stems: info,
                        seconds: started.elapsed().as_secs_f64(),
                        peak_rss_mb: done.peak_rss_mb,
                        peak_gpu_mb: done.peak_gpu_mb,
                        checks,
                    },
                );
            }
            Err(err) => {
                let log = job_dir.join("worker.log");
                let message = match &err {
                    JobError::Cancelled => "Cancelled.".to_string(),
                    JobError::Failed(m) => with_log_tail(m, &log),
                };
                // Keep the last failed job's log; the stems and mix go.
                if let Some(parent) = job_dir.parent() {
                    let _ = std::fs::copy(&log, parent.join("last-failed-job.log"));
                }
                let _ = std::fs::remove_dir_all(&job_dir);
                if current() {
                    state.0.lock().unwrap().job = None;
                }
                let cancelled = err == JobError::Cancelled;
                let _ = app.emit("analyzer:failed", Failed { job, message, cancelled });
            }
        }
    });
    Ok(job)
}

#[tauri::command]
pub fn analyzer_cancel(state: State<'_, Analyzer>) {
    state.cancel_job();
}

/// Plays or pauses, opening the output first if nothing has opened it.
#[tauri::command]
pub async fn analyzer_play(playing: bool, audio: AudioState<'_>, player: State<'_, Player>) -> Result<(), String> {
    let audio: &'static Mutex<Audio> = *audio.inner();
    if playing {
        spawn_blocking(move || audio.lock().unwrap().ensure_open()).await.map_err(|e| e.to_string())??;
    }
    player.set_playing(playing);
    Ok(())
}

#[tauri::command]
pub fn analyzer_seek(seconds: f64, player: State<'_, Player>) {
    player.seek_seconds(seconds);
}

/// Level (linear), mute and solo for one player track (0 = the original).
#[tauri::command]
pub fn analyzer_track(index: usize, gain: f32, mute: bool, solo: bool, player: State<'_, Player>) {
    player.set_track(index, gain, mute, solo);
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Exported {
    path: String,
    format: WavFormat,
}

/// Writes the named stems (or all four) as WAV files into `dir`.
#[tauri::command]
pub async fn analyzer_export(dir: String, names: Vec<String>, state: State<'_, Analyzer>) -> Result<Vec<Exported>, String> {
    let (loaded, stems) = {
        let inner = state.0.lock().unwrap();
        (inner.song.clone().ok_or("open a song first")?, inner.stems.clone().ok_or("separate the song first")?)
    };
    spawn_blocking(move || {
        let names: Vec<&str> = if names.is_empty() { STEM_NAMES.to_vec() } else { names.iter().map(String::as_str).collect() };
        let mut out = Vec::new();
        for name in names {
            let audio = stems.get(name).ok_or_else(|| format!("no stem called {name}"))?;
            let path = Path::new(&dir).join(format!("{} - {}.wav", loaded.name, title(name)));
            let format = export_wav(audio, stems.sample_rate, &path)?;
            out.push(Exported { path: path.display().to_string(), format });
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Sends the playhead to the UI about 30 times a second while playing.
pub fn spawn_position(app: AppHandle, player: Player) {
    std::thread::spawn(move || {
        let mut was_playing = false;
        loop {
            std::thread::sleep(Duration::from_millis(33));
            let playing = player.is_playing();
            if playing || was_playing {
                let _ = app.emit(
                    "analyzer:position",
                    Position { position: player.position_seconds(), duration: player.duration_seconds(), playing },
                );
            }
            was_playing = playing;
        }
    });
}

fn title(name: &str) -> String {
    let mut c = name.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn finite(db: f64) -> f64 {
    if db.is_finite() { db } else { -120.0 }
}

/// The error plus the last lines of the worker's log, which usually say why.
fn with_log_tail(message: &str, log: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(log) else { return message.to_string() };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return message.to_string();
    }
    let tail = lines[lines.len().saturating_sub(6)..].join("\n");
    format!("{message}\n\n{tail}")
}
