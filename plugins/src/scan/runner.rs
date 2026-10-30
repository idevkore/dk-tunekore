//! The scanner's own work: find the bundles, probe each in a child process of its own, collect
//! the answers, write the cache. This is what the `tunekore-scan` binary runs.
//!
//! A plugin that crashes, aborts, hangs or throws while loading takes down only its probe
//! process. The probe is killed after `timeout` and the bundle is recorded with the reason.

use super::bundles;
use super::cache::ScanCache;
use super::cache::{ScanEntry, ScanStatus};
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
    /// Probe the plugins that failed last time again (and only those, unless they changed).
    pub retry_failed: bool,
    /// The `tunekore-scan` program. `None` looks next to the running program, then in
    /// `TUNEKORE_SCAN`.
    pub scanner: Option<PathBuf>,
    /// How long the whole scan may take. The scanner then stops the probes still running
    /// (their plugins are recorded as timed out) and writes what it found; [`super::scan`] kills
    /// the scanner and its probes if it overruns that by 15 s.
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
            retry_failed: false,
            scanner: None,
            total_timeout: Duration::from_secs(30 * 60),
        }
    }
}

/// The file beside the cache that lists the probes running right now (one process id per
/// line), so that whoever started the scanner can kill them if the scanner itself is killed.
pub fn running_probes_path(cache: &Path) -> PathBuf {
    let mut name = cache.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".probes");
    cache.with_file_name(name)
}

/// What the workers share while the scan runs.
struct Progress {
    /// Entries from the cache for bundles that have not changed.
    kept: Vec<ScanEntry>,
    /// Entries the cache had for bundles being probed again, used until the probe answers (so a
    /// scan stopped early still lists them).
    previous: HashMap<PathBuf, Vec<ScanEntry>>,
    /// Answers so far.
    found: Vec<ScanEntry>,
    /// Probes running now.
    running: Vec<u32>,
}

impl Progress {
    fn cache(&self, paths: &[PathBuf]) -> ScanCache {
        let mut entries = self.kept.clone();
        entries.extend(self.found.iter().cloned());
        for old in self.previous.values() {
            entries.extend(old.iter().cloned());
        }
        ScanCache::new(paths.to_vec(), entries)
    }
}

/// Runs the scan in this process (spawning one probe process per bundle that needs one), writes
/// the cache and returns it. `exe` is the program to run as the probe: the scanner itself.
/// `progress` is told about each bundle as it finishes.
///
/// The cache is rewritten after every bundle, so a scan stopped early keeps what it found. When
/// `opts.total_timeout` runs out, no more probes start and the running ones are killed (their
/// plugins are recorded as timed out); the scan then finishes normally.
pub fn run(opts: &ScanOptions, exe: &Path, progress: &(dyn Fn(&str) + Sync)) -> std::io::Result<ScanCache> {
    let deadline = Instant::now() + opts.total_timeout;
    let mut dirs = if opts.default_paths { bundles::default_search_paths() } else { Vec::new() };
    dirs.extend(opts.search_paths.iter().cloned());
    dirs.sort();
    dirs.dedup();
    let found = bundles::find_bundles(&dirs);

    // Reuse what the cache already knows about bundles that have not changed, except plugins that
    // timed out: those get another try every scan.
    let mut cached: HashMap<PathBuf, Vec<ScanEntry>> = HashMap::new();
    if !opts.force
        && let Some(old) = ScanCache::read(&opts.cache_path)
    {
        for e in old.entries {
            cached.entry(e.info.path.clone()).or_default().push(e);
        }
    }
    let mut state = Progress { kept: Vec::new(), previous: HashMap::new(), found: Vec::new(), running: Vec::new() };
    let mut todo: Vec<PathBuf> = Vec::new();
    for b in &found {
        let now = stamp(b);
        match cached.remove(b) {
            Some(old)
                if !now.is_empty()
                    && old.iter().all(|e| e.stamp == now && e.status != ScanStatus::TimedOut)
                    && !(opts.retry_failed && old.iter().any(|e| e.status != ScanStatus::Ok)) =>
            {
                progress(&format!("{}: unchanged, {} plugin(s) from the cache", b.display(), old.len()));
                state.kept.extend(old);
            }
            old => {
                if let Some(old) = old {
                    state.previous.insert(b.clone(), old);
                }
                todo.push(b.clone());
            }
        }
    }

    let work = tempdir()?;
    let probes_file = running_probes_path(&opts.cache_path);
    let next = AtomicUsize::new(0);
    let state = Mutex::new(state);
    let lock = || state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let total = todo.len();
    std::thread::scope(|s| {
        for _ in 0..opts.jobs.clamp(1, 16) {
            s.spawn(|| loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(bundle) = todo.get(i) else { break };
                let t = Instant::now();
                let on_start = |pid: u32| {
                    let mut g = lock();
                    g.running.push(pid);
                    write_running(&probes_file, &g.running);
                };
                let found = probe_in_child(exe, bundle, opts.timeout, left, &work, i, &on_start);
                let bad = found.iter().filter(|e| e.status != ScanStatus::Ok).count();
                progress(&format!(
                    "[{}/{total}] {}: {} plugin(s){} in {} ms",
                    i + 1,
                    bundle.file_name().map_or_else(|| bundle.display().to_string(), |n| n.to_string_lossy().into_owned()),
                    found.len() - bad,
                    if bad > 0 { format!(", {bad} failed: {}", found.iter().find_map(|e| e.error.as_deref()).unwrap_or("")) } else { String::new() },
                    t.elapsed().as_millis()
                ));
                let mut g = lock();
                g.previous.remove(bundle);
                g.found.extend(found);
                g.running.retain(|&p| !probe_pids_of(bundle, &work, i).contains(&p));
                write_running(&probes_file, &g.running);
                // Rewritten after every bundle, so a scan cut short keeps what it found.
                let _ = g.cache(&dirs).write(&opts.cache_path);
            });
        }
    });
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&probes_file);
    let state = state.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
    if next.load(Ordering::Relaxed) < total {
        progress(&format!("the scan's time limit ran out with {} bundle(s) not probed; they keep their old entries, if any", total.saturating_sub(next.load(Ordering::Relaxed))));
    }
    let cache = state.cache(&dirs);
    cache.write(&opts.cache_path)?;
    Ok(cache)
}

/// The probe pid recorded for bundle `n` (its pid file in the work folder).
fn probe_pids_of(_bundle: &Path, work: &Path, n: usize) -> Vec<u32> {
    std::fs::read_to_string(work.join(format!("{n}.pid"))).ok().and_then(|s| s.trim().parse().ok()).into_iter().collect()
}

fn write_running(path: &Path, pids: &[u32]) {
    let text: String = pids.iter().map(|p| format!("{p}\n")).collect();
    let _ = std::fs::write(path, text);
}

/// Kills the probes a scanner listed as running (after killing the scanner itself).
pub fn kill_listed_probes(cache: &Path) {
    let path = running_probes_path(cache);
    if let Ok(text) = std::fs::read_to_string(&path) {
        for pid in text.lines().filter_map(|l| l.trim().parse::<i32>().ok()) {
            kill_pgid(pid);
        }
    }
    let _ = std::fs::remove_file(&path);
}

fn tempdir() -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("tunekore-scan-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos())));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Runs `exe --probe <bundle>` and returns what it found, or one failed entry saying why not.
/// The probe gets `timeout`, or `left` (what remains of the whole scan) if that is shorter.
fn probe_in_child(exe: &Path, bundle: &Path, timeout: Duration, left: Duration, work: &Path, n: usize, on_start: &dyn Fn(u32)) -> Vec<ScanEntry> {
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
        Err(e) => return vec![failed_entry(bundle, ScanStatus::Failed, format!("cannot start the probe: {e}"))],
    };
    let _ = std::fs::write(work.join(format!("{n}.pid")), child.id().to_string());
    on_start(child.id());
    let cut_short = left < timeout;
    let timeout = timeout.min(left);
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
            Err(e) => return vec![failed_entry(bundle, ScanStatus::Failed, format!("cannot wait for the probe: {e}"))],
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
    let (kind, why) = if timed_out && cut_short {
        (ScanStatus::TimedOut, "timed out: the scan's overall time limit ran out while this plugin was loading".to_string())
    } else if timed_out {
        (ScanStatus::TimedOut, format!("timed out: the plugin did not finish loading in {} s", timeout.as_secs()))
    } else {
        match status {
            Some(st) => (ScanStatus::Crashed, describe_exit(st)),
            None => (ScanStatus::Crashed, "the probe ended without a result".into()),
        }
    };
    let tail = stderr_tail(&errlog);
    vec![failed_entry(bundle, kind, if tail.is_empty() { why } else { format!("{why} (last output: {tail})") })]
}

fn kill_group(child: &mut std::process::Child) {
    if !kill_pgid(child.id() as i32) {
        let _ = child.kill();
    }
}

/// Kills process group `pgid` (a probe and anything it started). `false` if that failed.
fn kill_pgid(pgid: i32) -> bool {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn killpg(pgrp: i32, sig: i32) -> i32;
        }
        // SAFETY: `killpg` is the C library function; a probe is started as the leader of its own
        // group, so its pid is the group id.
        pgid > 0 && unsafe { killpg(pgid, 9) } == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        false
    }
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
