//! The CLAP host, beside the VST3 one and behind the same traits.
//!
//! * [`classes`] lists the plugins in a `.clap` without making any.
//! * [`load`] makes one and returns its [`PluginController`]; `activate` on that gives the
//!   audio-thread [`Plugin`](crate::Plugin).
//!
//! CLAP is a C API (its headers are MIT; the bindings are `clap-sys`). The host keeps the rules
//! the VST3 host keeps: every call into plugin code goes through the exception-catching shim
//! (`shim.rs`, `shim/tk_shim.cpp`), the controller is used on the main thread and the processor on
//! the audio thread, `process` neither allocates nor locks, and a plugin that throws or hangs is
//! switched off and leaked rather than called again. What CLAP adds:
//!
//! * Parameters are in the plugin's own units at the CLAP boundary and normalized (0 to 1) at
//!   ours; the controller and the processor convert with the ranges the plugin lists.
//! * The plugin's requests to the main thread (`request_callback`, timers, a parameter flush, a
//!   resize) are served by `poll`. The app polls every 100 ms, so a timer runs at most that often.
//! * Edits in the plugin's own window come back as output events of `process` (or of
//!   `params.flush`: on the main thread while the plugin is inactive, and in the audio thread's
//!   place when no block comes, as on a switched-off device), and `poll` reports them as
//!   [`ControllerEvent`](crate::ControllerEvent)s.
//! * State is the plugin's own (`clap.state`), with no container around it.
//!
//! Derived in part from soundCraft's `clap-host`, MIT OR Apache-2.0; see `third_party/soundcraft/`.
//! `unsafe` is confined to this module and `vst3`: the plugin boundary is `unsafe` by nature.

#![allow(unsafe_code)]

mod editor;
mod events;
mod host;
mod instance;
mod module;
pub(crate) mod shim;

pub use instance::{ClapController, ClapProcessor};

use crate::plugin::{Format, PluginController, PluginError, PluginInfo};
use crate::scan::bundles;
use module::{Descriptor, Module};
use std::path::Path;

fn info_of(d: Descriptor, path: &Path) -> PluginInfo {
    PluginInfo {
        format: Format::Clap,
        name: d.name,
        vendor: d.vendor,
        version: d.version,
        category: bundles::category_from_features(&d.features),
        sub_categories: d.features,
        class_id: d.id,
        path: path.to_path_buf(),
    }
}

/// The plugins in a `.clap` (one file can hold several), read from its factory without creating
/// any. Loads the file's code, so one that crashes on load crashes the caller: the scanner runs
/// this in a helper process.
pub fn classes(path: &Path) -> Result<Vec<PluginInfo>, PluginError> {
    let m = Module::load(path)?;
    let (found, err) = m.descriptors();
    if found.is_empty()
        && let Some(e) = err
    {
        return Err(PluginError::Failed(e));
    }
    Ok(found.into_iter().map(|d| info_of(d, path)).collect())
}

/// Makes the plugin `info` names (from [`classes`] or the scan cache). Main thread.
pub fn load(info: &PluginInfo) -> Result<Box<dyn PluginController>, PluginError> {
    if info.format != Format::Clap {
        return Err(PluginError::Unsupported(format!("{:?} plugins are not hosted by the CLAP host", info.format)));
    }
    let not_found = || PluginError::NotFound { path: info.path.display().to_string(), class_id: info.class_id.clone() };
    let m = Module::load(&info.path)?;
    // Trust the file over a stale cache for what the plugin is called and what it is for.
    let found = m.descriptors().0.into_iter().find(|d| d.id == info.class_id).ok_or_else(not_found)?;
    let fresh = info_of(found, &info.path);
    let info = PluginInfo { category: fresh.category, sub_categories: fresh.sub_categories, ..info.clone() };
    let inst = instance::Instance::create(&m, &info)?;
    Ok(Box::new(ClapController::new(inst)))
}

/// Like [`load`], from a `.clap` path and a plugin id.
pub fn load_class(path: &Path, id: &str) -> Result<Box<dyn PluginController>, PluginError> {
    let info = classes(path)?.into_iter().find(|i| i.class_id == id).ok_or_else(|| PluginError::NotFound { path: path.display().to_string(), class_id: id.to_string() })?;
    load(&info)
}

/// Runs `clap_entry.deinit` for every loaded module with no live or leaked plugin, and stops
/// loading modules. Call it when the app quits, after dropping every plugin
/// ([`crate::shutdown`] does, with the VST3 host's).
pub fn shutdown() {
    module::exit_modules();
}
