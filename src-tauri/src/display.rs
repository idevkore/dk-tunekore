//! Display zoom: the whole interface at 80–150%, for big or high-density
//! screens. View menu items (⌘+, ⌘−, ⌘0) and the Audio dialog set it; it
//! applies to every window and is saved in the app's config folder.

use std::sync::Mutex;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Manager, Runtime};

/// The zoom levels the menu steps through.
pub const LEVELS: [f64; 7] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.35, 1.5];
const MIN: f64 = 0.8;
const MAX: f64 = 1.5;
const FILE: &str = "display.json";

/// The current zoom, 1.0 = 100%.
pub struct Zoom(pub Mutex<f64>);

impl Default for Zoom {
    fn default() -> Self {
        Self(Mutex::new(1.0))
    }
}

/// The next level up or down from `z`.
pub fn step(z: f64, up: bool) -> f64 {
    let levels = &LEVELS;
    if up {
        levels.iter().copied().find(|&l| l > z + 0.001).unwrap_or(MAX)
    } else {
        levels.iter().rev().copied().find(|&l| l < z - 0.001).unwrap_or(MIN)
    }
}

fn file<R: Runtime>(app: &AppHandle<R>) -> Option<std::path::PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join(FILE))
}

/// The saved zoom, or 1.0.
pub fn load<R: Runtime>(app: &AppHandle<R>) -> f64 {
    file(app)
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("zoom")?.as_f64())
        .map_or(1.0, |z| z.clamp(MIN, MAX))
}

/// Applies `z` to every open window, saves it, and tells the UI.
pub fn set<R: Runtime>(app: &AppHandle<R>, z: f64) {
    let z = z.clamp(MIN, MAX);
    *app.state::<Zoom>().0.lock().unwrap() = z;
    for w in app.webview_windows().values() {
        let _ = w.set_zoom(z);
    }
    if let Some(f) = file(app) {
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(f, serde_json::json!({ "zoom": z }).to_string());
    }
    let _ = app.emit("display:zoom", z);
}

/// Applies the current zoom to a window just opened.
pub fn apply<R: Runtime>(window: &tauri::WebviewWindow<R>) {
    let z = *window.app_handle().state::<Zoom>().0.lock().unwrap();
    if (z - 1.0).abs() > 0.001 {
        let _ = window.set_zoom(z);
    }
}

/// The default menu, with Zoom In, Zoom Out and Actual Size added to View.
pub fn menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let menu = Menu::default(app)?;
    let zoom_in = MenuItem::with_id(app, "zoom-in", "Zoom In", true, Some("CmdOrCtrl+="))?;
    let zoom_out = MenuItem::with_id(app, "zoom-out", "Zoom Out", true, Some("CmdOrCtrl+-"))?;
    let actual = MenuItem::with_id(app, "zoom-reset", "Actual Size", true, Some("CmdOrCtrl+0"))?;
    let sep = PredefinedMenuItem::separator(app)?;
    let view = menu
        .items()?
        .into_iter()
        .filter_map(|i| i.as_submenu().cloned())
        .find(|s| s.text().map(|t| t == "View").unwrap_or(false));
    match view {
        Some(v) => v.append_items(&[&sep, &zoom_in, &zoom_out, &actual])?,
        None => menu.append(&Submenu::with_items(app, "View", true, &[&zoom_in, &zoom_out, &actual])?)?,
    }
    Ok(menu)
}

/// Handles the View menu's zoom items.
pub fn on_menu<R: Runtime>(app: &AppHandle<R>, id: &str) {
    let z = *app.state::<Zoom>().0.lock().unwrap();
    match id {
        "zoom-in" => set(app, step(z, true)),
        "zoom-out" => set(app, step(z, false)),
        "zoom-reset" => set(app, 1.0),
        _ => {}
    }
}

#[tauri::command]
pub fn display_zoom(app: AppHandle) -> f64 {
    *app.state::<Zoom>().0.lock().unwrap()
}

#[tauri::command]
pub fn set_display_zoom(zoom: f64, app: AppHandle) {
    set(&app, zoom);
}

/// The display zoom commands.
pub fn commands() -> crate::Commands {
    crate::commands![display_zoom, set_display_zoom]
}

/// Puts back the zoom the app was left at.
pub fn setup(app: &AppHandle) {
    let zoom = load(app);
    if (zoom - 1.0).abs() > 0.001 {
        set(app, zoom);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_steps_through_the_levels_and_stops_at_the_ends() {
        assert_eq!(step(1.0, true), 1.1);
        assert_eq!(step(1.1, true), 1.25);
        assert_eq!(step(1.5, true), 1.5);
        assert_eq!(step(1.0, false), 0.9);
        assert_eq!(step(0.8, false), 0.8);
        assert_eq!(step(1.2, false), 1.1, "an in-between value snaps to the next level");
    }
}
