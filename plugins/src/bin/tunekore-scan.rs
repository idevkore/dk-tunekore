//! The plugin scanner: a helper program the app runs so that a plugin which crashes or hangs
//! while loading cannot take the app down.
//!
//!     tunekore-scan [options]
//!         --cache FILE           where to read and write the JSON cache
//!         --path DIR             also search DIR (repeatable)
//!         --no-default-paths     skip the standard VST3 folders
//!         --timeout SECONDS      how long one plugin may take to load (default 60)
//!         --deadline SECONDS     how long the whole scan may take (default 1800); then the
//!                                probes still running are stopped and the cache is written
//!         --jobs N               plugins probed at once (default 4)
//!         --force                probe every plugin again, even if it has not changed
//!         --retry-failed         probe the plugins that failed last time again
//!         --quiet                no progress lines
//!
//! It starts one copy of itself per plugin bundle (`--probe BUNDLE --result FILE`, an internal
//! mode), kills a copy that takes too long, and records a copy that crashed with the reason.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use tunekore_plugins::scan::{probe, runner, ScanOptions};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().map(String::as_str) == Some("--probe") {
        return probe_mode(args.collect());
    }
    let mut opts = ScanOptions::default();
    let mut quiet = false;
    while let Some(a) = args.next() {
        let mut value = |what: &str| args.next().ok_or_else(|| format!("{a} needs {what}"));
        let r: Result<(), String> = (|| {
            match a.as_str() {
                "--cache" => opts.cache_path = PathBuf::from(value("a file")?),
                "--path" => opts.search_paths.push(PathBuf::from(value("a folder")?)),
                "--no-default-paths" => opts.default_paths = false,
                "--timeout" => opts.timeout = Duration::from_secs(value("seconds")?.parse().map_err(|_| "--timeout needs a whole number of seconds")?),
                "--jobs" => opts.jobs = value("a number")?.parse().map_err(|_| "--jobs needs a number")?,
                "--deadline" => opts.total_timeout = Duration::from_secs(value("seconds")?.parse().map_err(|_| "--deadline needs a whole number of seconds")?),
                "--force" => opts.force = true,
                "--retry-failed" => opts.retry_failed = true,
                "--quiet" => quiet = true,
                "--help" | "-h" => return Err("usage: tunekore-scan [--cache FILE] [--path DIR]... [--no-default-paths] [--timeout SECONDS] [--deadline SECONDS] [--jobs N] [--force] [--retry-failed] [--quiet]".into()),
                other => return Err(format!("unknown option {other}")),
            }
            Ok(())
        })();
        if let Err(e) = r {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot find myself: {e}");
            return ExitCode::FAILURE;
        }
    };
    let progress = |line: &str| {
        if !quiet {
            eprintln!("{line}");
        }
    };
    match runner::run(&opts, &exe, &progress) {
        Ok(cache) => {
            if !quiet {
                eprintln!("{} plugin(s) usable, {} failed; cache written to {}", cache.plugins().count(), cache.failures().count(), opts.cache_path.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("scan failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `--probe BUNDLE --result FILE`: loads the bundle's plugins and writes what they look like.
fn probe_mode(args: Vec<String>) -> ExitCode {
    let (Some(bundle), Some(result)) = (args.get(1), args.iter().position(|a| a == "--result").and_then(|i| args.get(i + 1))) else {
        eprintln!("usage: tunekore-scan --probe BUNDLE --result FILE");
        return ExitCode::from(2);
    };
    let entries = probe::probe_bundle(std::path::Path::new(bundle));
    let tmp = format!("{result}.tmp");
    let written = serde_json::to_vec(&entries).map_err(std::io::Error::other).and_then(|j| std::fs::write(&tmp, j)).and_then(|()| std::fs::rename(&tmp, result));
    if let Err(e) = written {
        eprintln!("cannot write the result: {e}");
        return ExitCode::FAILURE;
    }
    // Leave without running the plugins' exit handlers and static destructors: some of them crash,
    // and there is nothing left to tidy in a process that is about to end.
    unsafe extern "C" {
        fn _exit(code: i32) -> !;
    }
    // SAFETY: `_exit` is the C library function; the result file is complete and closed.
    unsafe { _exit(0) }
}

