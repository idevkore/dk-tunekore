//! Against the plugins installed on this Mac (the test list in `docs/PLAN.md` section 5). Each
//! test skips, passing, when `CI` is set or its plugin is not installed.

use std::path::PathBuf;
use tunekore_plugins::offline::{self, Input, RenderSpec};
use tunekore_plugins::*;

/// The installed bundle called `file`, unless running in CI or it is missing.
fn installed(file: &str) -> Option<PathBuf> {
    if std::env::var_os("CI").is_some() {
        eprintln!("skipped: CI is set");
        return None;
    }
    let mut dirs = vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Library/Audio/Plug-Ins/VST3"));
    }
    let found = dirs.into_iter().map(|d| d.join(file)).find(|p| p.exists());
    if found.is_none() {
        eprintln!("skipped: {file} is not installed");
    }
    found
}

fn load(bundle: &std::path::Path, name: &str) -> Box<dyn PluginController> {
    let info = vst3::classes(bundle).unwrap().into_iter().find(|i| i.name == name).unwrap_or_else(|| panic!("{name} is not in {}", bundle.display()));
    vst3::load(&info).unwrap_or_else(|e| panic!("load {name}: {e}"))
}

fn notes(seconds: f64) -> RenderSpec {
    RenderSpec { seconds, notes: RenderSpec::arpeggio(), ..RenderSpec::default() }
}

#[test]
fn serum_2_plays_notes_is_silent_without_them_and_restores_its_state_exactly() {
    let Some(bundle) = installed("Serum2.vst3") else { return };
    let mut a = load(&bundle, "Serum 2");
    let played = offline::render(&mut *a, &notes(2.0)).unwrap();
    assert!(played.peak() > 0.05, "peak {}", played.peak());
    let wav = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("serum2-notes.wav");
    wav::write_f32(&wav, 48_000, &played.channels).unwrap();

    let mut quiet = load(&bundle, "Serum 2");
    let silent = offline::render(&mut *quiet, &RenderSpec { notes: Vec::new(), ..notes(2.0) }).unwrap();
    assert_eq!(silent.peak(), 0.0, "no notes, no sound");

    let state = a.save_state().unwrap();
    assert!(state.len() > 100);
    let mut b = load(&bundle, "Serum 2");
    b.load_state(&state).unwrap();
    let again = offline::render(&mut *b, &notes(2.0)).unwrap();
    assert!(offline::max_difference(&played, &again) < 1e-4, "restored render differs by {}", offline::max_difference(&played, &again));
}

#[test]
fn serum_plays_notes() {
    let Some(bundle) = installed("Serum.vst3") else { return };
    let mut c = load(&bundle, "Serum");
    let r = offline::render(&mut *c, &notes(1.0)).unwrap();
    assert!(r.peak() > 0.05, "peak {}", r.peak());
}

#[test]
fn analog_lab_v_loads_or_fails_with_an_error_but_never_aborts() {
    let Some(bundle) = installed("Analog Lab V.vst3") else { return };
    // In the hosting spike this plugin threw a C++ exception into Rust and the process aborted.
    // Whatever its licence state on this Mac, getting here and returning is the test.
    match vst3::classes(&bundle).map(|v| v.into_iter().next()) {
        Ok(Some(info)) => match vst3::load(&info) {
            Ok(mut c) => {
                let r = offline::render(&mut *c, &notes(1.0));
                assert!(r.is_ok(), "{:?}", r.err());
            }
            Err(e) => eprintln!("Analog Lab V refused to load, cleanly: {e}"),
        },
        Ok(None) => eprintln!("Analog Lab V lists no plugin"),
        Err(e) => eprintln!("Analog Lab V did not load, cleanly: {e}"),
    }
}

#[test]
fn jup_8_v4_plays_notes() {
    let Some(bundle) = installed("Jup-8 V4.vst3") else { return };
    let mut c = load(&bundle, "Jup-8 V4");
    let r = offline::render(&mut *c, &notes(1.0)).unwrap();
    assert!(r.peak() > 0.01, "peak {}", r.peak());
}

#[test]
fn kontakt_7_runs_with_all_its_output_buses() {
    let Some(bundle) = installed("Kontakt 7.vst3") else { return };
    let mut c = load(&bundle, "Kontakt 7");
    assert!(c.default_layout().audio_outputs.len() > 1, "Kontakt has extra outputs");
    let spec = RenderSpec { outputs: BusSelection::All, ..notes(1.0) };
    let r = offline::render(&mut *c, &spec).unwrap();
    assert_eq!(r.layout.audio_outputs.len(), c.default_layout().audio_outputs.len());
    assert!(r.layout.audio_outputs.iter().all(|b| b.active));
    assert_eq!(r.channels.len(), r.layout.output_channels());
    assert!(r.channels.len() > 2);
    assert!(r.channels.iter().all(|ch| ch.len() == 48_000));
}

#[test]
fn guitar_rig_7_processes_audio_as_an_effect() {
    let Some(bundle) = installed("Guitar Rig 7.vst3") else { return };
    let mut c = load(&bundle, "Guitar Rig 7");
    assert_eq!(c.info().category, Category::Effect);
    let spec = RenderSpec { seconds: 1.0, input: Input::Sine { hz: 220.0, level: 0.25 }, ..RenderSpec::default() };
    let r = offline::render(&mut *c, &spec).unwrap();
    assert_eq!(r.layout.input_channels(), 2, "main input on, sidechain off");
    assert!(r.peak() > 0.01, "peak {}", r.peak());
}
