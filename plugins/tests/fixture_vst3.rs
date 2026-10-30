//! End to end against the test plugin in `tests/fixture`: scan -> load -> activate -> process ->
//! parameters -> notes -> state.

mod common;

use common::{fixture_info, load_fixture};
use tunekore_plugins::*;

const FRAMES: usize = 64;

/// Runs one block through `plugin` with the given input (all channels the same) and returns
/// channel 0 of the output.
fn run(plugin: &mut dyn Plugin, input: f32, events: &[Event], changes: &[ParamChange]) -> Vec<f32> {
    let layout = plugin.layout().clone();
    let ins: Vec<Vec<f32>> = (0..layout.input_channels()).map(|_| vec![input; FRAMES]).collect();
    let in_refs: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
    let mut outs: Vec<Vec<f32>> = (0..layout.output_channels()).map(|_| vec![9.0; FRAMES]).collect();
    let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
    let (mut midi, mut params) = (OutBuf::with_capacity(16), OutBuf::with_capacity(16));
    let t = Transport::default();
    let mut block = ProcessBlock { frames: FRAMES, inputs: &in_refs, outputs: &mut out_refs, events, param_changes: changes, transport: &t, midi_out: &mut midi, param_out: &mut params };
    plugin.process(&mut block).expect("process");
    outs.swap_remove(0)
}

fn setup() -> ProcessSetup {
    ProcessSetup { max_block_size: FRAMES as u32, ..ProcessSetup::default() }
}

#[test]
fn the_bundle_lists_its_audio_classes_and_not_the_controller_class() {
    let found = vst3::classes(common::fixture_bundle()).unwrap();
    assert_eq!(found.len(), 3, "{found:?}");
    let g = found.iter().find(|d| d.name == "Fixture Gain").unwrap();
    assert_eq!((g.vendor.as_str(), g.version.as_str()), ("SoundCraft tests", "1.2.3"));
    assert_eq!((g.format, g.category, g.sub_categories.clone()), (Format::Vst3, Category::Effect, vec!["Fx".to_string()]));
    // `uid()` lays out the first 8 bytes differently on Windows (COM GUID order).
    assert_eq!(g.class_id, if cfg!(windows) { "001D0A5C111122223333444455556666" } else { "5C0A1D00111122223333444455556666" });
    assert!(g.path.ends_with("Fixture.vst3"));
    let s = found.iter().find(|d| d.name == "Fixture Synth").unwrap();
    assert_eq!(s.category, Category::Instrument);
}

#[test]
fn an_effect_has_buses_parameters_latency_and_changes_the_audio() {
    let mut c = load_fixture("Fixture Gain");
    let layout = c.default_layout().clone();
    assert_eq!((layout.audio_inputs.len(), layout.audio_outputs.len()), (1, 1));
    assert_eq!((layout.audio_inputs[0].channels, layout.audio_inputs[0].kind, layout.audio_inputs[0].active), (2, BusKind::Main, true));
    let mut ids: Vec<ParamId> = c.params().iter().map(|p| p.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![3, 7, 9, 11, 12, 13, 14], "hidden and bypass parameters are listed, flagged");
    let gain = c.params().iter().find(|p| p.id == 7).unwrap();
    assert_eq!((gain.name.as_str(), gain.step_count, gain.automatable, gain.hidden), ("Gain", 0, true, false));
    assert!(c.params().iter().find(|p| p.id == 9).unwrap().hidden);
    assert!(c.params().iter().find(|p| p.id == 11).unwrap().is_bypass);

    let mut p = c.activate(&setup()).unwrap();
    assert_eq!(p.layout().input_channels(), 2);
    assert_eq!(c.latency_samples(), 3);
    // The plugin's own state starts at gain 0.25 (normalized), which doubles to x0.5.
    assert_eq!(run(&mut *p, 0.25, &[], &[]), vec![0.125; FRAMES]);
    // A parameter change in the block reaches the plugin.
    let change = ParamChange { id: 7, offset: 0, value: 1.0 };
    assert_eq!(run(&mut *p, 0.25, &[], &[change]), vec![0.5; FRAMES]);
    // So does a change from the UI side.
    c.set_param(7, 0.25);
    assert_eq!(run(&mut *p, 0.5, &[], &[]), vec![0.25; FRAMES]);
    assert_eq!(c.param_value(7), Some(0.25));
    assert!(c.param_text(7, 0.25).is_some());
}

#[test]
fn an_instrument_plays_notes_at_the_sample_offsets_given() {
    let mut c = load_fixture("Fixture Synth");
    assert_eq!(c.default_layout().event_inputs, 1);
    assert_eq!(c.default_layout().audio_inputs.len(), 0);
    let mut p = c.activate(&setup()).unwrap();
    let on = Event { offset: 10, kind: EventKind::NoteOn { channel: 0, key: 60, velocity: 0.75, note_id: 1 } };
    let off = Event { offset: 40, kind: EventKind::NoteOff { channel: 0, key: 60, velocity: 0.0, note_id: 1 } };
    let out = run(&mut *p, 0.0, &[on, off], &[]);
    assert!(out[..10].iter().all(|&x| x == 0.0), "silent before the note");
    assert!(out[10..40].iter().all(|&x| x == 0.75), "the note's velocity from its offset");
    assert!(out[40..].iter().all(|&x| x == 0.0), "silent after the note off");
    // MIDI CC 7 reaches the plugin through its IMidiMapping.
    let cc = Event { offset: 5, kind: EventKind::ControlChange { channel: 0, controller: 7, value: 0.5 } };
    let out = run(&mut *p, 0.0, &[cc], &[]);
    assert!(out[..5].iter().all(|&x| x == 0.0) && out[5..].iter().all(|&x| x == 0.5), "{out:?}");
}

#[test]
fn a_block_that_does_not_fit_is_refused_and_a_replaced_processor_goes_stale() {
    let mut c = load_fixture("Fixture Gain");
    let mut p = c.activate(&setup()).unwrap();
    let t = Transport::default();
    let (mut midi, mut params) = (OutBuf::with_capacity(4), OutBuf::with_capacity(4));
    let mut outs = vec![vec![1.0f32; FRAMES]; 2];
    let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
    let mut block = ProcessBlock { frames: FRAMES, inputs: &[], outputs: &mut out_refs, events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    assert_eq!(p.process(&mut block), Err(ProcessError::BadBlock), "no input channels given");
    assert!(outs.iter().all(|c| c.iter().all(|&x| x == 0.0)), "outputs are silent after a refusal");

    let mut q = c.activate(&setup()).unwrap();
    assert_eq!(run(&mut *q, 0.5, &[], &[]), vec![0.25; FRAMES]);
    let ins = vec![vec![0.5f32; FRAMES]; 2];
    let in_refs: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
    let mut outs = vec![vec![1.0f32; FRAMES]; 2];
    let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
    let mut block = ProcessBlock { frames: FRAMES, inputs: &in_refs, outputs: &mut out_refs, events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    assert_eq!(p.process(&mut block), Err(ProcessError::Stale), "the first processor was replaced");
}

#[test]
fn state_saves_and_restores_and_a_garbage_blob_is_refused() {
    let mut c = load_fixture("Fixture Gain");
    let mut p = c.activate(&setup()).unwrap();
    // The plugin's state is what its processor has seen, so a block runs between a change and a save.
    c.set_param(7, 0.75);
    let _ = run(&mut *p, 0.0, &[], &[]);
    let blob = c.save_state().unwrap();
    c.set_param(7, 0.1);
    let _ = run(&mut *p, 0.0, &[], &[]);
    c.load_state(&blob).unwrap();
    assert_eq!(c.param_value(7), Some(0.75), "the controller shows the restored value");
    assert_eq!(run(&mut *p, 0.5, &[], &[]), vec![0.75; FRAMES], "and the processor uses it");
    assert!(c.load_state(b"").is_err());
    assert!(c.load_state(b"TKV3\xff\xff\xff\xffabc").is_err());
}

#[test]
fn loading_an_unknown_class_is_an_error() {
    let mut info = fixture_info("Fixture Gain");
    info.class_id = "00000000000000000000000000000000".into();
    assert!(matches!(vst3::load(&info), Err(PluginError::NotFound { .. })));
    info.class_id = "nope".into();
    assert!(matches!(vst3::load(&info), Err(PluginError::NotFound { .. })));
}
