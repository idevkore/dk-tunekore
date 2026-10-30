//! The scanner's own work: find the bundles, probe each in a child process of its own, collect
//! the answers, write the cache. This is what the `tunekore-scan` binary runs.
//!
//! A plugin that crashes, aborts, hangs or throws while loading takes down only its probe
//! process. The probe is killed after `timeout` and the bundle is recorded with the reason.

use super::bundles;
use super::cache::ScanCache;
use super::cache::ScanEntry;
use super::probe::{failed_entry, stamp};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What to scan and how.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Where the JSON cache is read from and written to.
    pub cache_path: PathBuf,
    /// Folders to search besides the standard ones.
    pub search_paths: Vec<PathBuf>,
    /// Search the standard VST3 folders (`/Library/Audio/Plug-Ins/VST3`, the user's own, and
    /// `VST3_PATH`).
    pub default_paths: bool,
    /// How long one bundle may take to load before its probe is killed.
    pub timeout: Duration,
    /// Bundles probed at once.
    pub jobs: usize,
    /// Probe everything again, ignoring what the cache holds.
    pub force: bool,
    /// The `tunekore-scan` program. `None` looks next to the running program, then in
    /// `TUNEKORE_SCAN`.
    pub scanner: Option<PathBuf>,
    /// How long the whole scan may take before the helper is killed.
    pub total_timeout: Duration,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            cache_path: super::cache::default_cache_path(),
            search_paths: Vec::new(),
            default_paths: true,
            timeout: Duration::from_secs(60),
            jobs: std::thread::available_parallelism().map_or(2, |n| n.get().min(4)),
            force: false,
            scanner: None,
            total_timeout: Duration::from_secs(30 * 60),
        }
    }
}

/// Runs the scan in this process (spawning one probe process per bundle that needs one), writes
/// the cache and returns it. `exe` is the program to run as the probe: the scanner itself.
/// `progress` is told about each bundle as it finishes.
pub fn run(opts: &ScanOptions, exe: &Path, progress: &(dyn Fn(&str) + Sync)) -> std::io::Result<ScanCache> {
    let mut dirs = if opts.default_paths { bundles::default_search_paths() } else { Vec::new() };
    dirs.extend(opts.search_paths.iter().cloned());
    let found = bundles::find_bundles(&dirs);

    // Reuse what the cache already knows about bundles that have not changed.
    let mut reused: HashMap<PathBuf, Vec<ScanEntry>> = HashMap::new();
    if !opts.force
        && let Some(old) = ScanCache::read(&opts.cache_path)
    {
        for e in old.entries {
            reused.entry(e.info.path.clone()).or_default().push(e);
        }
    }
    let mut entries: Vec<ScanEntry> = Vec::new();
    let mut todo: Vec<PathBuf> = Vec::new();
    for b in &found {
        let now = stamp(b);
        match reused.remove(b) {
            Some(old) if !now.is_empty() && old.iter().all(|e| e.stamp == now) => {
                progress(&format!("{}: unchanged, {} plugin(s) from the cache", b.display(), old.len()));
                entries.extend(old);
            }
            _ => todo.push(b.clone()),
        }
    }

    let work = tempdir()?;
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<ScanEntry>> = Mutex::new(Vec::new());
    let total = todo.len();
    std::thread::scope(|s| {
        for _ in 0..opts.jobs.clamp(1, 16) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(bundle) = todo.get(i) else { break };
                let t = Instant::now();
                let found = probe_in_child(exe, bundle, opts.timeout, &work, i);
                let bad = found.iter().filter(|e| e.error.is_some()).count();
                progress(&format!(
                    "[{}/{total}] {}: {} plugin(s){} in {} ms",
                    i + 1,
                    bundle.file_name().map_or_else(|| bundle.display().to_string(), |n| n.to_string_lossy().into_owned()),
                    found.len() - bad,
                    if bad > 0 { format!(", {bad} failed: {}", found.iter().find_map(|e| e.error.as_deref()).unwrap_or("")) } else { String::new() },
                    t.elapsed().as_millis()
                ));
                results.lock().unwrap_or_else(std::sync::PoisonError::into_inner).extend(found);
            });
        }
    });
    let _ = std::fs::remove_dir_all(&work);
    entries.extend(results.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner));

    let mut paths = dirs;
    paths.sort();
    paths.dedup();
    let cache = ScanCache::new(paths, entries);
    cache.write(&opts.cache_path)?;
    Ok(cache)
}

fn tempdir() -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("tunekore-scan-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos())));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Runs `exe --probe <bundle>` and returns what it found, or one failed entry saying why not.
fn probe_in_child(exe: &Path, bundle: &Path, timeout: Duration, work: &Path, n: usize) -> Vec<ScanEntry> {
    let result = work.join(format!("{n}.json"));
    let errlog = work.join(format!("{n}.err"));
    let stderr = match File::create(&errlog) {
        Ok(f) => Stdio::from(f),
        Err(_) => Stdio::null(),
    };
    let mut cmd = Command::new(exe);
    cmd.arg("--probe").arg(bundle).arg("--result").arg(&result).stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr);
    #[cfg(unix)]
    {
        // Its own process group, so a hung probe and anything it started can be killed together.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return vec![failed_entry(bundle, format!("cannot start the probe: {e}"))],
    };
    let started = Instant::now();
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(st)) => break (Some(st), false),
            Ok(None) if started.elapsed() >= timeout => {
                kill_group(&mut child);
                let _ = child.wait();
                break (None, true);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return vec![failed_entry(bundle, format!("cannot wait for the probe: {e}"))],
        }
    };
    // The probe writes its answer before it exits, so an answer is good even if the exit was not
    // (some plugins crash in their destructors).
    if !timed_out
        && let Ok(bytes) = std::fs::read(&result)
        && let Ok(entries) = serde_json::from_slice::<Vec<ScanEntry>>(&bytes)
    {
        return entries;
    }
    let why = if timed_out {
        format!("timed out: the plugin did not finish loading in {} s", timeout.as_secs())
    } else {
        match status {
            Some(st) => describe_exit(st),
            None => "the probe ended without a result".into(),
        }
    };
    let tail = stderr_tail(&errlog);
    vec![failed_entry(bundle, if tail.is_empty() { why } else { format!("{why} (last output: {tail})") })]
}

fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn killpg(pgrp: i32, sig: i32) -> i32;
        }
        // SAFETY: `killpg` is the C library function; the child was started as the leader of its
        // own group, so its pid is the group id.
        if unsafe { killpg(child.id() as i32, 9) } == 0 {
            return;
        }
    }
    let _ = child.kill();
}

fn describe_exit(st: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = st.signal() {
            let name = match sig {
                4 => "SIGILL",
                5 => "SIGTRAP",
                6 => "SIGABRT",
                7 | 10 => "SIGBUS",
                8 => "SIGFPE",
                9 => "SIGKILL",
                11 => "SIGSEGV",
                15 => "SIGTERM",
                _ => "a signal",
            };
            return format!("crashed while loading ({name}, signal {sig})");
        }
    }
    match st.code() {
        Some(0) => "the probe ended without a result".into(),
        Some(c) => format!("the probe exited with status {c} while loading the plugin"),
        None => "the probe ended abnormally".into(),
    }
}

/// The last non-empty line of the probe's stderr, shortened.
fn stderr_tail(path: &Path) -> String {
    let mut buf = Vec::new();
    if let Ok(f) = File::open(path) {
        let _ = f.take(8192).read_to_end(&mut buf);
    }
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    line.chars().take(200).collect()
}
