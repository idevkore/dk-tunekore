mod analyzer;
mod sampler;

use tunekore_engine::{Engine, Meter, OutputDevice};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Set once the user has said to quit without saving.
static QUIT_CONFIRMED: AtomicBool = AtomicBool::new(false);

/// Whether quitting or closing the main window must ask first.
fn must_ask(app: &tauri::AppHandle) -> bool {
    !QUIT_CONFIRMED.load(Ordering::Relaxed) && app.state::<sampler::SamplerState>().is_dirty()
}

/// Quits, after the UI has asked about unsaved changes.
#[tauri::command]
fn app_quit(app: tauri::AppHandle) {
    QUIT_CONFIRMED.store(true, Ordering::Relaxed);
    app.exit(0);
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

/// Stops the tone. The output stays open for the Sampler and the Analyzer.
#[tauri::command]
fn stop_tone(state: Shared<'_>) {
    audio(&state).lock().unwrap().engine.set_tone(false, 440.0, 0.0);
}

/// The output device in use (`None` = the system default), or `None` when closed.
#[tauri::command]
fn current_output(state: Shared<'_>) -> Option<Option<String>> {
    audio(&state).lock().unwrap().open_device.clone()
}

/// Moves the output to another device (`None` = the system default).
#[tauri::command]
async fn set_output(device_id: Option<String>, state: Shared<'_>) -> Result<(), String> {
    let audio = audio(&state);
    spawn_blocking(move || {
        let mut a = audio.lock().unwrap();
        if a.open_device.as_ref() == Some(&device_id) {
            return Ok(());
        }
        if a.open_device.is_some() {
            a.fade_out_and_close();
        }
        a.engine.open_output(device_id.as_deref())?;
        a.open_device = Some(device_id);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
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
    let pads = engine.sampler();
    // One engine for the life of the app.
    let audio: &'static Mutex<Audio> = Box::leak(Box::new(Mutex::new(Audio { engine, open_device: None })));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(audio)
        .manage(player.clone())
        .manage(analyzer::Analyzer::default())
        .manage(sampler::SamplerState::new(pads.clone()))
        .on_window_event(|window, event| {
            // Closing the main window with unsaved changes asks first (the UI shows the question).
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" && must_ask(window.app_handle()) {
                    api.prevent_close();
                    let _ = window.emit("app:confirm-quit", ());
                } else if window.label() == "main" {
                    window.app_handle().exit(0);
                }
            }
        })
        .setup(move |app| {
            spawn_meter(app.handle().clone(), meter);
            analyzer::spawn_position(app.handle().clone(), player);
            sampler::spawn_tick(app.handle().clone(), pads);
            // Open the default output now, so the pads play on the first click.
            std::thread::spawn(move || {
                if let Err(e) = audio.lock().unwrap().ensure_open() {
                    eprintln!("no audio output yet: {e}");
                }
            });
            let _ = app.get_webview_window("main");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            engine_version,
            app_quit,
            list_outputs,
            start_tone,
            stop_tone,
            current_output,
            set_output,
            analyzer::open_analyzer,
            analyzer::analyzer_backends,
            analyzer::analyzer_open,
            analyzer::analyzer_separate,
            analyzer::analyzer_cancel,
            analyzer::analyzer_play,
            analyzer::analyzer_seek,
            analyzer::analyzer_track,
            analyzer::analyzer_export,
            analyzer::analyzer_to_pads,
            sampler::sampler_state,
            sampler::sampler_load,
            sampler::sampler_clear,
            sampler::sampler_move,
            sampler::sampler_settings,
            sampler::sampler_step,
            sampler::sampler_clear_steps,
            sampler::sampler_transport,
            sampler::sampler_play,
            sampler::sampler_trigger,
            sampler::sampler_release,
            sampler::sampler_export,
            sampler::sampler_bounce,
            sampler::sampler_chop,
            sampler::sampler_undo,
            sampler::sampler_pattern,
            sampler::sampler_velocity,
            sampler::sampler_metronome,
            sampler::sampler_record,
            sampler::sampler_copy_pattern,
            sampler::sampler_redo,
            sampler::library_scan,
            sampler::library_default,
            sampler::library_preview,
            sampler::library_stop,
            sampler::project_new,
            sampler::project_open,
            sampler::project_save,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tuneKore")
        .run(|app, event| {
            match event {
                // Quit (⌘Q) with unsaved changes asks first, in the main window.
                tauri::RunEvent::ExitRequested { api, code: None, .. } if must_ask(app) => {
                    api.prevent_exit();
                    let _ = app.emit_to("main", "app:confirm-quit", ());
                }
                // Don't leave a stem worker running after the app quits.
                tauri::RunEvent::Exit => app.state::<analyzer::Analyzer>().cancel_job(),
                _ => {}
            }
        });
}
