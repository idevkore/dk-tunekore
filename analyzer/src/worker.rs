//! Runs the stem worker (`stems/`, Python) as a child process, one per job.
//!
//! The protocol is JSON lines; see `stems/tunekore_stems/worker.py`. A job is
//! cancelled by killing its process, so a stuck or crashed model only ever
//! fails that one job.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Where the worker lives. Phase 1 runs it from this checkout's `stems/`
/// folder on Nate's Mac; nothing about it is bundled into the app.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub python: PathBuf,
    pub dir: PathBuf,
}

impl WorkerConfig {
    /// `TUNEKORE_STEMS_DIR` and `TUNEKORE_STEMS_PYTHON` override the defaults:
    /// `stems/` next to this crate, and the virtualenv `uv sync` makes there.
    pub fn locate() -> Result<Self, String> {
        let dir = std::env::var_os("TUNEKORE_STEMS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../stems"));
        let python = std::env::var_os("TUNEKORE_STEMS_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.join(".venv/bin/python"));
        if !dir.join("tunekore_stems/worker.py").exists() {
            return Err(format!("no stem worker in {}", dir.display()));
        }
        if !python.exists() {
            return Err(format!(
                "the stem worker isn't set up yet. In a terminal: cd {} && uv sync",
                dir.display()
            ));
        }
        Ok(Self { python, dir })
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.python);
        cmd.args(["-u", "-m", "tunekore_stems.worker"]).current_dir(&self.dir).env("PYTHONUNBUFFERED", "1");
        cmd
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BackendInfo {
    pub id: String,
    pub name: String,
    pub modes: Vec<String>,
    #[serde(default)]
    pub mode_labels: HashMap<String, String>,
    pub available: bool,
    pub reason: Option<String>,
}

/// Asks the worker which separators it has and whether each can run.
pub fn list_backends(cfg: &WorkerConfig) -> Result<Vec<BackendInfo>, String> {
    let out = cfg
        .command()
        .arg("--list")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("can't start the stem worker: {e}"))?;
    let line = String::from_utf8_lossy(&out.stdout);
    #[derive(Deserialize)]
    struct Msg {
        backends: Vec<BackendInfo>,
    }
    let msg: Msg = serde_json::from_str(line.trim())
        .map_err(|e| format!("the stem worker answered something unexpected ({e}): {}", line.trim()))?;
    Ok(msg.backends)
}

/// One separation job.
#[derive(Debug, Clone, Serialize)]
pub struct Request {
    /// Planar f32 little-endian mix, written by [`crate::write_planar`].
    pub input: PathBuf,
    pub channels: usize,
    pub frames: usize,
    pub sample_rate: u32,
    pub backend: String,
    pub mode: String,
    pub out_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Done {
    pub stems: HashMap<String, PathBuf>,
    /// Model load plus separation, in the worker.
    pub seconds: f64,
    pub peak_rss_mb: u64,
    pub peak_gpu_mb: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobError {
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobError::Cancelled => write!(f, "cancelled"),
            JobError::Failed(m) => write!(f, "{m}"),
        }
    }
}

/// Lets another thread cancel a running job.
#[derive(Clone, Default)]
pub struct Cancel {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

impl Cancel {
    /// Kills the job's worker. Safe to call before it starts or after it ends.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
enum Event {
    Progress { stage: String, fraction: f64 },
    Done(Done),
    Error { message: String },
}

/// Runs a job to the end on this thread. `progress(stage, fraction)` gets
/// the worker's progress; the worker's stderr goes to `log`.
pub fn run_job(
    cfg: &WorkerConfig,
    req: &Request,
    log: &Path,
    cancel: &Cancel,
    mut progress: impl FnMut(&str, f64),
) -> Result<Done, JobError> {
    let fail = |m: String| JobError::Failed(m);
    let log_file = File::create(log).map_err(|e| fail(format!("can't write the job log: {e}")))?;
    let mut child = cfg
        .command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log_file))
        .spawn()
        .map_err(|e| fail(format!("can't start the stem worker: {e}")))?;
    let mut stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    {
        let mut slot = cancel.child.lock().unwrap();
        if cancel.is_cancelled() {
            let _ = child.kill();
        }
        *slot = Some(child);
    }
    let line = serde_json::to_string(req).expect("request serializes");
    let _ = writeln!(stdin, "{line}");
    drop(stdin);

    let mut result: Option<Result<Done, JobError>> = None;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        match serde_json::from_str::<Event>(&line) {
            Ok(Event::Progress { stage, fraction }) => progress(&stage, fraction),
            Ok(Event::Done(done)) => result = Some(Ok(done)),
            Ok(Event::Error { message }) => result = Some(Err(fail(message))),
            Err(_) => {} // not ours; the worker routes library output to stderr, but be lenient
        }
    }
    let status = cancel.child.lock().unwrap().take().map(|mut c| c.wait());
    if cancel.is_cancelled() {
        return Err(JobError::Cancelled);
    }
    match result {
        Some(r) => r,
        None => {
            let code = match status {
                Some(Ok(s)) => s.to_string(),
                _ => "unknown status".into(),
            };
            Err(fail(format!("the stem worker stopped without finishing ({code})")))
        }
    }
}
