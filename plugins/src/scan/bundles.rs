//! Finding VST3 and CLAP plugins on disk (safe, pure `std::fs`), resolving bundles to their
//! binaries and mapping VST3 sub-categories and CLAP features to our categories (instrument,
//! effect or other).
//!
//! Derived from soundCraft's `vst3-host` (`scan.rs`), MIT OR Apache-2.0; see
//! `third_party/soundcraft/`. What changed: the category mapping, and CLAP's folders, files and
//! features (soundCraft's `clap-host` `scan.rs` names the same folders).

use crate::{Category, Format};
use std::path::{Path, PathBuf};

/// Directory recursion limit while scanning (vendors nest folders, never deeply).
const MAX_DEPTH: usize = 8;
/// Upper bound on bundles collected by one scan (hostile or huge folders).
const MAX_BUNDLES: usize = 4096;
/// Upper bound on directory entries visited by one scan.
const MAX_ENTRIES: usize = 100_000;

/// The operating systems whose VST3 bundle layouts we know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Windows,
    /// Linux and the BSDs (same bundle layout).
    Unix,
}

impl Os {
    /// The OS this build runs on.
    pub fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Unix
        }
    }
}

/// Every standard plugin folder for this OS: the VST3 ones, then the CLAP ones.
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut v = vst3_search_paths();
    v.extend(clap_search_paths());
    v
}

/// The standard VST3 search locations for this OS (existing or not), followed by every entry of
/// the `VST3_PATH` environment variable.
pub fn vst3_search_paths() -> Vec<PathBuf> {
    let mut v = os_search_paths(Os::current(), env_path("HOME"), env_path("COMMONPROGRAMFILES"), env_path("LOCALAPPDATA"));
    v.extend(env_list("VST3_PATH"));
    v
}

/// The standard CLAP search locations for this OS (existing or not), followed by every entry of
/// the `CLAP_PATH` environment variable (`:`-separated, `;` on Windows), as CLAP's `entry.h` lists
/// them.
pub fn clap_search_paths() -> Vec<PathBuf> {
    let mut v = os_clap_search_paths(Os::current(), env_path("HOME"), env_path("COMMONPROGRAMFILES"), env_path("LOCALAPPDATA"));
    v.extend(env_list("CLAP_PATH"));
    v
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn env_list(name: &str) -> Vec<PathBuf> {
    std::env::var_os(name).map(|v| std::env::split_paths(&v).filter(|p| !p.as_os_str().is_empty()).collect()).unwrap_or_default()
}

/// The OS-specific standard CLAP locations, given the relevant environment values.
pub fn os_clap_search_paths(os: Os, home: Option<PathBuf>, common_program_files: Option<PathBuf>, local_app_data: Option<PathBuf>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    match os {
        Os::MacOs => {
            if let Some(h) = home {
                v.push(h.join("Library/Audio/Plug-Ins/CLAP"));
            }
            v.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
        }
        Os::Windows => {
            if let Some(c) = common_program_files {
                v.push(c.join("CLAP"));
            }
            if let Some(l) = local_app_data {
                v.push(l.join("Programs").join("Common").join("CLAP"));
            }
        }
        Os::Unix => {
            if let Some(h) = home {
                v.push(h.join(".clap"));
            }
            v.push(PathBuf::from("/usr/lib/clap"));
        }
    }
    v
}

/// The OS-specific standard VST3 locations, given the relevant environment values.
pub fn os_search_paths(os: Os, home: Option<PathBuf>, common_program_files: Option<PathBuf>, local_app_data: Option<PathBuf>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    match os {
        Os::MacOs => {
            if let Some(h) = home {
                v.push(h.join("Library/Audio/Plug-Ins/VST3"));
            }
            v.push(PathBuf::from("/Library/Audio/Plug-Ins/VST3"));
        }
        Os::Windows => {
            if let Some(c) = common_program_files {
                v.push(c.join("VST3"));
            }
            if let Some(l) = local_app_data {
                v.push(l.join("Programs").join("Common").join("VST3"));
            }
        }
        Os::Unix => {
            if let Some(h) = home {
                v.push(h.join(".vst3"));
            }
            v.push(PathBuf::from("/usr/lib/vst3"));
            v.push(PathBuf::from("/usr/local/lib/vst3"));
        }
    }
    v
}

/// True when the path names a VST3 plugin (`*.vst3`, case-insensitive).
pub fn is_vst3_path(p: &Path) -> bool {
    has_extension(p, "vst3")
}

/// True when the path names a CLAP plugin (`*.clap`, case-insensitive).
pub fn is_clap_path(p: &Path) -> bool {
    has_extension(p, "clap")
}

fn has_extension(p: &Path, ext: &str) -> bool {
    p.extension().and_then(std::ffi::OsStr::to_str).is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// The plugin format a path names by its extension (`.vst3` or `.clap`), if either.
pub fn format_of(p: &Path) -> Option<Format> {
    if is_vst3_path(p) {
        Some(Format::Vst3)
    } else if is_clap_path(p) {
        Some(Format::Clap)
    } else {
        None
    }
}

/// Recursively collects `.vst3` and `.clap` bundles and files under `dirs` (missing directories
/// are skipped). A plugin bundle is not descended into. The result is sorted and deduplicated.
pub fn find_bundles(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut budget = MAX_ENTRIES;
    for d in dirs {
        if format_of(d).is_some() && d.exists() {
            out.push(d.clone());
        } else {
            walk(d, 0, &mut out, &mut budget);
        }
    }
    out.sort();
    out.dedup();
    out.truncate(MAX_BUNDLES);
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, budget: &mut usize) {
    if depth > MAX_DEPTH || out.len() >= MAX_BUNDLES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        if *budget == 0 || out.len() >= MAX_BUNDLES {
            return;
        }
        *budget -= 1;
        let p = entry.path();
        // `metadata` follows symlinks (plugins are often symlinked in); errors are skipped.
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if format_of(&p).is_some() {
            out.push(p);
        } else if md.is_dir() {
            walk(&p, depth + 1, out, budget);
        }
    }
}

/// The `Contents/<dir>` folder names that hold this machine's binaries, most specific first.
fn arch_dirs(os: Os) -> &'static [&'static str] {
    match os {
        Os::MacOs => &["MacOS"],
        Os::Windows => {
            if cfg!(target_arch = "aarch64") {
                &["arm64-win", "arm64ec-win", "arm64x-win"]
            } else if cfg!(target_arch = "x86") {
                &["x86-win"]
            } else {
                &["x86_64-win"]
            }
        }
        Os::Unix => {
            if cfg!(target_arch = "aarch64") {
                &["aarch64-linux"]
            } else if cfg!(target_arch = "x86") {
                &["i386-linux"]
            } else {
                &["x86_64-linux"]
            }
        }
    }
}

/// The loadable binary of a plugin for this OS: a single-file `.vst3` itself, or inside a bundle
/// directory `Contents/MacOS/<name>` (macOS), `Contents/x86_64-win/<name>.vst3` (Windows) or
/// `Contents/x86_64-linux/<name>.so` (Linux/BSD), falling back to the only/first fitting file there.
pub fn bundle_binary(path: &Path) -> Option<PathBuf> {
    bundle_binary_for(Os::current(), path)
}

/// [`bundle_binary`] for a given OS layout (testable everywhere).
pub fn bundle_binary_for(os: Os, path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if !path.is_dir() {
        return None;
    }
    let stem = path.file_stem()?;
    let ext = match os {
        Os::MacOs => None,
        Os::Windows => Some("vst3"),
        Os::Unix => Some("so"),
    };
    for dir in arch_dirs(os) {
        let folder = path.join("Contents").join(dir);
        let mut named = folder.join(stem);
        if let Some(e) = ext {
            named.set_extension(e);
        }
        if named.is_file() {
            return Some(named);
        }
        let Ok(rd) = std::fs::read_dir(&folder) else { continue };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .filter(|p| ext.is_none_or(|e| p.extension().and_then(std::ffi::OsStr::to_str).is_some_and(|x| x.eq_ignore_ascii_case(e))))
            .take(64)
            .collect();
        files.sort();
        if let Some(f) = files.into_iter().next() {
            return Some(f);
        }
    }
    None
}

/// The `|`-separated VST3 sub-category tokens (`"Fx|Reverb"` → `["Fx", "Reverb"]`).
pub fn split_sub_categories(s: &str) -> Vec<String> {
    s.split('|').map(str::trim).filter(|t| !t.is_empty()).take(32).map(str::to_string).collect()
}

/// Instrument or effect, from the VST3 sub-categories (`Fx|EQ`, `Instrument|Synth`, ...).
pub fn category_from_sub_categories<S: AsRef<str>>(subs: &[S]) -> Category {
    if is_instrument(subs) {
        Category::Instrument
    } else if subs.iter().any(|f| f.as_ref().eq_ignore_ascii_case("Fx")) {
        Category::Effect
    } else {
        Category::Other
    }
}

/// True when the sub-categories mark an instrument (`Instrument`, `Instrument|Synth`, …).
pub fn is_instrument<S: AsRef<str>>(subs: &[S]) -> bool {
    subs.first().is_some_and(|f| f.as_ref().eq_ignore_ascii_case("Instrument"))
}

/// Instrument or effect, from a CLAP plugin's features (`instrument`, `audio-effect`,
/// `analyzer`, ...). An analyzer processes audio, so it is an effect; note effects and note
/// detectors (MIDI-only plugins) are other, as tracks can't host those yet.
pub fn category_from_features<S: AsRef<str>>(features: &[S]) -> Category {
    let has = |f: &str| features.iter().any(|x| x.as_ref().eq_ignore_ascii_case(f));
    if has("instrument") {
        Category::Instrument
    } else if has("audio-effect") || has("analyzer") {
        Category::Effect
    } else {
        Category::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soundcraft-vst3-scan-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn finds_files_and_bundles_recursively_but_not_inside_bundles() {
        let d = tmp("find");
        std::fs::write(d.join("a.vst3"), b"x").unwrap();
        std::fs::write(d.join("readme.txt"), b"x").unwrap();
        std::fs::create_dir_all(d.join("vendor/sub")).unwrap();
        std::fs::write(d.join("vendor/sub/B.VST3"), b"x").unwrap();
        std::fs::create_dir_all(d.join("c.vst3/Contents/MacOS")).unwrap();
        std::fs::write(d.join("c.vst3/Contents/MacOS/c"), b"x").unwrap();
        std::fs::write(d.join("c.vst3/Contents/inner.vst3"), b"x").unwrap();
        std::fs::write(d.join("vendor/D.clap"), b"x").unwrap();
        std::fs::create_dir_all(d.join("E.clap/Contents/MacOS")).unwrap();
        std::fs::write(d.join("E.clap/Contents/MacOS/E"), b"x").unwrap();
        std::fs::write(d.join("E.clap/Contents/inner.vst3"), b"x").unwrap();
        let found = find_bundles(&[d.clone(), d.join("missing")]);
        let names: Vec<String> = found.iter().map(|p| p.strip_prefix(&d).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(names, vec!["E.clap", "a.vst3", "c.vst3", "vendor/D.clap", "vendor/sub/B.VST3"], "both formats, not inside bundles");
        assert_eq!(find_bundles(&[d.join("vendor/D.clap")]), vec![d.join("vendor/D.clap")]);
        assert_eq!((format_of(&d.join("x.CLAP")), format_of(&d.join("x.vst3")), format_of(&d.join("x.component"))), (Some(Format::Clap), Some(Format::Vst3), None));
        assert_eq!(find_bundles(&[d.join("a.vst3")]), vec![d.join("a.vst3")]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bundles_resolve_per_os_layout() {
        let d = tmp("layout");
        // macOS
        std::fs::create_dir_all(d.join("M.vst3/Contents/MacOS")).unwrap();
        std::fs::write(d.join("M.vst3/Contents/MacOS/M"), b"x").unwrap();
        std::fs::write(d.join("M.vst3/Contents/MacOS/aaa"), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::MacOs, &d.join("M.vst3")), Some(d.join("M.vst3/Contents/MacOS/M")));
        // macOS fallback: first file when the name differs.
        std::fs::create_dir_all(d.join("N.vst3/Contents/MacOS")).unwrap();
        std::fs::write(d.join("N.vst3/Contents/MacOS/other"), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::MacOs, &d.join("N.vst3")), Some(d.join("N.vst3/Contents/MacOS/other")));
        // Windows
        let win = arch_dirs(Os::Windows)[0];
        std::fs::create_dir_all(d.join(format!("W.vst3/Contents/{win}"))).unwrap();
        std::fs::write(d.join(format!("W.vst3/Contents/{win}/W.vst3")), b"x").unwrap();
        std::fs::write(d.join(format!("W.vst3/Contents/{win}/helper.dll")), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::Windows, &d.join("W.vst3")), Some(d.join(format!("W.vst3/Contents/{win}/W.vst3"))));
        // Linux
        let lin = arch_dirs(Os::Unix)[0];
        std::fs::create_dir_all(d.join(format!("L.vst3/Contents/{lin}"))).unwrap();
        std::fs::write(d.join(format!("L.vst3/Contents/{lin}/notes.txt")), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::Unix, &d.join("L.vst3")), None, "only .so files qualify");
        std::fs::write(d.join(format!("L.vst3/Contents/{lin}/Renamed.so")), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::Unix, &d.join("L.vst3")), Some(d.join(format!("L.vst3/Contents/{lin}/Renamed.so"))));
        std::fs::write(d.join(format!("L.vst3/Contents/{lin}/L.so")), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::Unix, &d.join("L.vst3")), Some(d.join(format!("L.vst3/Contents/{lin}/L.so"))));
        // Single-file plugin, wrong layout, missing.
        std::fs::write(d.join("single.vst3"), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::Windows, &d.join("single.vst3")), Some(d.join("single.vst3")));
        assert_eq!(bundle_binary_for(Os::Windows, &d.join("M.vst3")), None);
        assert_eq!(bundle_binary_for(Os::MacOs, &d.join("nope.vst3")), None);
        std::fs::create_dir_all(d.join("Empty.vst3")).unwrap();
        assert_eq!(bundle_binary(&d.join("Empty.vst3")), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn os_paths_cover_the_standard_locations() {
        let mac = os_search_paths(Os::MacOs, Some("/Users/u".into()), None, None);
        assert_eq!(mac, vec![PathBuf::from("/Users/u/Library/Audio/Plug-Ins/VST3"), PathBuf::from("/Library/Audio/Plug-Ins/VST3")]);
        let win = os_search_paths(Os::Windows, None, Some("C:/CPF".into()), Some("C:/LAD".into()));
        assert_eq!(win[0], PathBuf::from("C:/CPF").join("VST3"));
        assert_eq!(win.len(), 2);
        let unix = os_search_paths(Os::Unix, Some("/home/u".into()), None, None);
        assert_eq!(unix, vec![PathBuf::from("/home/u/.vst3"), PathBuf::from("/usr/lib/vst3"), PathBuf::from("/usr/local/lib/vst3")]);
        assert!(!default_search_paths().is_empty());
    }

    #[test]
    fn clap_paths_are_the_ones_clap_names() {
        let mac = os_clap_search_paths(Os::MacOs, Some("/Users/u".into()), None, None);
        assert_eq!(mac, vec![PathBuf::from("/Users/u/Library/Audio/Plug-Ins/CLAP"), PathBuf::from("/Library/Audio/Plug-Ins/CLAP")]);
        let win = os_clap_search_paths(Os::Windows, None, Some("C:/CPF".into()), Some("C:/LAD".into()));
        assert_eq!(win, vec![PathBuf::from("C:/CPF").join("CLAP"), PathBuf::from("C:/LAD").join("Programs").join("Common").join("CLAP")]);
        let unix = os_clap_search_paths(Os::Unix, Some("/home/u".into()), None, None);
        assert_eq!(unix, vec![PathBuf::from("/home/u/.clap"), PathBuf::from("/usr/lib/clap")]);
        let all = default_search_paths();
        assert!(all.iter().any(|p| p.ends_with("CLAP") || p.ends_with("clap") || p.ends_with(".clap")), "{all:?}");
    }

    #[test]
    fn clap_features_map_to_categories() {
        assert_eq!(category_from_features(&["instrument", "synthesizer", "stereo"]), Category::Instrument);
        assert_eq!(category_from_features(&["audio-effect", "reverb"]), Category::Effect);
        assert_eq!(category_from_features(&["analyzer"]), Category::Effect);
        assert_eq!(category_from_features(&["note-effect", "arpeggiator"]), Category::Other);
        assert_eq!(category_from_features::<&str>(&[]), Category::Other);
    }

    #[test]
    fn sub_categories_map_to_categories() {
        let c = |s: &str| category_from_sub_categories(&split_sub_categories(s));
        assert_eq!(c("Fx|Reverb"), Category::Effect);
        assert_eq!(c("Instrument|Synth"), Category::Instrument);
        assert_eq!(c("Fx"), Category::Effect);
        assert_eq!(c(""), Category::Other);
        assert_eq!(c("Analyzer"), Category::Other);
        assert_eq!(split_sub_categories(" Fx || Delay |"), vec!["Fx", "Delay"]);
        assert!(is_instrument(&split_sub_categories("Instrument|Sampler")));
        assert!(!is_instrument(&split_sub_categories("Fx|Instrument")));
    }
}
