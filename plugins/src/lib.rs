//! tuneKore's plugin host: scans, loads and runs VST3 plugins behind one `Plugin` trait.
//!
//! Work in progress; see `README.md`.

pub mod scan;
pub mod vst3;

mod plugin;

pub use plugin::{Category, Transport};
