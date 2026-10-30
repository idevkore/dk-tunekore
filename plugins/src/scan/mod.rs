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
//! // The failed ones, with why (the blocklist the UI shows):
//! for p in cache.failures() {
//!     println!("{}: {:?}, {}", p.info.name, p.status, p.error.as_deref().unwrap_or(""));
//! }
//! // "Retry failed plugins" in the UI probes just those again; "Rescan" probes everything.
//! let cache = scan::retry_failed(&ScanOptions::default())?;
//! let cache = scan::rescan(&ScanOptions::default())?;
//! # Ok::<(), scan::ScanError>(())
//! ```

pub mod bundles;
mod cache;
pub mod probe;
pub mod runner;

pub use cache::{default_cache_path, ScanCache, ScanEntry, ScanStatus, CACHE_VERSION, TIMEOUT_RETRIES};
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
    cmd.arg("--cache")
        .arg(&opts.cache_path)
        .arg("--timeout")
        .arg(opts.timeout.as_secs().max(1).to_string())
        .arg("--deadline")
        .arg(opts.total_timeout.as_secs().max(1).to_string())
        .arg("--jobs")
        .arg(opts.jobs.to_string())
        .arg("--quiet");
    if !opts.default_paths {
        cmd.arg("--no-default-paths");
    }
    if opts.force {
        cmd.arg("--force");
    }
    if opts.retry_failed {
        cmd.arg("--retry-failed");
    }
    for p in &opts.search_paths {
        cmd.arg("--path").arg(p);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| ScanError::Start(scanner.clone(), e))?;
    let started = Instant::now();
    // The scanner keeps to `total_timeout` itself (it stops its probes and writes what it found);
    // this is the backstop in case the scanner itself is stuck.
    let backstop = opts.total_timeout + Duration::from_secs(15);
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
            Ok(None) if started.elapsed() >= backstop => {
                let _ = child.kill();
                let _ = child.wait();
                // Its probes run in process groups of their own; it lists them for this.
                runner::kill_listed_probes(&opts.cache_path, child.id());
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

/// Like [`scan`], but gives the plugins that failed last time another try (after the user fixed
/// a licence, say). Plugins that loaded are taken from the cache as usual.
pub fn retry_failed(opts: &ScanOptions) -> Result<ScanCache, ScanError> {
    scan(&ScanOptions { retry_failed: true, ..opts.clone() })
}
