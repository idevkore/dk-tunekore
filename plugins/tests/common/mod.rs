//! Shared by the integration tests: builds the fixture plugins (cdylibs outside this workspace)
//! and installs them as a `.vst3` and a `.clap` in temp folders, and builds the misbehaving C++
//! plugins.
//!
//! Derived from soundCraft's `vst3-host` tests (MIT OR Apache-2.0); see `third_party/soundcraft/`.
//! The CLAP parts are ours.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use tunekore_plugins::{PluginController, PluginInfo};

/// Builds the fixture once and returns the installed `Fixture.vst3` bundle.
pub fn fixture_bundle() -> &'static Path {
    static BUNDLE: OnceLock<PathBuf> = OnceLock::new();
    BUNDLE.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture/Cargo.toml");
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("vst3-fixture-target");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let st = Command::new(cargo)
            .args(["build", "--quiet", "--locked", "--manifest-path"])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&target)
            .env_remove("RUSTFLAGS")
            .status()
            .expect("run cargo");
        assert!(st.success(), "building the VST3 fixture failed");
        let (lib, folder, exe) = if cfg!(target_os = "macos") {
            ("libvst3_fixture.dylib", "MacOS", "Fixture")
        } else if cfg!(windows) {
            ("vst3_fixture.dll", "x86_64-win", "Fixture.vst3")
        } else {
            ("libvst3_fixture.so", if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" }, "Fixture.so")
        };
        let built = target.join("debug").join(lib);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("vst3-fixture-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Installed as a real bundle, to exercise bundle resolution and the module entry.
        let bundle = dir.join("vendor/Fixture.vst3");
        let bin = bundle.join("Contents").join(folder);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::copy(&built, bin.join(exe)).unwrap();
        bundle
    })
}

/// The fixture's plugin called `name`.
pub fn fixture_info(name: &str) -> PluginInfo {
    tunekore_plugins::vst3::classes(fixture_bundle()).expect("read the fixture").into_iter().find(|i| i.name == name).unwrap_or_else(|| panic!("no fixture plugin {name}"))
}

pub fn load_fixture(name: &str) -> Box<dyn PluginController> {
    tunekore_plugins::vst3::load(&fixture_info(name)).unwrap_or_else(|e| panic!("load {name}: {e}"))
}

/// The ways the C++ test plugin (`tests/bad_plugin`) misbehaves.
#[derive(Clone, Copy)]
pub enum Misbehaviour {
    /// `initialize` throws a C++ exception.
    ThrowsInInitialize = 0,
    /// `GetPluginFactory` aborts the process.
    AbortsOnLoad = 1,
    /// `GetPluginFactory` never returns.
    HangsOnLoad = 2,
    /// `GetPluginFactory` throws a C++ exception.
    ThrowsOnLoad = 3,
    /// Loads and runs as an instrument until `process` throws.
    ThrowsInProcess = 4,
}

/// Compiles the misbehaving plugin and installs it as `<dir>/<name>.vst3`. `None` when there is no
/// C++ compiler (the test then skips).
pub fn bad_plugin(dir: &Path, name: &str, mode: Misbehaviour) -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    let bundle = dir.join(format!("{name}.vst3"));
    let (folder, exe) = if cfg!(target_os = "macos") { ("MacOS", name.to_string()) } else { (if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" }, format!("{name}.so")) };
    let bin = bundle.join("Contents").join(folder);
    std::fs::create_dir_all(&bin).ok()?;
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/bad_plugin/bad_plugin.cpp");
    let shared = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let st = Command::new("c++").args(["-std=c++17", shared, "-fPIC", "-fvisibility=hidden", &format!("-DMODE={}", mode as i32)]).arg(&src).arg("-o").arg(bin.join(exe)).status().ok()?;
    st.success().then_some(bundle)
}

/// A fresh empty folder under the test target directory.
pub fn scratch_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---- CLAP ----------------------------------------------------------------------------------

/// Builds the CLAP fixture (`tests/clap_fixture`) once and returns the installed `Fixture.clap`:
/// a bundle on macOS (`Contents/MacOS/Fixture`), a single file elsewhere, as CLAP has them.
pub fn clap_fixture_bundle() -> &'static Path {
    static BUNDLE: OnceLock<PathBuf> = OnceLock::new();
    BUNDLE.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/clap_fixture/Cargo.toml");
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("clap-fixture-target");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let st = Command::new(cargo)
            .args(["build", "--quiet", "--locked", "--manifest-path"])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&target)
            .env_remove("RUSTFLAGS")
            .status()
            .expect("run cargo");
        assert!(st.success(), "building the CLAP fixture failed");
        let lib = if cfg!(target_os = "macos") {
            "libclap_fixture.dylib"
        } else if cfg!(windows) {
            "clap_fixture.dll"
        } else {
            "libclap_fixture.so"
        };
        let built = target.join("debug").join(lib);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("clap-fixture-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bundle = dir.join("vendor/Fixture.clap");
        if cfg!(target_os = "macos") {
            let bin = bundle.join("Contents/MacOS");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::copy(&built, bin.join("Fixture")).unwrap();
        } else {
            std::fs::create_dir_all(dir.join("vendor")).unwrap();
            std::fs::copy(&built, &bundle).unwrap();
        }
        bundle
    })
}

/// The CLAP fixture's plugin called `name`.
pub fn clap_fixture_info(name: &str) -> PluginInfo {
    tunekore_plugins::clap::classes(clap_fixture_bundle()).expect("read the CLAP fixture").into_iter().find(|i| i.name == name).unwrap_or_else(|| panic!("no CLAP fixture plugin {name}"))
}

pub fn load_clap_fixture(name: &str) -> Box<dyn PluginController> {
    tunekore_plugins::clap::load(&clap_fixture_info(name)).unwrap_or_else(|e| panic!("load {name}: {e}"))
}

/// The ways the C++ CLAP test plugin (`tests/bad_clap`) misbehaves.
#[derive(Clone, Copy)]
pub enum ClapMisbehaviour {
    /// `clap_plugin.init` throws a C++ exception.
    ThrowsInInit = 0,
    /// `clap_entry.init` aborts the process.
    AbortsOnLoad = 1,
    /// `clap_entry.init` never returns.
    HangsOnLoad = 2,
    /// `clap_entry.init` throws a C++ exception.
    ThrowsOnLoad = 3,
    /// Loads and runs as an instrument until `process` throws.
    ThrowsInProcess = 4,
}

/// Compiles the misbehaving CLAP plugin and installs it as `<dir>/<name>.clap`. `None` when there
/// is no C++ compiler (the test then skips).
pub fn bad_clap(dir: &Path, name: &str, mode: ClapMisbehaviour) -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    let bundle = dir.join(format!("{name}.clap"));
    let out = if cfg!(target_os = "macos") {
        let bin = bundle.join("Contents/MacOS");
        std::fs::create_dir_all(&bin).ok()?;
        bin.join(name)
    } else {
        bundle.clone()
    };
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/bad_clap/bad_clap.cpp");
    let shared = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let st = Command::new("c++").args(["-std=c++17", shared, "-fPIC", "-fvisibility=hidden", &format!("-DMODE={}", mode as i32)]).arg(&src).arg("-o").arg(&out).status().ok()?;
    st.success().then_some(bundle)
}
