//! The scanner: probes each plugin in a child process of its own, so crashes, hangs and
//! exceptions end up in the cache as errors and the scan carries on.

mod common;

use common::{bad_plugin, fixture_bundle, scratch_dir, Misbehaviour};
use std::time::{Duration, Instant};
use tunekore_plugins::scan::{self, ScanCache, ScanOptions, ScanStatus};
use tunekore_plugins::vst3;
use tunekore_plugins::{Category, PluginError};

/// Scan options for a test folder. The per-plugin timeout is long: probes of plugins that do not
/// hang must never time out, however busy the machine (a loaded Mac running other builds can take
/// seconds to start a probe). Tests about hangs set their own, short, limits.
fn options(dir: &std::path::Path, cache: &std::path::Path) -> ScanOptions {
    ScanOptions {
        cache_path: cache.to_path_buf(),
        search_paths: vec![dir.to_path_buf()],
        default_paths: false,
        timeout: Duration::from_secs(120),
        jobs: 4,
        force: false,
        retry_failed: false,
        scanner: Some(env!("CARGO_BIN_EXE_tunekore-scan").into()),
        total_timeout: Duration::from_secs(600),
    }
}

/// Every bundle in `dir` has at least one entry in `cache`: a scan never loses a plugin.
fn assert_every_bundle_listed(dir: &std::path::Path, cache: &ScanCache) {
    let listing = || cache.entries.iter().map(|e| format!("{} [{:?}] {}", e.info.path.display(), e.status, e.error.as_deref().unwrap_or(""))).collect::<Vec<_>>().join("\n");
    for b in std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "vst3")) {
        assert!(cache.entries.iter().any(|e| e.info.path == b), "{} has no entry; the cache has:\n{}", b.display(), listing());
    }
}

/// A plugin folder with the good fixture and one of each misbehaving plugin (the one that hangs
/// only if `hang`); `None` without a C++ compiler.
fn mixed_folder(tag: &str, hang: bool) -> Option<std::path::PathBuf> {
    let dir = scratch_dir(&format!("scan-mixed-{tag}"));
    for (name, mode) in [("Throws", Misbehaviour::ThrowsInInitialize), ("Aborts", Misbehaviour::AbortsOnLoad), ("ThrowsEarly", Misbehaviour::ThrowsOnLoad)] {
        bad_plugin(&dir, name, mode)?;
    }
    if hang {
        bad_plugin(&dir, "Hangs", Misbehaviour::HangsOnLoad)?;
    }
    // The good one, as a copy so the folder holds only what this test put there.
    let target = dir.join("Fixture.vst3");
    let status = std::process::Command::new("cp").arg("-R").arg(fixture_bundle()).arg(&target).status().ok()?;
    status.success().then_some(dir)
}

#[test]
fn a_plugin_that_throws_from_initialize_is_an_error_not_an_abort() {
    let dir = scratch_dir("throws-in-process");
    let Some(bundle) = bad_plugin(&dir, "Throws", Misbehaviour::ThrowsInInitialize) else { return eprintln!("skipped: no C++ compiler") };
    // Loaded in this very process: without the shim, this would abort the test run.
    let infos = vst3::classes(&bundle).expect("the class list is readable");
    assert_eq!(infos.len(), 1);
    match vst3::load(&infos[0]) {
        Err(PluginError::Exception { during, message }) => {
            assert_eq!(during, "IComponent::initialize");
            assert!(message.contains("boom from initialize"), "{message}");
        }
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("loaded a plugin that throws"),
    }
    // And the host is still usable afterwards.
    assert!(vst3::load(&common::fixture_info("Fixture Synth")).is_ok());
}

#[test]
fn a_bundle_that_throws_while_loading_is_an_error_too() {
    let dir = scratch_dir("throws-on-load");
    let Some(bundle) = bad_plugin(&dir, "ThrowsEarly", Misbehaviour::ThrowsOnLoad) else { return eprintln!("skipped: no C++ compiler") };
    let err = vst3::classes(&bundle).unwrap_err().to_string();
    assert!(err.contains("GetPluginFactory threw") && err.contains("boom from GetPluginFactory"), "{err}");
}

#[test]
fn the_scan_survives_crashes_and_exceptions_and_reports_each() {
    let Some(dir) = mixed_folder("survives", false) else { return eprintln!("skipped: no C++ compiler") };
    let cache_path = dir.join("cache.json");
    let cache = scan::scan(&options(&dir, &cache_path)).expect("the scan runs");
    assert_every_bundle_listed(&dir, &cache);

    let good: Vec<&str> = cache.plugins().map(|e| e.info.name.as_str()).collect();
    assert_eq!(good, vec!["Fixture Gain", "Fixture Synth", "Fixture Thru"], "the good plugins are listed despite the bad ones");
    let synth = cache.find("Fixture Synth").unwrap();
    assert_eq!(synth.info.category, Category::Instrument);
    assert_eq!(synth.buses.event_inputs, 1);
    assert_eq!(synth.buses.audio_outputs.len(), 1);
    assert_eq!(synth.buses.audio_outputs[0].channels, 2);
    let gain = cache.find("Fixture Gain").unwrap();
    assert_eq!((gain.buses.audio_inputs.len(), gain.buses.audio_outputs.len()), (1, 1));
    assert_eq!(gain.params, 7);

    let listing = || cache.entries.iter().map(|e| format!("{} [{:?}] {}", e.info.name, e.status, e.error.as_deref().unwrap_or(""))).collect::<Vec<_>>().join("\n");
    let entry = |name: &str| cache.find(name).unwrap_or_else(|| panic!("no entry for {name}; the cache has:\n{}", listing()));
    let err = |name: &str| entry(name).error.clone().unwrap_or_else(|| panic!("{name} should have failed"));
    // The plugin is named "Bad Plugin" in its bundle "Throws.vst3": its probe loaded it far enough
    // to read its name, then initialize threw.
    assert_eq!(entry("Bad Plugin").status, ScanStatus::Failed, "threw an exception: caught, a plain failure");
    assert!(err("Bad Plugin").contains("boom from initialize"));
    // These never got as far as a name: listed by bundle.
    assert_eq!(entry("ThrowsEarly").status, ScanStatus::Failed);
    assert!(err("ThrowsEarly").contains("boom from GetPluginFactory"));
    assert_eq!(entry("Aborts").status, ScanStatus::Crashed);
    assert!(err("Aborts").contains("crashed while loading (SIGABRT"));
    assert_eq!(cache.failures().count(), 3);

    // The file on disk is what was returned.
    assert_eq!(ScanCache::read(&cache_path), Some(cache));
}

#[test]
fn a_second_scan_reuses_the_cache_and_a_rescan_does_not() {
    let Some(dir) = mixed_folder("reuse", false) else { return eprintln!("skipped: no C++ compiler") };
    let cache_path = dir.join("cache.json");
    let opts = options(&dir, &cache_path);
    let first = scan::scan(&opts).unwrap();
    assert_every_bundle_listed(&dir, &first);
    assert_eq!(first.failures().count(), 3);
    // Nothing changed, so nothing is probed, failures included (they stay on the blocklist).
    let second = scan::scan(&opts).unwrap();
    assert_eq!(second.entries, first.entries, "every entry, load times included, came from the cache");
    // Retrying the failures probes the three bad bundles again and nothing else.
    let retried = scan::retry_failed(&opts).unwrap();
    assert_eq!(retried.failures().count(), 3);
    let good = |c: &ScanCache| c.plugins().cloned().collect::<Vec<_>>();
    assert_eq!(good(&retried), good(&first), "the good plugins were not probed again");
    // A rescan on demand probes everything.
    let third = scan::rescan(&opts).unwrap();
    assert_eq!(third.entries.len(), first.entries.len());
}

#[test]
fn a_plugin_that_timed_out_is_tried_a_few_more_times_then_blocklisted() {
    let dir = scratch_dir("scan-timeout-retry");
    let Some(_) = bad_plugin(&dir, "Hangs", Misbehaviour::HangsOnLoad) else { return eprintln!("skipped: no C++ compiler") };
    let opts = ScanOptions { timeout: Duration::from_secs(1), ..options(&dir, &dir.join("cache.json")) };
    // It may only have been slow (a licence check on a busy Mac): the next scans try again...
    // (Each scan that probes it counts one more timeout; one that reuses the entry does not.)
    for n in 1..=scan::TIMEOUT_RETRIES {
        let c = scan::scan(&opts).unwrap();
        let e = c.find("Hangs").unwrap();
        assert_eq!((e.status, e.timeouts), (ScanStatus::TimedOut, n), "scan {n} probed it");
    }
    // ...and after that it stays on the blocklist instead of costing every scan its timeout.
    let c = scan::scan(&opts).unwrap();
    assert_eq!(c.find("Hangs").unwrap().timeouts, scan::TIMEOUT_RETRIES, "reused, not probed");
    // Retrying failures on demand still tries it.
    let c = scan::retry_failed(&opts).unwrap();
    assert_eq!(c.find("Hangs").unwrap().timeouts, scan::TIMEOUT_RETRIES + 1);
}

#[test]
fn only_the_scanner_that_wrote_the_probe_list_has_its_probes_killed() {
    let dir = scratch_dir("probe-list");
    let cache = dir.join("cache.json");
    let spawn = || {
        let mut c = std::process::Command::new("sleep");
        c.arg("30");
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut c, 0);
        c.spawn().unwrap()
    };
    let list = scan::runner::running_probes_path(&cache);
    // A list left by a scanner that died: its ids may be other programs now.
    let mut bystander = spawn();
    std::fs::write(&list, format!("scanner 1\n{}\n", bystander.id())).unwrap();
    scan::runner::kill_listed_probes(&cache, 2);
    std::thread::sleep(Duration::from_millis(100));
    assert!(bystander.try_wait().unwrap().is_none(), "not ours: left alone");
    // The list of the scanner that was just killed: its probes go too.
    std::fs::write(&list, format!("scanner 2\n{}\n", bystander.id())).unwrap();
    scan::runner::kill_listed_probes(&cache, 2);
    let t = Instant::now();
    while bystander.try_wait().unwrap().is_none() && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(bystander.try_wait().unwrap().is_some(), "killed");
    assert!(!list.exists());
    let _ = bystander.kill();
}

#[test]
fn the_scan_keeps_to_its_overall_limit_and_leaves_no_probe_behind() {
    let Some(dir) = mixed_folder("deadline", true) else { return eprintln!("skipped: no C++ compiler") };
    let cache_path = dir.join("cache.json");
    // A plugin may take 120 s, but the whole scan only 4: the hang is cut by the overall limit.
    let opts = ScanOptions { total_timeout: Duration::from_secs(4), ..options(&dir, &cache_path) };
    let t = Instant::now();
    let cache = scan::scan(&opts).expect("the scanner stops itself and reports");
    assert!(t.elapsed() < Duration::from_secs(60), "took {:?}", t.elapsed());
    assert_every_bundle_listed(&dir, &cache);
    let hang = cache.find("Hangs").unwrap();
    assert_eq!(hang.status, ScanStatus::TimedOut);
    assert!(hang.error.as_deref().unwrap_or("").contains("overall time limit"), "{hang:?}");
    // What finished in time is kept with its real result. (On a badly overloaded machine a plugin
    // may itself miss the limit; it is then listed as timed out, never left out.)
    for name in ["Fixture Gain", "Fixture Synth", "Fixture Thru"] {
        if let Some(e) = cache.find(name) {
            assert_eq!(e.status, ScanStatus::Ok);
        } else {
            assert_eq!(cache.find("Fixture").map(|e| e.status), Some(ScanStatus::TimedOut));
        }
    }
    assert_eq!(ScanCache::read(&cache_path), Some(cache));
    assert!(!scan::runner::running_probes_path(&cache_path).exists(), "the running-probes list is cleaned up");
    // The hung probe was killed, not orphaned.
    std::thread::sleep(Duration::from_millis(200));
    let left = std::process::Command::new("pgrep").arg("-f").arg(dir.join("Hangs.vst3")).output();
    if let Ok(out) = left {
        assert!(out.stdout.is_empty(), "a probe is still running: {}", String::from_utf8_lossy(&out.stdout));
    }
}

#[test]
fn a_bundle_the_time_limit_never_reached_is_still_listed() {
    let Some(dir) = mixed_folder("never-reached", true) else { return eprintln!("skipped: no C++ compiler") };
    // One probe at a time, in name order: Aborts, Fixture, Hangs, Throws, ThrowsEarly. The hang
    // holds the only worker until the 3 s limit, so the last two never get a turn.
    let opts = ScanOptions { jobs: 1, total_timeout: Duration::from_secs(3), ..options(&dir, &dir.join("cache.json")) };
    let cache = scan::scan(&opts).unwrap();
    assert_every_bundle_listed(&dir, &cache);
    for name in ["Throws", "ThrowsEarly"] {
        let e = cache.find(name).unwrap_or_else(|| panic!("{name} is missing"));
        assert_eq!((e.status, e.timeouts), (ScanStatus::TimedOut, 0), "{e:?}");
        assert!(e.error.as_deref().unwrap_or("").contains("not probed"), "{e:?}");
    }
    // Next time, with room (and without the hang), they are probed.
    std::fs::remove_dir_all(dir.join("Hangs.vst3")).unwrap();
    let cache = scan::scan(&options(&dir, &dir.join("cache.json"))).unwrap();
    assert_every_bundle_listed(&dir, &cache);
    assert_eq!(cache.find("Bad Plugin").map(|e| e.status), Some(ScanStatus::Failed));
    assert_eq!(cache.find("ThrowsEarly").map(|e| e.status), Some(ScanStatus::Failed));
}

#[test]
fn a_missing_scanner_is_an_error_and_a_missing_folder_scans_empty() {
    let dir = scratch_dir("scan-empty");
    let mut opts = options(&dir, &dir.join("cache.json"));
    opts.search_paths = vec![dir.join("does-not-exist")];
    let cache = scan::scan(&opts).unwrap();
    assert!(cache.entries.is_empty());
    opts.scanner = Some(dir.join("no-such-program"));
    assert!(matches!(scan::scan(&opts), Err(scan::ScanError::Start(..))));
}
