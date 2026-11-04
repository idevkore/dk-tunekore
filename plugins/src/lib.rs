//! tuneKore's plugin host: scans, loads and runs VST3 and CLAP plugins behind one `Plugin` trait.
//!
//! * [`plugin`]: the traits and types the engine uses ([`PluginController`] on the main thread,
//!   [`Plugin`] on the audio thread), whatever the plugin's format.
//! * [`vst3`] and [`clap`]: the two hosts behind them. [`load`], [`classes`] and [`shutdown`]
//!   pick the right one.
//! * [`scan`]: finding plugins, in a helper process so a crash cannot take the app down, and a
//!   JSON cache of what was found.
//!
//! Work in progress; see `README.md`.

pub mod clap;
pub mod editor_window;
pub mod offline;
pub mod plugin;
mod rt;
pub mod scan;
pub mod vst3;
pub mod wav;

pub use plugin::*;

use std::path::Path;

/// Makes the plugin `info` names, with the host for its format. Main thread.
pub fn load(info: &PluginInfo) -> Result<Box<dyn PluginController>, PluginError> {
    match info.format {
        Format::Vst3 => vst3::load(info),
        Format::Clap => clap::load(info),
    }
}

/// The plugins in the bundle at `path`, in `format`. Loads the bundle's code: see
/// [`vst3::classes`] and [`clap::classes`].
pub fn classes(format: Format, path: &Path) -> Result<Vec<PluginInfo>, PluginError> {
    match format {
        Format::Vst3 => vst3::classes(path),
        Format::Clap => clap::classes(path),
    }
}

/// Lets every loaded plugin module run its exit code (`bundleExit`, `clap_entry.deinit`). Call it
/// when the app quits, after dropping every plugin.
pub fn shutdown() {
    vst3::shutdown();
    clap::shutdown();
}

/// Whether a category is something the engine puts on a track.
pub fn is_hostable(c: Category) -> bool {
    matches!(c, Category::Instrument | Category::Effect)
}
