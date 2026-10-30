//! The scanner: probes each plugin in a child process of its own, so crashes, hangs and
//! exceptions end up in the cache as errors and the scan carries on.

mod common;

use common::{bad_plugin, fixture_bundle, scratch_dir, Misbehaviour};
use std::time::{Duration, Instant};
use tunekore_plugins::scan::{self, ScanCache, ScanOptions, ScanStatus};
use tunekore_plugins::vst3;
use tunekore_plugins::{Category, PluginError};

fn options(dir: &std::path::Path, cache: &std::path::Path) -> ScanOptions {
    ScanOptions {
        cache_path: cache.to_path_buf(),
        search_paths: vec![dir.to_path_buf()],
        default_paths: false,
        timeout: Duration::from_secs(3),
        jobs: 4,
        force: false,
        retry_failed: false,
        scanner: Some(env!("CARGO_BIN_EXE_tunekore-scan").into()),
        total_timeout: Duration::from_secs(120),
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
fn the_scan_survives_a_crash_a_hang_and_exceptions_and_reports_each() {
    let Some(dir) = mixed_folder("survives", true) else { return eprintln!("skipped: no C++ compiler") };
    let cache_path = dir.join("cache.json");
    let t = Instant::now();
    let cache = scan::scan(&options(&dir, &cache_path)).expect("the scan runs");
    assert!(t.elapsed() < Duration::from_secs(30), "the hang is cut off by the timeout");

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

    let err = |name: &str| cache.find(name).unwrap_or_else(|| panic!("no entry for {name}")).error.clone().unwrap_or_else(|| panic!("{name} should have failed"));
    let status = |name: &str| cache.find(name).unwrap().status;
    assert_eq!(status("Fixture Synth"), ScanStatus::Ok);
    assert_eq!(status("Bad Plugin"), ScanStatus::Failed, "threw an exception: caught, a plain failure");
    assert_eq!(status("ThrowsEarly"), ScanStatus::Failed);
    assert_eq!(status("Aborts"), ScanStatus::Crashed);
    assert_eq!(status("Hangs"), ScanStatus::TimedOut);
    let e = err("Bad Plugin");
    // Two bundles produce a plugin called Bad Plugin (initialize throws); the others never get that far.
    assert!(e.contains("boom from initialize"), "{e}");
    assert!(err("Aborts").contains("crashed while loading (SIGABRT"), "{}", err("Aborts"));
    assert!(err("Hangs").contains("timed out"), "{}", err("Hangs"));
    assert!(err("ThrowsEarly").contains("boom from GetPluginFactory"), "{}", err("ThrowsEarly"));
    assert_eq!(cache.failures().count(), 4);

    // The file on disk is what was returned.
    assert_eq!(ScanCache::read(&cache_path), Some(cache));
}

#[test]
fn a_second_scan_reuses_the_cache_and_a_rescan_does_not() {
    let Some(dir) = mixed_folder("reuse", false) else { return eprintln!("skipped: no C++ compiler") };
    let cache_path = dir.join("cache.json");
    let opts = options(&dir, &cache_path);
    let first = scan::scan(&opts).unwrap();
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
fn a_plugin_that_timed_out_is_tried_again_every_scan() {
    let dir = scratch_dir("scan-timeout-retry");
    let Some(_) = bad_plugin(&dir, "Hangs", Misbehaviour::HangsOnLoad) else { return eprintln!("skipped: no C++ compiler") };
    let opts = options(&dir, &dir.join("cache.json"));
    let first = scan::scan(&opts).unwrap();
    assert_eq!(first.find("Hangs").unwrap().status, ScanStatus::TimedOut);
    // It may only have been slow (a licence check on a busy Mac): the next scan tries again.
    let t = Instant::now();
    let second = scan::scan(&opts).unwrap();
    assert!(t.elapsed() >= Duration::from_secs(3), "probed again: {:?}", t.elapsed());
    assert_eq!(second.find("Hangs").unwrap().status, ScanStatus::TimedOut);
}

#[test]
fn the_scan_keeps_to_its_overall_limit_and_leaves_no_probe_behind() {
    let Some(dir) = mixed_folder("deadline", true) else { return eprintln!("skipped: no C++ compiler") };
    let cache_path = dir.join("cache.json");
    // Each plugin may take 30 s, but the whole scan only 3.
    let opts = ScanOptions { timeout: Duration::from_secs(30), total_timeout: Duration::from_secs(3), ..options(&dir, &cache_path) };
    let t = Instant::now();
    let cache = scan::scan(&opts).expect("the scanner stops itself and reports");
    assert!(t.elapsed() < Duration::from_secs(15), "took {:?}", t.elapsed());
    let hang = cache.find("Hangs").unwrap();
    assert_eq!(hang.status, ScanStatus::TimedOut);
    assert!(hang.error.as_deref().unwrap_or("").contains("overall time limit"), "{hang:?}");
    assert_eq!(cache.plugins().count(), 3, "what finished in time is kept");
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
fn a_missing_scanner_is_an_error_and_a_missing_folder_scans_empty() {
    let dir = scratch_dir("scan-empty");
    let mut opts = options(&dir, &dir.join("cache.json"));
    opts.search_paths = vec![dir.join("does-not-exist")];
    let cache = scan::scan(&opts).unwrap();
    assert!(cache.entries.is_empty());
    opts.scanner = Some(dir.join("no-such-program"));
    assert!(matches!(scan::scan(&opts), Err(scan::ScanError::Start(..))));
}
