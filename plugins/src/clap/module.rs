//! Loading `.clap` binaries and reading their plugin factories.
//!
//! Derived from soundCraft's `clap-host` (`ffi.rs`: `Bundle`), MIT OR Apache-2.0; see
//! `third_party/soundcraft/`. What changed: every call into plugin code goes through the
//! exception-catching shim (`clap_entry.init`, `get_factory` and `deinit` included); loaded modules
//! are never unloaded, and `deinit` runs once, from [`exit_modules`] at quit, for modules with no
//! live or leaked instance; errors are [`PluginError`].

use super::shim;
use crate::plugin::PluginError;
use crate::scan::bundles;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::version::clap_version_is_compatible;
use std::ffi::{c_char, CStr, CString};
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, TryLockError};

/// Most plugins one factory may report (hostile-input cap).
const MAX_PLUGINS: u32 = 1024;
/// Most feature strings read from one descriptor.
const MAX_FEATURES: usize = 64;
/// Longest descriptor string kept.
const MAX_STRING: usize = 1024;

/// A plugin as its factory describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Descriptor {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub features: Vec<String>,
}

/// A loaded `.clap` binary, its entry initialised, and its plugin factory.
pub(crate) struct Module {
    entry: *const clap_plugin_entry,
    factory: *const clap_plugin_factory,
    /// Set when plugin objects were leaked on purpose (a plugin that threw or hung): the module
    /// must never be deinitialised while any of them is alive.
    leaked: AtomicBool,
    // Never closed: code stays mapped for the process lifetime, even after `deinit`.
    _lib: ManuallyDrop<libloading::Library>,
}

// SAFETY: CLAP requires the entry and the plugin factory to be thread-safe; the raw pointers point
// at read-only tables inside the loaded library, which is never unmapped.
unsafe impl Send for Module {}
// SAFETY: as above.
unsafe impl Sync for Module {}

type Loaded = Vec<(PathBuf, Arc<Module>)>;

/// Loaded modules, oldest first (deinitialised newest first).
fn loaded() -> &'static Mutex<Loaded> {
    static L: OnceLock<Mutex<Loaded>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(Vec::new()))
}

/// Set once the modules were deinitialised: loading another one after that is refused.
static MODULES_EXITED: AtomicBool = AtomicBool::new(false);

/// Reads a NUL-terminated plugin string, bounded (empty for null).
///
/// # Safety
/// `p` is null or points to a NUL-terminated string that stays valid for the call.
pub(crate) unsafe fn read_cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: non-null and NUL-terminated per this function's contract.
    let bytes = unsafe { CStr::from_ptr(p) }.to_bytes();
    let bytes = &bytes[..bytes.len().min(MAX_STRING)];
    String::from_utf8_lossy(bytes).trim().to_string()
}

/// Reads a fixed-size `char[N]` buffer, stopping at the first NUL or the end of the array.
pub(crate) fn read_chars(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

impl Module {
    /// Loads (once per path, then cached for the life of the process) and initialises a `.clap`.
    pub fn load(path: &Path) -> Result<Arc<Module>, PluginError> {
        let mut cache = loaded().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, m)) = cache.iter().find(|(p, _)| p == path) {
            return Ok(m.clone());
        }
        if MODULES_EXITED.load(Ordering::Acquire) {
            return Err(PluginError::Load { path: path.display().to_string(), reason: "plugin hosting has shut down".into() });
        }
        let m = Arc::new(Module::open(path)?);
        cache.push((path.to_path_buf(), m.clone()));
        Ok(m)
    }

    fn open(path: &Path) -> Result<Module, PluginError> {
        let err = |m: String| PluginError::Load { path: path.display().to_string(), reason: m };
        let exe = bundles::bundle_binary(path).ok_or_else(|| err("no loadable binary".into()))?;
        // SAFETY: loading a shared library runs its static initialisers. That is inherent to
        // hosting plugins; we only load files found in plugin folders or named explicitly.
        let lib = unsafe { libloading::Library::new(&exe) }.map_err(|e| err(e.to_string()))?;
        // SAFETY: `clap_entry` is the CLAP-mandated exported data symbol of type
        // `const clap_plugin_entry`; the symbol's address is therefore a `*const clap_plugin_entry`.
        let entry: *const clap_plugin_entry = match unsafe { lib.get::<*const clap_plugin_entry>(b"clap_entry\0") } {
            Ok(sym) => *sym,
            Err(e) => return Err(err(format!("not a CLAP plugin: {e}"))),
        };
        if entry.is_null() {
            return Err(err("not a CLAP plugin: clap_entry is null".into()));
        }
        // SAFETY: non-null pointer to the entry struct inside the library, which stays mapped.
        let e = unsafe { *entry };
        if !clap_version_is_compatible(e.clap_version) {
            return Err(err(format!("it is written for CLAP {}.{}, which tuneKore can't host", e.clap_version.major, e.clap_version.minor)));
        }
        // CLAP takes the bundle on macOS and the binary elsewhere.
        let named = if cfg!(target_os = "macos") { path } else { exe.as_path() };
        let cpath = CString::new(named.to_string_lossy().into_owned()).map_err(|_| err("the path holds a NUL".into()))?;
        // SAFETY: `init` comes from a valid entry; the path is NUL-terminated and outlives the call.
        match unsafe { shim::b_p(e.init, cpath.as_ptr()) } {
            Some(Ok(true)) => {}
            Some(Ok(false)) => return Err(err("clap_entry.init failed".into())),
            Some(Err(x)) => return Err(err(format!("clap_entry.init threw: {x}"))),
            None => return Err(err("clap_entry has no init".into())),
        }
        // The entry ran: balance it before giving up on the module.
        let fail = |m: String| {
            // SAFETY: balanced with the successful `init` above; no plugin exists.
            let _ = unsafe { shim::v_(e.deinit) };
            err(m)
        };
        // SAFETY: the entry is initialised; the id is a static NUL-terminated string.
        let factory = match unsafe { shim::p_p(e.get_factory, CLAP_PLUGIN_FACTORY_ID.as_ptr()) } {
            Some(Ok(f)) if !f.is_null() => f.cast::<clap_plugin_factory>(),
            Some(Ok(_)) => return Err(fail("it has no plugin factory".into())),
            Some(Err(x)) => return Err(fail(format!("clap_entry.get_factory threw: {x}"))),
            None => return Err(fail("clap_entry has no get_factory".into())),
        };
        Ok(Module { entry, factory, leaked: AtomicBool::new(false), _lib: ManuallyDrop::new(lib) })
    }

    fn factory(&self) -> clap_plugin_factory {
        // SAFETY: non-null at load time; points into the still-mapped library.
        unsafe { *self.factory }
    }

    /// Plugin objects of this module were leaked on purpose: never deinitialise it.
    pub fn mark_leaked(&self) {
        self.leaked.store(true, Ordering::Release);
    }

    /// Every plugin this binary provides. A factory that throws gives what it gave so far, and
    /// the error.
    pub fn descriptors(&self) -> (Vec<Descriptor>, Option<String>) {
        let f = self.factory();
        // SAFETY: the factory's own function, with the factory it belongs to.
        let n = match unsafe { shim::u_p(f.get_plugin_count, self.factory) } {
            Some(Ok(n)) => n.min(MAX_PLUGINS),
            Some(Err(e)) => return (Vec::new(), Some(format!("clap_plugin_factory.get_plugin_count threw: {e}"))),
            None => 0,
        };
        let mut out = Vec::new();
        for i in 0..n {
            // SAFETY: index below the reported count.
            let d = match unsafe { shim::p_pu(f.get_plugin_descriptor, self.factory, i) } {
                Some(Ok(d)) if !d.is_null() => d,
                Some(Ok(_)) | None => continue,
                Some(Err(e)) => return (out, Some(format!("clap_plugin_factory.get_plugin_descriptor threw: {e}"))),
            };
            // SAFETY: a non-null descriptor owned by the library, valid while it is loaded.
            let d: clap_plugin_descriptor = unsafe { *d };
            // SAFETY (all `read_cstr` below): descriptor strings are null or NUL-terminated.
            let id = unsafe { read_cstr(d.id) };
            if id.is_empty() {
                continue;
            }
            let mut features = Vec::new();
            if !d.features.is_null() {
                for k in 0..MAX_FEATURES {
                    // SAFETY: `features` is a NULL-terminated array; we stop at the terminator (or
                    // the cap) and never read past it.
                    let p = unsafe { *d.features.add(k) };
                    if p.is_null() {
                        break;
                    }
                    let s = unsafe { read_cstr(p) };
                    if !s.is_empty() {
                        features.push(s);
                    }
                }
            }
            let name = unsafe { read_cstr(d.name) };
            out.push(Descriptor {
                name: if name.is_empty() { id.clone() } else { name },
                id,
                vendor: unsafe { read_cstr(d.vendor) },
                version: unsafe { read_cstr(d.version) },
                features,
            });
        }
        (out, None)
    }

    /// Asks the factory for plugin `id`, talking to `host`. The plugin is created, not yet
    /// initialised.
    pub fn create(&self, host: *const clap_host, id: &str) -> Result<*const clap_plugin, PluginError> {
        let cid = CString::new(id).map_err(|_| PluginError::Failed("the plugin id holds a NUL".into()))?;
        // SAFETY: valid factory; `host` outlives the plugin (the instance owns both and destroys
        // the plugin first); `cid` outlives the call.
        match unsafe { shim::p_ppp(self.factory().create_plugin, self.factory, host, cid.as_ptr()) } {
            Some(Ok(p)) if !p.is_null() => Ok(p),
            Some(Ok(_)) => Err(PluginError::Failed("the factory made no plugin".into())),
            Some(Err(e)) => {
                // It threw part way through: whatever it made stays, and so must the module.
                self.mark_leaked();
                Err(PluginError::Exception { during: "clap_plugin_factory.create_plugin", message: e.0 })
            }
            None => Err(PluginError::Failed("the factory can't make plugins".into())),
        }
    }

    fn deinit(&self) {
        // SAFETY: `entry` was valid at load time and the library stays mapped.
        let e = unsafe { *self.entry };
        // SAFETY: balanced with the `init` in `open`; called once (`exit_modules` takes the module
        // out of the cache), after every instance is gone (each holds an `Arc` to the module).
        let _ = unsafe { shim::v_(e.deinit) };
    }
}

/// Deinitialises every loaded module that has no live or leaked plugin, newest first, on the
/// calling thread (CLAP's entry functions are thread-safe). Runs once; later loads are refused.
pub(crate) fn exit_modules() {
    if MODULES_EXITED.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut cache = match loaded().try_lock() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        // Another thread is loading a module right now: leave every module as it is.
        Err(TryLockError::WouldBlock) => return,
    };
    let mut kept = Vec::new();
    let mut exit = Vec::new();
    while let Some((path, m)) = cache.pop() {
        if m.leaked.load(Ordering::Acquire) {
            kept.push((path, m));
            continue;
        }
        match Arc::try_unwrap(m) {
            Ok(m) => exit.push(m),
            Err(m) => kept.push((path, m)),
        }
    }
    kept.reverse();
    *cache = kept;
    drop(cache);
    for m in exit {
        m.deinit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_strings_are_bounded_and_trimmed() {
        // SAFETY: null and static NUL-terminated strings.
        assert_eq!(unsafe { read_cstr(std::ptr::null()) }, "");
        assert_eq!(unsafe { read_cstr(c" Gain ".as_ptr()) }, "Gain");
        let long = CString::new("x".repeat(5000)).unwrap();
        assert_eq!(unsafe { read_cstr(long.as_ptr()) }.len(), MAX_STRING);
        let buf: Vec<c_char> = b"ab\0cd".iter().map(|&b| b as c_char).collect();
        assert_eq!(read_chars(&buf), "ab");
        let full: Vec<c_char> = b"abc".iter().map(|&b| b as c_char).collect();
        assert_eq!(read_chars(&full), "abc", "no NUL: the whole array");
    }

    #[test]
    fn a_file_that_is_not_a_clap_plugin_is_an_error() {
        let dir = std::env::temp_dir().join(format!("tunekore-clap-module-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("Fake.clap");
        std::fs::write(&fake, b"not a library").unwrap();
        assert!(matches!(Module::load(&fake), Err(PluginError::Load { .. })));
        assert!(matches!(Module::load(&dir.join("Missing.clap")), Err(PluginError::Load { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
