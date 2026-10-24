//! The app → worker → stems path, with the worker's model-free test backend.
//! Skips when the stem worker isn't set up (as on CI's build jobs).

use musikore_analyzer::{check, separate, Cancel, JobError, Song, WorkerConfig};

fn config() -> Option<WorkerConfig> {
    std::env::set_var("MUSIKORE_STEMS_TEST", "1");
    match WorkerConfig::locate() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("skipping: {e}");
            None
        }
    }
}

fn song() -> Song {
    let l: Vec<f32> = (0..96_000).map(|i| (i as f32 * 0.013).sin() * 0.4).collect();
    let r: Vec<f32> = (0..96_000).map(|i| (i as f32 * 0.021).sin() * 0.3).collect();
    Song { sample_rate: 44_100, channels: std::sync::Arc::new(vec![l, r]), codec: "PCM".into(), bits: None }
}

#[test]
fn separates_through_the_worker_and_passes_the_checks() {
    let Some(cfg) = config() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut stages = Vec::new();
    let (stems, done) = separate(&cfg, &song(), "test-split", "fast", dir.path(), &Cancel::default(), |s, f| {
        stages.push((s.to_string(), f))
    })
    .unwrap();
    assert!(stages.iter().any(|(s, _)| s == "vocals"));
    assert_eq!(done.stems.len(), 4);
    let c = check(&song(), &stems);
    assert!(c.same_length);
    assert_eq!(c.lag_frames, 0);
    assert!(c.sum_snr_db > 100.0, "{}", c.sum_snr_db);
}

#[test]
fn cancel_before_start_cancels() {
    let Some(cfg) = config() else { return };
    let dir = tempfile::tempdir().unwrap();
    let cancel = Cancel::default();
    cancel.cancel();
    let r = separate(&cfg, &song(), "test-split", "fast", dir.path(), &cancel, |_, _| {});
    assert_eq!(r.err(), Some(JobError::Cancelled));
}

#[test]
fn worker_errors_come_back_as_messages() {
    let Some(cfg) = config() else { return };
    let dir = tempfile::tempdir().unwrap();
    let r = separate(&cfg, &song(), "no-such-model", "fast", dir.path(), &Cancel::default(), |_, _| {});
    match r {
        Err(JobError::Failed(m)) => assert!(m.contains("no-such-model"), "{m}"),
        other => panic!("{other:?}"),
    }
}
