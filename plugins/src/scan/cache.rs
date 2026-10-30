//! What a scan found, and the JSON file it is kept in.

use crate::plugin::{BusLayout, Category, PluginInfo};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The cache format's version: a file with another number is ignored and scanned again.
pub const CACHE_VERSION: u32 = 1;

/// One plugin the scan found, or one bundle it could not read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanEntry {
    /// Name, vendor, version, category, class id and path. For a bundle that failed, the name is
    /// the file's name and the class id is empty.
    #[serde(flatten)]
    pub info: PluginInfo,
    /// The buses the plugin starts with.
    pub buses: BusLayout,
    pub params: u32,
    /// How long the probe took to load the plugin, in milliseconds.
    pub load_ms: u32,
    /// Why the plugin cannot be used: it threw, crashed, hung or refused to load. `None` if it
    /// loaded.
    pub error: Option<String>,
    /// The bundle's binary at probe time (`<modified seconds>:<bytes>`); a changed stamp means
    /// the plugin was updated and must be probed again.
    pub stamp: String,
}

impl ScanEntry {
    /// Whether the engine can put this on a track.
    pub fn usable(&self) -> bool {
        self.error.is_none() && matches!(self.info.category, Category::Instrument | Category::Effect)
    }
}

/// The scan's result, as kept on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanCache {
    pub version: u32,
    /// Seconds since the Unix epoch.
    pub scanned_at: u64,
    /// The folders that were searched.
    pub search_paths: Vec<PathBuf>,
    /// Sorted by name, then path.
    pub entries: Vec<ScanEntry>,
}

impl ScanCache {
    pub fn new(search_paths: Vec<PathBuf>, mut entries: Vec<ScanEntry>) -> ScanCache {
        entries.sort_by(|a, b| (a.info.name.to_lowercase(), &a.info.path).cmp(&(b.info.name.to_lowercase(), &b.info.path)));
        let scanned_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        ScanCache { version: CACHE_VERSION, scanned_at, search_paths, entries }
    }

    /// The plugins that loaded.
    pub fn plugins(&self) -> impl Iterator<Item = &ScanEntry> {
        self.entries.iter().filter(|e| e.usable())
    }

    /// The bundles that did not load, with why.
    pub fn failures(&self) -> impl Iterator<Item = &ScanEntry> {
        self.entries.iter().filter(|e| e.error.is_some())
    }

    pub fn find(&self, name: &str) -> Option<&ScanEntry> {
        self.entries.iter().find(|e| e.info.name.eq_ignore_ascii_case(name))
    }

    /// Reads a cache file. `None` if it is missing, damaged, or from another version.
    pub fn read(path: &Path) -> Option<ScanCache> {
        let bytes = std::fs::read(path).ok()?;
        let c: ScanCache = serde_json::from_slice(&bytes).ok()?;
        (c.version == CACHE_VERSION).then_some(c)
    }

    /// Writes the cache file in one step, so a reader never sees half of one.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

/// Where the cache lives unless the caller says otherwise: the user's application-support
/// folder.
pub fn default_cache_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dir = if cfg!(target_os = "macos") {
        home.map(|h| h.join("Library/Application Support/tuneKore"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("tuneKore"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(|x| PathBuf::from(x).join("tuneKore")).or_else(|| home.map(|h| h.join(".config/tuneKore")))
    };
    dir.unwrap_or_else(std::env::temp_dir).join("plugin-cache.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{BusInfo, BusKind, Format};

    pub fn entry(name: &str, error: Option<&str>) -> ScanEntry {
        ScanEntry {
            info: PluginInfo {
                format: Format::Vst3,
                name: name.into(),
                vendor: "Vendor".into(),
                version: "1.0".into(),
                category: Category::Instrument,
                sub_categories: vec!["Instrument".into(), "Synth".into()],
                class_id: "00112233445566778899AABBCCDDEEFF".into(),
                path: PathBuf::from(format!("/Library/Audio/Plug-Ins/VST3/{name}.vst3")),
            },
            buses: BusLayout { audio_outputs: vec![BusInfo { name: "Out".into(), channels: 2, kind: BusKind::Main, active: true }], event_inputs: 1, ..BusLayout::default() },
            params: 12,
            load_ms: 40,
            error: error.map(str::to_string),
            stamp: "1:2".into(),
        }
    }

    #[test]
    fn the_json_has_the_fields_a_reader_wants_and_round_trips() {
        let cache = ScanCache::new(vec!["/x".into()], vec![entry("Zed", None), entry("Alpha", Some("timed out after 60 s"))]);
        let json = serde_json::to_value(&cache).unwrap();
        let e = &json["entries"][0];
        assert_eq!(e["name"], "Alpha", "sorted by name");
        for key in ["name", "vendor", "version", "category", "class_id", "path", "buses", "error", "format"] {
            assert!(e.get(key).is_some(), "missing {key}: {e}");
        }
        assert_eq!(e["category"], "instrument");
        assert_eq!(e["buses"]["audio_outputs"][0]["channels"], 2);
        assert_eq!(e["error"], "timed out after 60 s");
        assert!(json["entries"][1]["error"].is_null());
        let back: ScanCache = serde_json::from_value(json).unwrap();
        assert_eq!(back, cache);
        assert_eq!(cache.plugins().map(|e| e.info.name.as_str()).collect::<Vec<_>>(), vec!["Zed"]);
        assert_eq!(cache.failures().count(), 1);
    }

    #[test]
    fn a_cache_file_is_written_whole_and_a_wrong_version_is_ignored() {
        let dir = std::env::temp_dir().join(format!("tunekore-cache-test-{}", std::process::id()));
        let path = dir.join("nested/plugins.json");
        let cache = ScanCache::new(vec![], vec![entry("A", None)]);
        cache.write(&path).unwrap();
        assert_eq!(ScanCache::read(&path), Some(cache.clone()));
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1, "no temp file left behind");
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(ScanCache::read(&path), None);
        let mut old = cache;
        old.version = 0;
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(ScanCache::read(&path), None);
        assert_eq!(ScanCache::read(&dir.join("missing.json")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
