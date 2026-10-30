//! Reading one bundle: what plugins it holds and what each looks like once loaded.
//!
//! This runs in the scanner's child process, never in the app: loading a plugin runs its code,
//! and some plugins crash, hang or throw while doing it.

use super::bundles;
use super::cache::{ScanEntry, ScanStatus};
use crate::plugin::{BusLayout, Category, Format, PluginInfo};
use crate::vst3;
use std::path::Path;
use std::time::Instant;

/// A bundle's binary at probe time: `<modified seconds>:<bytes>`.
pub fn stamp(bundle: &Path) -> String {
    bundles::bundle_binary(bundle)
        .and_then(|bin| std::fs::metadata(bin).ok())
        .map(|m| {
            let secs = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            format!("{secs}:{}", m.len())
        })
        .unwrap_or_default()
}

/// The entry for a bundle that could not be read at all.
pub fn failed_entry(bundle: &Path, status: ScanStatus, error: String) -> ScanEntry {
    let name = bundle.file_stem().map_or_else(|| bundle.display().to_string(), |s| s.to_string_lossy().into_owned());
    ScanEntry {
        info: PluginInfo {
            format: Format::Vst3,
            name,
            vendor: String::new(),
            version: String::new(),
            category: Category::Other,
            sub_categories: Vec::new(),
            class_id: String::new(),
            path: bundle.to_path_buf(),
        },
        buses: BusLayout::default(),
        params: 0,
        load_ms: 0,
        status,
        error: Some(error),
        stamp: stamp(bundle),
    }
}

/// Loads every plugin in `bundle`, one after another, and describes each. A plugin that fails
/// gets an entry with the error; a bundle that fails to load gets one entry for the bundle.
///
/// The plugins are left loaded (leaked): the probe process writes its answer and ends without
/// tearing them down, because some plugins crash shortly after they are unloaded (Kontakt 7's
/// web-request thread does), and that must not cost the scan its answer. Only call this in a
/// process that is about to end.
pub fn probe_bundle(bundle: &Path) -> Vec<ScanEntry> {
    let stamp = stamp(bundle);
    let infos = match vst3::classes(bundle) {
        Ok(i) => i,
        Err(e) => return vec![failed_entry(bundle, ScanStatus::Failed, e.to_string())],
    };
    if infos.is_empty() {
        return vec![failed_entry(bundle, ScanStatus::Failed, "no audio plugins in this bundle".into())];
    }
    infos
        .into_iter()
        .map(|info| {
            let t = Instant::now();
            match vst3::load(&info) {
                Ok(c) => {
                    let entry = ScanEntry {
                        buses: c.default_layout().clone(),
                        params: c.params().len() as u32,
                        load_ms: t.elapsed().as_millis().min(u128::from(u32::MAX)) as u32,
                        status: ScanStatus::Ok,
                        error: None,
                        stamp: stamp.clone(),
                        info,
                    };
                    std::mem::forget(c);
                    entry
                }
                Err(e) => ScanEntry { buses: BusLayout::default(), params: 0, load_ms: t.elapsed().as_millis().min(u128::from(u32::MAX)) as u32, status: ScanStatus::Failed, error: Some(e.to_string()), stamp: stamp.clone(), info },
            }
        })
        .collect()
}
