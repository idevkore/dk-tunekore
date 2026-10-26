mod analyzer;
mod display;
mod midi;
mod recorder;
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
    // The user chose to drop the unsaved work, so don't offer it back next time.
    sampler::clear_autosave(&app);
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

/// The master level in dB (-60 = off, up to +6). Everything is limited after it.
#[tauri::command]
fn set_master(db: f32, state: Shared<'_>) {
    let gain = if db <= -60.0 { 0.0 } else { 10f32.powf(db.min(6.0) / 20.0) };
    audio(&state).lock().unwrap().engine.set_master(gain);
}

#[tauri::command]
fn master(state: Shared<'_>) -> f32 {
    let g = audio(&state).lock().unwrap().engine.master();
    if g <= 0.0 { -60.0 } else { 20.0 * g.log10() }
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
            // Gain reduction in dB while the master limiter works, so the UI can light up.
            let g = meter.take_limiting();
            if g < 0.99 {
                let _ = app.emit("limiter", 20.0 * g.max(1e-6).log10());
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let engine = Engine::new();
    let meter = engine.meter();
    let player = engine.player();
    let pads = engine.sampler();
    let midi_state = midi::state();
    // One engine for the life of the app.
    let audio: &'static Mutex<Audio> = Box::leak(Box::new(Mutex::new(Audio { engine, open_device: None })));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // Windows reopen at the size, place and display they were left at.
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(display::Zoom::default())
        .menu(display::menu)
        .on_menu_event(|app, event| display::on_menu(app, event.id().as_ref()))
        .manage(audio)
        .manage(player.clone())
        .manage(analyzer::Analyzer::default())
        .manage(midi_state.clone())
        .manage(recorder::RecorderState::default())
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
            let zoom = display::load(app.handle());
            if (zoom - 1.0).abs() > 0.001 {
                display::set(app.handle(), zoom);
            }
            spawn_meter(app.handle().clone(), meter);
            analyzer::spawn_position(app.handle().clone(), player);
            midi::spawn(app.handle().clone(), pads.clone(), midi_state);
            sampler::spawn_tick(app.handle().clone(), pads);
            sampler::spawn_autosave(app.handle().clone());
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
            set_master,
            master,
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
            analyzer::analyzer_tempo_key,
            display::display_zoom,
            display::set_display_zoom,
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
            sampler::sampler_export_stems,
            sampler::project_collect,
            sampler::sampler_chop,
            sampler::sampler_undo,
            sampler::sampler_pattern,
            sampler::sampler_velocity,
            sampler::sampler_step_extras,
            sampler::sampler_song,
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
            sampler::autosave_check,
            sampler::autosave_recover,
            sampler::autosave_discard,
            midi::midi_inputs,
            recorder::input_devices,
            recorder::sampling_start,
            recorder::sampling_stop,
            recorder::sampling_cancel,
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
