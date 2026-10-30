//! Shared by the integration tests: builds the fixture plugin (a cdylib outside this workspace)
//! and installs it as a `.vst3` bundle in a temp folder.
//!
//! Derived from soundCraft's `vst3-host` tests (MIT OR Apache-2.0); see `third_party/soundcraft/`.

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
