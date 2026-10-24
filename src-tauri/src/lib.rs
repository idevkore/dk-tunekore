mod analyzer;

use tunekore_engine::{Engine, Meter, OutputDevice};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{async_runtime::spawn_blocking, Emitter, Manager, State};

/// Test tone level. Quiet on purpose.
const TONE_GAIN: f32 = 0.2;
/// Longer than the engine's 10 ms gain ramp, so the stream closes on silence.
const RAMP_WAIT: Duration = Duration::from_millis(40);

/// The engine plus the device its stream is open on (`Some(None)` = system default).
pub(crate) struct Audio {
    pub(crate) engine: Engine,
    open_device: Option<Option<String>>,
}

impl Audio {
    fn fade_out_and_close(&mut self) {
        self.engine.set_tone(false, 440.0, 0.0);
        self.engine.player().set_playing(false);
        std::thread::sleep(RAMP_WAIT);
        self.engine.close_output();
        self.open_device = None;
    }

    /// Opens the system default output unless an output is already open.
    pub(crate) fn ensure_open(&mut self) -> Result<(), String> {
        if self.open_device.is_none() {
            self.engine.open_output(None)?;
            self.open_device = Some(None);
        }
        Ok(())
    }
}

type Shared<'a> = State<'a, &'static Mutex<Audio>>;

fn audio(state: &Shared<'_>) -> &'static Mutex<Audio> {
    *state.inner()
}

#[tauri::command]
fn engine_version() -> &'static str {
    tunekore_engine::VERSION
}

#[tauri::command]
async fn list_outputs(state: Shared<'_>) -> Result<Vec<OutputDevice>, String> {
    let audio = audio(&state);
    spawn_blocking(move || audio.lock().unwrap().engine.list_outputs())
        .await
        .map_err(|e| e.to_string())?
}

/// Starts the tone, opening the device first if it isn't open already. Called
/// again with a new frequency, it retunes the running tone.
#[tauri::command]
async fn start_tone(device_id: Option<String>, frequency: f32, state: Shared<'_>) -> Result<(), String> {
    let audio = audio(&state);
    spawn_blocking(move || {
        let mut a = audio.lock().unwrap();
        if a.open_device.as_ref() != Some(&device_id) {
            if a.open_device.is_some() {
                a.fade_out_and_close();
            }
            a.engine.open_output(device_id.as_deref())?;
            a.open_device = Some(device_id);
        }
        a.engine.set_tone(true, frequency, TONE_GAIN);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Stops the tone. Closes the output too, unless the Analyzer is playing on it.
#[tauri::command]
async fn stop_tone(state: Shared<'_>) -> Result<(), String> {
    let audio = audio(&state);
    spawn_blocking(move || {
        let mut a = audio.lock().unwrap();
        if a.engine.player().is_playing() {
            a.engine.set_tone(false, 440.0, 0.0);
        } else if a.open_device.is_some() {
            a.fade_out_and_close();
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// Sends the output peak to the UI about 30 times a second while there is
/// sound, then one 0 when it stops.
fn spawn_meter(app: tauri::AppHandle, meter: Meter) {
    std::thread::spawn(move || {
        let mut was_active = false;
        loop {
            std::thread::sleep(Duration::from_millis(33));
            let peak = meter.take_peak();
            if peak > 0.0 || was_active {
                let _ = app.emit("meter", peak);
            }
            was_active = peak > 0.0;
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let engine = Engine::new();
    let meter = engine.meter();
    let player = engine.player();
    // One engine for the life of the app.
    let audio: &'static Mutex<Audio> = Box::leak(Box::new(Mutex::new(Audio { engine, open_device: None })));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(audio)
        .manage(player.clone())
        .manage(analyzer::Analyzer::default())
        .setup(move |app| {
            spawn_meter(app.handle().clone(), meter);
            analyzer::spawn_position(app.handle().clone(), player);
            let _ = app.get_webview_window("main");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            engine_version,
            list_outputs,
            start_tone,
            stop_tone,
            analyzer::open_analyzer,
            analyzer::analyzer_backends,
            analyzer::analyzer_open,
            analyzer::analyzer_separate,
            analyzer::analyzer_cancel,
            analyzer::analyzer_play,
            analyzer::analyzer_seek,
            analyzer::analyzer_track,
            analyzer::analyzer_export,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tuneKore")
        .run(|app, event| {
            // Don't leave a stem worker running after the app quits.
            if let tauri::RunEvent::Exit = event {
                app.state::<analyzer::Analyzer>().cancel_job();
            }
        });
}
