//! The Analyzer (Phase 1): open a song, split it into vocals, drums, bass
//! and other in the stem worker, check the stems against the song, and
//! export them.
//!
//! The model itself sits behind the worker's `Separator` interface
//! (`stems/tunekore_stems/separator.py`), so the app never depends on which
//! model runs.

mod decode;
mod stems;
mod worker;

pub use decode::{decode_file, Song, EXTENSIONS};
pub use stems::{check, export_wav, peaks, read_planar, rms_db, write_planar, Checks, Stems, WavFormat, STEM_NAMES};
pub use worker::{list_backends, run_job, BackendInfo, Cancel, Done, JobError, Request, WorkerConfig};

use std::path::Path;

/// Writes the song for the worker and runs one separation job in `job_dir`.
pub fn separate(
    cfg: &WorkerConfig,
    song: &Song,
    backend: &str,
    mode: &str,
    job_dir: &Path,
    cancel: &Cancel,
    progress: impl FnMut(&str, f64),
) -> Result<(Stems, Done), JobError> {
    let fail = JobError::Failed;
    std::fs::create_dir_all(job_dir).map_err(|e| fail(format!("can't make {}: {e}", job_dir.display())))?;
    let input = job_dir.join("mix.f32");
    write_planar(&input, &song.channels).map_err(fail)?;
    let req = Request {
        input: input.clone(),
        channels: song.channels.len(),
        frames: song.frames(),
        sample_rate: song.sample_rate,
        backend: backend.into(),
        mode: mode.into(),
        out_dir: job_dir.to_path_buf(),
    };
    let done = run_job(cfg, &req, &job_dir.join("worker.log"), cancel, progress)?;
    let stems = Stems::read(&done.stems, song).map_err(fail)?;
    let _ = std::fs::remove_file(&input);
    Ok((stems, done))
}
