//! Runs the Analyzer on a song from the command line and prints the Phase 1
//! checks as JSON. Used for the model comparison in docs/PLAN.md section 6.
//!
//!     cargo run --release -p tunekore-analyzer --example analyze -- \
//!         song.mp3 --backend demucs --mode fast --out ~/stems/song
//!
//! With --out it also writes the four stems as WAV files there.

use std::path::PathBuf;
use std::time::Instant;

use tunekore_analyzer::{check, decode_file, export_wav, separate, Cancel, WorkerConfig};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut song_path = None;
    let (mut backend, mut mode, mut out) = ("demucs".to_string(), "fast".to_string(), None::<PathBuf>);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--backend" => backend = args.next().expect("--backend needs a value"),
            "--mode" => mode = args.next().expect("--mode needs a value"),
            "--out" => out = Some(args.next().expect("--out needs a value").into()),
            _ => song_path = Some(PathBuf::from(a)),
        }
    }
    let song_path = song_path.expect("usage: analyze <song> [--backend demucs|sam-audio] [--mode fast|best] [--out dir]");

    let t0 = Instant::now();
    let song = decode_file(&song_path).unwrap_or_else(|e| fail(&e));
    let decode_s = t0.elapsed().as_secs_f64();

    let cfg = WorkerConfig::locate().unwrap_or_else(|e| fail(&e));
    let job = tempdir();
    let t1 = Instant::now();
    let mut last = String::new();
    let (stems, done) = separate(&cfg, &song, &backend, &mode, &job, &Cancel::default(), |stage, f| {
        if stage != last {
            eprintln!("  {stage} ({:.0}%)", f * 100.0);
            last = stage.to_string();
        }
    })
    .unwrap_or_else(|e| fail(&format!("{e} (log: {})", job.join("worker.log").display())));
    let separate_s = t1.elapsed().as_secs_f64();
    let checks = check(&song, &stems);

    if let Some(dir) = &out {
        std::fs::create_dir_all(dir).unwrap();
        for (name, audio) in &stems.stems {
            export_wav(audio.as_slice(), stems.sample_rate, &dir.join(format!("{name}.wav"))).unwrap_or_else(|e| fail(&e));
        }
    }
    let _ = std::fs::remove_dir_all(&job);
    let report = serde_json::json!({
        "song": song_path.file_name().map(|n| n.to_string_lossy().to_string()),
        "seconds": song.seconds(),
        "sampleRate": song.sample_rate,
        "channels": song.channels.len(),
        "backend": backend,
        "mode": mode,
        "decodeSeconds": decode_s,
        "separateSeconds": separate_s,
        "workerSeconds": done.seconds,
        "peakRssMb": done.peak_rss_mb,
        "peakGpuMb": done.peak_gpu_mb,
        "checks": checks,
        "out": out,
    });
    println!("{report}");
}

fn tempdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("tunekore-analyze-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn fail(msg: &str) -> ! {
    eprintln!("analyze: {msg}");
    std::process::exit(1)
}
