//! Sampling from an audio input onto a pad. `sampling_start` opens the input
//! and records; `sampling_stop` trims the silence off both ends, writes a
//! 24-bit WAV into Music/tuneKore/Samples/Recordings and puts it on the pad.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::async_runtime::spawn_blocking;
use tauri::{AppHandle, Emitter, Manager, State};
use tunekore_analyzer::export_wav;
use tunekore_engine::{InputDevice, Recorder, PADS};

/// Takes this far below full scale or quieter count as silence at the ends.
const SILENCE_DB: f32 = -50.0;
/// Audio kept before the first sound.
const PRE_ROLL_MS: usize = 10;
const LEVEL_EVERY: Duration = Duration::from_millis(33);

struct Active {
    recorder: Recorder,
    pad: usize,
    /// Cleared when the take ends, to stop its level thread.
    live: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct RecorderState {
    active: Mutex<Option<Active>>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Level {
    /// 0..1 (a little over 1 when the input clips).
    peak: f32,
    seconds: f32,
    /// The take reached its 10-minute cap and ignores more.
    full: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sampled {
    pad: usize,
    path: String,
    seconds: f32,
}

/// Drops the silence at both ends of a take. Silence is quieter than
/// -50 dBFS in every channel; 10 ms before the first sound is kept. `None`
/// when the whole take is silent.
pub fn trim_silence(channels: &[Vec<f32>], rate: u32) -> Option<Vec<Vec<f32>>> {
    let threshold = 10f32.powf(SILENCE_DB / 20.0);
    let frames = channels.iter().map(Vec::len).min()?;
    let loud = |i: usize| channels.iter().any(|c| c[i].abs() >= threshold);
    let first = (0..frames).find(|&i| loud(i))?;
    let last = (first..frames).rev().find(|&i| loud(i))?;
    let start = first.saturating_sub(rate as usize * PRE_ROLL_MS / 1000);
    Some(channels.iter().map(|c| c[start..=last].to_vec()).collect())
}

/// The first `Recording N.wav` in `dir` that doesn't exist yet.
fn next_name(dir: &Path) -> PathBuf {
    (1..)
        .map(|n| dir.join(format!("Recording {n}.wav")))
        .find(|p| !p.exists())
        .expect("an unused name")
}

#[tauri::command]
pub async fn input_devices() -> Result<Vec<InputDevice>, String> {
    spawn_blocking(tunekore_engine::list_inputs).await.map_err(|e| e.to_string())?
}

/// Starts recording from an input (`None` = the system default) for a pad.
#[tauri::command]
pub async fn sampling_start(pad: usize, device_id: Option<String>, app: AppHandle) -> Result<(), String> {
    if pad >= PADS {
        return Err(format!("there is no pad {}", pad + 1));
    }
    if app.state::<RecorderState>().active.lock().unwrap().is_some() {
        return Err("already sampling: stop or cancel it first".into());
    }
    // Opening the stream is what asks macOS for the microphone: not on the UI thread.
    let recorder = spawn_blocking(move || Recorder::start(device_id.as_deref())).await.map_err(|e| e.to_string())??;
    let meter = recorder.meter();
    let rate = recorder.sample_rate();
    let live = Arc::new(AtomicBool::new(true));
    {
        let state = app.state::<RecorderState>();
        let mut active = state.active.lock().unwrap();
        if active.is_some() {
            return Err("already sampling: stop or cancel it first".into());
        }
        *active = Some(Active { recorder, pad, live: live.clone() });
    }
    // The meter: peak, time and any trouble, about 30 times a second.
    std::thread::spawn(move || {
        let mut reported = false;
        while live.load(Ordering::Relaxed) {
            std::thread::sleep(LEVEL_EVERY);
            let _ = app.emit("input:level", Level { peak: meter.take_peak(), seconds: meter.seconds(rate), full: meter.is_full() });
            if !reported {
                let trouble = app.state::<RecorderState>().active.lock().unwrap().as_mut().and_then(|a| a.recorder.trouble());
                if let Some(msg) = trouble {
                    reported = true;
                    let _ = app.emit("input:error", msg);
                }
            }
        }
    });
    Ok(())
}

/// Stops, trims, writes the take and puts it on the pad it was started for.
#[tauri::command]
pub async fn sampling_stop(app: AppHandle) -> Result<Sampled, String> {
    let active = app.state::<RecorderState>().active.lock().unwrap().take().ok_or("not sampling")?;
    active.live.store(false, Ordering::Relaxed);
    let pad = active.pad;
    let dir = app.path().audio_dir().map_err(|e| e.to_string())?.join("tuneKore/Samples/Recordings");
    let a = app.clone();
    let result = spawn_blocking(move || -> Result<Sampled, String> {
        let (rate, mut channels) = active.recorder.stop()?;
        // The Sampler plays mono or stereo; an interface with more inputs gives its first two.
        channels.truncate(2);
        let Some(trimmed) = trim_silence(&channels, rate) else {
            return Err("only silence came in. Check the input's level, and that tuneKore may use the microphone (System Settings > Privacy & Security > Microphone).".into());
        };
        std::fs::create_dir_all(&dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
        let path = next_name(&dir);
        export_wav(&trimmed, rate, &path)?;
        crate::sampler::load_pad(&a, pad, &path, Default::default())?;
        Ok(Sampled { pad, path: path.display().to_string(), seconds: trimmed[0].len() as f32 / rate as f32 })
    })
    .await
    .map_err(|e| e.to_string())?;
    crate::sampler::publish_now(&app);
    if let Ok(s) = &result {
        if let Some(folder) = Path::new(&s.path).parent() {
            let _ = app.emit("sampler:chopped", folder.display().to_string());
        }
    }
    result
}

/// Throws the take away.
#[tauri::command]
pub async fn sampling_cancel(state: State<'_, RecorderState>) -> Result<(), String> {
    let active = state.active.lock().unwrap().take();
    if let Some(a) = active {
        a.live.store(false, Ordering::Relaxed);
        spawn_blocking(move || a.recorder.cancel()).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The sampling (audio input) commands.
pub fn commands() -> crate::Commands {
    crate::commands![input_devices, sampling_start, sampling_stop, sampling_cancel]
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn tone(frames: usize, amp: f32) -> Vec<f32> {
        vec![amp; frames]
    }

    #[test]
    fn trims_silence_at_both_ends_and_keeps_10_ms_of_pre_roll() {
        let mut ch = vec![0.0; 48_000]; // a second of nothing
        ch.extend(tone(1000, 0.5));
        ch.extend(vec![0.0; 24_000]);
        let out = trim_silence(&[ch], RATE).unwrap();
        assert_eq!(out[0].len(), 480 + 1000);
        assert!(out[0][..480].iter().all(|&s| s == 0.0));
        assert_eq!(out[0][480], 0.5);
        assert_eq!(*out[0].last().unwrap(), 0.5);
    }

    #[test]
    fn quiet_noise_below_minus_50_db_counts_as_silence() {
        let hiss = 0.002; // about -54 dBFS
        let mut ch = vec![hiss; 5000];
        ch.extend(tone(100, 0.3));
        ch.extend(vec![-hiss; 5000]);
        let out = trim_silence(&[ch], RATE).unwrap();
        assert_eq!(out[0].len(), 480 + 100);
    }

    #[test]
    fn pre_roll_cannot_go_before_the_start() {
        let mut ch = vec![0.0; 100];
        ch.extend(tone(10, 0.4));
        let out = trim_silence(&[ch], RATE).unwrap();
        assert_eq!(out[0].len(), 110);
    }

    #[test]
    fn a_sound_in_either_channel_keeps_both_channels_in_step() {
        let mut left = vec![0.0; 2000];
        let mut right = vec![0.0; 2000];
        left[1500] = 0.8; // only the left channel has sound early on
        right[1800] = 0.8; // and only the right has it late
        let out = trim_silence(&[left, right], RATE).unwrap();
        assert_eq!(out[0].len(), out[1].len());
        assert_eq!(out[0].len(), 1800 - 1020 + 1);
    }

    #[test]
    fn a_silent_take_is_none() {
        assert!(trim_silence(&[vec![0.0; 1000]], RATE).is_none());
        assert!(trim_silence(&[], RATE).is_none());
        assert!(trim_silence(&[vec![]], RATE).is_none());
    }

    #[test]
    fn names_count_up_from_the_first_unused() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(next_name(dir.path()), dir.path().join("Recording 1.wav"));
        std::fs::write(dir.path().join("Recording 1.wav"), b"").unwrap();
        std::fs::write(dir.path().join("Recording 3.wav"), b"").unwrap();
        assert_eq!(next_name(dir.path()), dir.path().join("Recording 2.wav"));
    }
}
