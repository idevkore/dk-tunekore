mod engine;

use engine::{Engine, OutputDevice};
use std::sync::Arc;
use std::time::Duration;
use tauri::{async_runtime::spawn_blocking, Emitter, Manager, State};

type Shared<'a> = State<'a, Arc<Engine>>;

#[tauri::command]
fn engine_version() -> String {
    Engine::version()
}

#[tauri::command]
async fn list_outputs(engine: Shared<'_>) -> Result<Vec<OutputDevice>, String> {
    let engine = engine.inner().clone();
    spawn_blocking(move || engine.list_outputs()).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn start_tone(
    device_id: Option<u32>,
    frequency: f32,
    engine: Shared<'_>,
) -> Result<(), String> {
    let engine = engine.inner().clone();
    spawn_blocking(move || engine.start_tone(device_id, frequency))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn stop_tone(engine: Shared<'_>) -> Result<(), String> {
    let engine = engine.inner().clone();
    spawn_blocking(move || engine.stop_tone()).await.map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let engine = Arc::new(Engine::new().expect("audio engine failed to start"));

    tauri::Builder::default()
        .manage(engine.clone())
        .setup(move |app| {
            // Meter feed: about 30 Hz while there is sound, then one 0 when it stops.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let mut was_active = false;
                loop {
                    std::thread::sleep(Duration::from_millis(33));
                    let peak = engine.take_peak();
                    if peak > 0.0 || was_active {
                        let _ = handle.emit("meter", peak);
                    }
                    was_active = peak > 0.0;
                }
            });
            let _ = app.get_webview_window("main");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            engine_version,
            list_outputs,
            start_tone,
            stop_tone
        ])
        .run(tauri::generate_context!())
        .expect("error while running musiKore");
}
