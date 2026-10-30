//! The Tauri shell: windows, menus and files around the engine.
//!
//! Each feature module brings its own Tauri commands as a [`Commands`] list,
//! made with [`commands!`] in its own file, its own `setup`, and its own part
//! of the project file as a [`project::Section`] it registers from that
//! setup. Adding a module here takes three lines, each in an alphabetical
//! list of one line per module: its `mod`, its `commands()` and its `setup`.

/// A module's Tauri commands, for [`run`]'s router: `commands![a, b]` lists
/// commands defined in the module where it is written, and
/// `commands!(module => [a, b])` lists another module's.
macro_rules! commands {
    ($($name:ident),* $(,)?) => {
        $crate::Commands::new(&[$(stringify!($name)),*], Box::new(tauri::generate_handler![$($name),*]))
    };
    ($module:ident => [$($name:ident),* $(,)?]) => {
        $crate::Commands::new(&[$(stringify!($name)),*], Box::new(tauri::generate_handler![$($module::$name),*]))
    };
}
pub(crate) use commands;

mod analyzer;
mod display;
mod midi;
mod project;
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

type Handler = Box<dyn Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync>;

/// One module's commands: their names, and the handler that runs them.
pub(crate) struct Commands {
    names: &'static [&'static str],
    handler: Handler,
}

impl Commands {
    pub(crate) fn new(names: &'static [&'static str], handler: Handler) -> Self {
        Self { names, handler }
    }
}

/// Sends each call to the module whose list names it.
fn router(modules: Vec<Commands>) -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    let mut seen = std::collections::HashSet::new();
    for name in modules.iter().flat_map(|m| m.names) {
        assert!(seen.insert(*name), "two modules have a command called {name}");
    }
    move |invoke| {
        let name = invoke.message.command();
        match modules.iter().position(|m| m.names.contains(&name)) {
            Some(i) => (modules[i].handler)(invoke),
            None => false,
        }
    }
}

/// The engine plus the device its stream is open on (`Some(None)` = system default).
pub(crate) struct Audio {
    pub(crate) engine: Engine,
    open_device: Option<Option<String>>,
}

impl Audio {
    pub(crate) fn new(engine: Engine) -> Self {
        Self { engine, open_device: None }
    }

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

fn audio_state(state: &Shared<'_>) -> &'static Mutex<Audio> {
    *state.inner()
}

/// The engine, from anywhere with the app.
pub(crate) fn audio(app: &tauri::AppHandle) -> &'static Mutex<Audio> {
    *app.state::<&'static Mutex<Audio>>().inner()
}

/// Set once the user has said to quit without saving.
static QUIT_CONFIRMED: AtomicBool = AtomicBool::new(false);

/// Whether quitting or closing the main window must ask first.
fn must_ask(app: &tauri::AppHandle) -> bool {
    !QUIT_CONFIRMED.load(Ordering::Relaxed) && project::is_dirty(app)
}

/// Quits, after the UI has asked about unsaved changes.
#[tauri::command]
fn app_quit(app: tauri::AppHandle) {
    QUIT_CONFIRMED.store(true, Ordering::Relaxed);
    // The user chose to drop the unsaved work, so don't offer it back next time.
    project::clear_autosave(&app);
    app.exit(0);
}

#[tauri::command]
fn engine_version() -> &'static str {
    tunekore_engine::VERSION
}

#[tauri::command]
async fn list_outputs(state: Shared<'_>) -> Result<Vec<OutputDevice>, String> {
    let audio = audio_state(&state);
    spawn_blocking(move || audio.lock().unwrap().engine.list_outputs())
        .await
        .map_err(|e| e.to_string())?
}

/// Starts the tone, opening the device first if it isn't open already. Called
/// again with a new frequency, it retunes the running tone.
#[tauri::command]
async fn start_tone(device_id: Option<String>, frequency: f32, state: Shared<'_>) -> Result<(), String> {
    let audio = audio_state(&state);
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
    audio_state(&state).lock().unwrap().engine.set_tone(false, 440.0, 0.0);
}

/// The master level in dB (-60 = off, up to +6). Everything is limited after it.
#[tauri::command]
fn set_master(db: f32, state: Shared<'_>) {
    let gain = if db <= -60.0 { 0.0 } else { 10f32.powf(db.min(6.0) / 20.0) };
    audio_state(&state).lock().unwrap().engine.set_master(gain);
}

#[tauri::command]
fn master(state: Shared<'_>) -> f32 {
    let g = audio_state(&state).lock().unwrap().engine.master();
    if g <= 0.0 { -60.0 } else { 20.0 * g.log10() }
}

/// The output device in use (`None` = the system default), or `None` when closed.
#[tauri::command]
fn current_output(state: Shared<'_>) -> Option<Option<String>> {
    audio_state(&state).lock().unwrap().open_device.clone()
}

/// Moves the output to another device (`None` = the system default).
#[tauri::command]
async fn set_output(device_id: Option<String>, state: Shared<'_>) -> Result<(), String> {
    let audio = audio_state(&state);
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
            // The output stream's errors, counted on the audio thread, logged here.
            let errors = meter.take_stream_errors();
            if errors > 0 {
                eprintln!("audio output: {errors} stream error(s), such as a buffer underrun");
            }
        }
    });
}

/// The shell's own commands: the engine, the output device, the tone, quitting.
fn shell_commands() -> Commands {
    commands![engine_version, app_quit, list_outputs, start_tone, stop_tone, current_output, set_master, master, set_output]
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let engine = Engine::new();
    let meter = engine.meter();
    let player = engine.player();
    let pads = engine.sampler();
    // One engine for the life of the app.
    let audio: &'static Mutex<Audio> = Box::leak(Box::new(Mutex::new(Audio::new(engine))));

    // Every module's commands, one line each, in alphabetical order.
    let commands = vec![
        analyzer::commands(),
        display::commands(),
        midi::commands(),
        project::commands(),
        recorder::commands(),
        sampler::commands(),
        shell_commands(),
    ];

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // Windows reopen at the size, place and display they were left at.
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .menu(display::menu)
        .on_menu_event(|app, event| display::on_menu(app, event.id().as_ref()))
        .manage(analyzer::Analyzer::default())
        .manage(audio)
        .manage(display::Zoom::default())
        .manage(midi::state())
        .manage(pads.clone())
        .manage(player)
        .manage(project::Project::<tauri::AppHandle>::new())
        .manage(recorder::RecorderState::default())
        .manage(sampler::SamplerState::new(pads))
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
            // Each module's setup, one line each, in alphabetical order.
            analyzer::setup(app.handle());
            display::setup(app.handle());
            midi::setup(app.handle());
            project::setup(app.handle());
            sampler::setup(app.handle());
            // Last: every module has registered its part of the project.
            project::ready(app.handle());
            // Open the default output now, so the pads play on the first click.
            std::thread::spawn(move || {
                if let Err(e) = audio.lock().unwrap().ensure_open() {
                    eprintln!("no audio output yet: {e}");
                }
            });
            let _ = app.get_webview_window("main");
            Ok(())
        })
        .invoke_handler(router(commands))
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
