//! Finding plugins, safely.
//!
//! Loading a plugin runs its code, and plugins crash, hang and throw. So the app never scans
//! itself: [`scan`] runs the `tunekore-scan` helper program, which probes every plugin in a child
//! process of its own with a timeout, and writes what it found to a JSON cache that [`ScanCache`]
//! reads. A plugin that dies during its probe is recorded with the reason and the scan goes on.
//!
//! ```no_run
//! use tunekore_plugins::scan::{self, ScanOptions};
//!
//! // Scan the standard VST3 folders (or reuse what is cached), then list what loaded.
//! let cache = scan::scan(&ScanOptions::default())?;
//! for p in cache.plugins() {
//!     println!("{} ({}) {:?}", p.info.name, p.info.vendor, p.info.category);
//! }
//! // "Rescan" in the UI forces every plugin to be probed again.
//! let cache = scan::rescan(&ScanOptions::default())?;
//! # Ok::<(), scan::ScanError>(())
//! ```

pub mod bundles;
mod cache;
pub mod probe;
pub mod runner;

pub use cache::{default_cache_path, ScanCache, ScanEntry, CACHE_VERSION};
pub use runner::ScanOptions;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Why a scan could not be run.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("cannot find the tunekore-scan program (looked next to {0}; set ScanOptions::scanner or TUNEKORE_SCAN)")]
    ScannerNotFound(String),
    #[error("cannot start {0}: {1}")]
    Start(PathBuf, std::io::Error),
    #[error("the scan took longer than {0} s and was stopped")]
    TimedOut(u64),
    #[error("the scanner failed: {0}")]
    Failed(String),
    #[error("the scanner finished but left no readable cache at {0}")]
    NoCache(PathBuf),
}

/// Finds the `tunekore-scan` program: `TUNEKORE_SCAN`, then the folder the running program is
/// in (and, for tests, the folder above that).
pub fn find_scanner() -> Option<PathBuf> {
    let name = if cfg!(windows) { "tunekore-scan.exe" } else { "tunekore-scan" };
    if let Some(p) = std::env::var_os("TUNEKORE_SCAN").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let here = std::env::current_exe().ok()?;
    let dir = here.parent()?;
    [dir.join(name), dir.parent()?.join(name)].into_iter().find(|p| p.is_file())
}

/// Scans, in a helper process, and returns the cache. Plugins whose files have not changed since
/// the last scan are not probed again.
pub fn scan(opts: &ScanOptions) -> Result<ScanCache, ScanError> {
    let scanner = match &opts.scanner {
        Some(p) => p.clone(),
        None => find_scanner().ok_or_else(|| ScanError::ScannerNotFound(std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.display().to_string())).unwrap_or_default()))?,
    };
    let mut cmd = Command::new(&scanner);
    cmd.arg("--cache").arg(&opts.cache_path).arg("--timeout").arg(opts.timeout.as_secs().max(1).to_string()).arg("--jobs").arg(opts.jobs.to_string()).arg("--quiet");
    if !opts.default_paths {
        cmd.arg("--no-default-paths");
    }
    if opts.force {
        cmd.arg("--force");
    }
    for p in &opts.search_paths {
        cmd.arg("--path").arg(p);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| ScanError::Start(scanner.clone(), e))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => break,
            Ok(Some(st)) => {
                let mut err = String::new();
                if let Some(mut s) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut s, &mut err);
                }
                return Err(ScanError::Failed(format!("{st}: {}", err.lines().last().unwrap_or("").trim())));
            }
            Ok(None) if started.elapsed() >= opts.total_timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ScanError::TimedOut(opts.total_timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(ScanError::Failed(e.to_string())),
        }
    }
    ScanCache::read(&opts.cache_path).ok_or_else(|| ScanError::NoCache(opts.cache_path.clone()))
}

/// Like [`scan`], but probes every plugin again, whether or not it changed.
pub fn rescan(opts: &ScanOptions) -> Result<ScanCache, ScanError> {
    scan(&ScanOptions { force: true, ..opts.clone() })
}
