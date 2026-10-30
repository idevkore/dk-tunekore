//! tuneKore's plugin host: scans, loads and runs VST3 plugins behind one `Plugin` trait.
//!
//! * [`plugin`]: the traits and types the engine uses ([`PluginController`] on the main thread,
//!   [`Plugin`] on the audio thread), whatever the plugin's format.
//! * [`vst3`]: the VST3 host behind them. CLAP will sit beside it.
//! * [`scan`]: finding plugins, in a helper process so a crash cannot take the app down, and a
//!   JSON cache of what was found.
//!
//! Work in progress; see `README.md`.

pub mod editor_window;
pub mod offline;
pub mod plugin;
pub mod scan;
pub mod vst3;
pub mod wav;

pub use plugin::*;
