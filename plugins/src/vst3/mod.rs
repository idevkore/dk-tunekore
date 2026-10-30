//! The VST3 host.
//!
//! * [`classes`] lists the plugins in a `.vst3` bundle without making any.
//! * [`load`] makes one and returns its [`PluginController`]; `activate` on that gives the
//!   audio-thread [`Plugin`](crate::Plugin).
//!
//! `unsafe` is confined to this module: the plugin boundary is `unsafe` by nature. Every call into
//! plugin code goes through the exception-catching shim (`shim.rs`, `shim/tk_shim.cpp`).

#![allow(unsafe_code)]

mod editor;
mod events;
mod host;
mod instance;
mod module;
mod ptr;
pub(crate) mod shim;
mod util;

pub use instance::{Vst3Controller, Vst3Processor};

use crate::plugin::{Category, Format, PluginController, PluginError, PluginInfo};
use crate::scan::bundles;
use std::path::Path;

/// The factory category of audio processors, the only classes we list.
const AUDIO_MODULE_CLASS: &str = "Audio Module Class";

/// The plugins in a `.vst3` bundle (one bundle can hold several), read from its factory without
/// creating any. Loads the bundle's code, so a bundle that crashes on load crashes the caller:
/// the scanner runs this in a helper process.
pub fn classes(bundle: &Path) -> Result<Vec<PluginInfo>, PluginError> {
    let b = module::Bundle::load(bundle)?;
    Ok(b.classes()
        .into_iter()
        .filter(|c| c.category == AUDIO_MODULE_CLASS)
        .map(|c| {
            let subs = bundles::split_sub_categories(&c.sub_categories);
            PluginInfo {
                format: Format::Vst3,
                name: c.name,
                vendor: c.vendor,
                version: c.version,
                category: bundles::category_from_sub_categories(&subs),
                sub_categories: subs,
                class_id: util::hex(&c.cid),
                path: bundle.to_path_buf(),
            }
        })
        .collect())
}

/// Makes the plugin `info` names (from [`classes`] or the scan cache). Main thread.
pub fn load(info: &PluginInfo) -> Result<Box<dyn PluginController>, PluginError> {
    if info.format != Format::Vst3 {
        return Err(PluginError::Unsupported(format!("{:?} plugins are not hosted by the VST3 host", info.format)));
    }
    let cid = util::parse_hex(&info.class_id).ok_or_else(|| PluginError::NotFound { path: info.path.display().to_string(), class_id: info.class_id.clone() })?;
    let bundle = module::Bundle::load(&info.path)?;
    if !bundle.classes().iter().any(|c| c.cid == cid) {
        return Err(PluginError::NotFound { path: info.path.display().to_string(), class_id: info.class_id.clone() });
    }
    let mut info = info.clone();
    // Trust the bundle over a stale cache for what the plugin is called and what it is for.
    if let Some(c) = bundle.classes().into_iter().find(|c| c.cid == cid) {
        let subs = bundles::split_sub_categories(&c.sub_categories);
        info.category = bundles::category_from_sub_categories(&subs);
        info.sub_categories = subs;
    }
    let inst = instance::Instance::create(&bundle, &info)?;
    Ok(Box::new(Vst3Controller::new(inst)))
}

/// Like [`load`], from a bundle path and a class id.
pub fn load_class(bundle: &Path, class_id: &str) -> Result<Box<dyn PluginController>, PluginError> {
    let info = classes(bundle)?
        .into_iter()
        .find(|i| i.class_id.eq_ignore_ascii_case(class_id))
        .ok_or_else(|| PluginError::NotFound { path: bundle.display().to_string(), class_id: class_id.to_string() })?;
    load(&info)
}

/// Whether a category is something the engine puts on a track.
pub fn is_hostable(c: Category) -> bool {
    matches!(c, Category::Instrument | Category::Effect)
}
