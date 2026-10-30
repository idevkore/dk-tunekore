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
    let mut outs: Vec<Vec<f32>> = (0..layout.output_channels()).map(|_| vec![9.0; FRAMES]).collect();
    let mut table = ChannelTable::default();
    let (inputs, outputs) = table.audio(&ins, &mut outs);
    let (mut midi, mut params) = (OutBuf::with_capacity(16), OutBuf::with_capacity(16));
    let t = Transport::default();
    let mut block = ProcessBlock { frames: FRAMES, inputs, outputs, events, param_changes: changes, transport: &t, midi_out: &mut midi, param_out: &mut params };
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
    assert_eq!((gain.units.as_str(), gain.min, gain.max, gain.default), ("dB", 0.0, 2.0, 0.5), "range in the plugin's own units");
    let freq = c.params().iter().find(|p| p.id == 12).unwrap();
    assert!((freq.min - 20.0).abs() < 1e-6 && (freq.max - 20_000.0).abs() < 1e-3, "{freq:?}");
    let level = c.params().iter().find(|p| p.id == 13).unwrap();
    assert_eq!((level.min, level.max), (0.0, 1.0), "no mapping: 0 to 1, and the text has the value");
    let mode = c.params().iter().find(|p| p.id == 3).unwrap();
    assert_eq!((mode.step_count, mode.min, mode.max), (2, 0.0, 2.0), "a three-way choice");
    assert!(c.params().iter().find(|p| p.id == 9).unwrap().hidden);
    assert!(c.params().iter().find(|p| p.id == 11).unwrap().is_bypass);

    let mut p = c.activate(&setup()).unwrap();
    assert_eq!(p.layout().input_channels(), 2);
    assert_eq!(c.latency_samples(), 3);
    assert_eq!(p.latency_samples(), 3, "the processor carries the latency it was activated with");
    // The fixture's controller makes an edit as soon as it is loaded (gain 0.9, as if the user
    // had turned the knob). There was no processor yet; the edit waited and arrives with the first
    // block: x1.8.
    assert!(c.has_pending_params());
    assert_eq!(run(&mut *p, 0.25, &[], &[]), vec![0.45; FRAMES]);
    assert!(!c.has_pending_params());
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
    let mut table = ChannelTable::default();
    let none: [Vec<f32>; 0] = [];
    let (inputs, outputs) = table.audio(&none, &mut outs);
    let mut block = ProcessBlock { frames: FRAMES, inputs, outputs, events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    assert_eq!(p.process(&mut block), Err(ProcessError::BadBlock), "no input channels given");
    assert!(outs.iter().all(|c| c.iter().all(|&x| x == 0.0)), "outputs are silent after a refusal");

    let mut q = c.activate(&setup()).unwrap();
    // The refused block did not take the edit made at load; the new processor gets it.
    assert_eq!(run(&mut *q, 0.5, &[], &[]), vec![0.9; FRAMES]);
    let ins = vec![vec![0.5f32; FRAMES]; 2];
    let mut outs = vec![vec![1.0f32; FRAMES]; 2];
    let (inputs, outputs) = table.audio(&ins, &mut outs);
    let mut block = ProcessBlock { frames: FRAMES, inputs, outputs, events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
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

#[test]
fn an_event_past_the_end_of_the_block_lands_on_its_last_frame() {
    let mut c = load_fixture("Fixture Synth");
    let mut p = c.activate(&setup()).unwrap();
    let late = Event { offset: 10_000, kind: EventKind::NoteOn { channel: 0, key: 60, velocity: 0.5, note_id: 1 } };
    let out = run(&mut *p, 0.0, &[late], &[]);
    assert!(out[..FRAMES - 1].iter().all(|&x| x == 0.0));
    assert_eq!(out[FRAMES - 1], 0.5);
}

#[test]
fn edits_wait_through_deactivation_and_a_zero_frame_block_delivers_them() {
    let mut c = load_fixture("Fixture Gain");
    let mut p = c.activate(&setup()).unwrap();
    let _ = run(&mut *p, 0.0, &[], &[]);
    c.deactivate();
    // Set while there is no processor: nothing is lost.
    c.set_param(7, 0.25);
    c.set_param(7, 0.75);
    assert!(c.has_pending_params());
    let mut p = c.activate(&setup()).unwrap();
    // A zero-frame block (what a stopped engine runs) delivers it, and only the latest value.
    let t = Transport::default();
    let (mut midi, mut params) = (OutBuf::with_capacity(4), OutBuf::with_capacity(4));
    let mut block = ProcessBlock { frames: 0, inputs: AudioIn::none(), outputs: AudioOut::none(), events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    // The block's channel counts must still match the layout; a zero-frame flush passes none.
    assert_eq!(p.process(&mut block), Err(ProcessError::BadBlock));
    let (ins, mut outs) = (vec![Vec::<f32>::new(); 2], vec![Vec::<f32>::new(); 2]);
    let mut table = ChannelTable::default();
    let (inputs, outputs) = table.audio(&ins, &mut outs);
    let mut block = ProcessBlock { frames: 0, inputs, outputs, events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    p.process(&mut block).unwrap();
    assert!(!c.has_pending_params());
    // The processor has it, so the saved state has it.
    let state = c.save_state().unwrap();
    let mut fresh = load_fixture("Fixture Gain");
    fresh.load_state(&state).unwrap();
    assert_eq!(fresh.param_value(7), Some(0.75));
}

#[test]
fn a_stale_processor_cannot_unblock_a_live_one() {
    // Two processors from two activations; the old one going stale must not disturb the new one.
    let mut c = load_fixture("Fixture Gain");
    let mut old = c.activate(&setup()).unwrap();
    let mut new = c.activate(&setup()).unwrap();
    let _ = run(&mut *new, 0.0, &[], &[]);
    let t = Transport::default();
    let (mut midi, mut params) = (OutBuf::with_capacity(4), OutBuf::with_capacity(4));
    let (ins, mut outs) = (vec![vec![0.5f32; FRAMES]; 2], vec![vec![0.0f32; FRAMES]; 2]);
    let mut table = ChannelTable::default();
    let (inputs, outputs) = table.audio(&ins, &mut outs);
    let mut block = ProcessBlock { frames: FRAMES, inputs, outputs, events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    assert_eq!(old.process(&mut block), Err(ProcessError::Stale));
    assert!(run(&mut *new, 0.5, &[], &[]).iter().all(|&x| x > 0.0));
    c.deactivate();
    drop(new);
    drop(old);
}

#[test]
fn buses_can_be_chosen_one_by_one() {
    let mut c = load_fixture("Fixture Gain");
    let only_out = ProcessSetup { inputs: BusSelection::Only(vec![]), outputs: BusSelection::Only(vec![0]), ..setup() };
    let p = c.activate(&only_out).unwrap();
    assert_eq!((p.layout().input_channels(), p.layout().output_channels()), (0, 2));
    assert!(!p.layout().audio_inputs[0].active && p.layout().audio_outputs[0].active);
    assert!(BusSelection::Only(vec![2, 5]).includes(5, 0) && !BusSelection::Only(vec![2, 5]).includes(0, 0));
}

#[test]
fn loading_a_state_discards_values_set_before_it() {
    let mut c = load_fixture("Fixture Gain");
    let mut p = c.activate(&setup()).unwrap();
    c.set_param(7, 0.75);
    let _ = run(&mut *p, 0.0, &[], &[]);
    let preset = c.save_state().unwrap();
    // The user turns the knob while nothing processes, then loads the preset.
    c.set_param(7, 0.1);
    assert!(c.has_pending_params());
    c.load_state(&preset).unwrap();
    assert!(!c.has_pending_params(), "the old knob value was dropped");
    assert_eq!(run(&mut *p, 0.5, &[], &[]), vec![0.75; FRAMES], "the processor plays the preset, not the old knob");
    assert_eq!(c.param_value(7), Some(0.75));
}

#[test]
fn a_plugin_with_no_audio_outputs_still_processes() {
    // Fixture Thru with its audio outputs off: like a MIDI-only plugin, it only sends notes.
    let mut c = load_fixture("Fixture Thru");
    let midi_only = ProcessSetup { outputs: BusSelection::Only(vec![]), ..setup() };
    let mut p = c.activate(&midi_only).unwrap();
    assert_eq!(p.layout().output_channels(), 0);
    let ins = vec![vec![0.5f32; FRAMES]; 2];
    let mut table = ChannelTable::default();
    let mut none: [Vec<f32>; 0] = [];
    let (inputs, outputs) = table.audio(&ins, &mut none);
    let note = [Event { offset: 3, kind: EventKind::NoteOn { channel: 0, key: 60, velocity: 0.5, note_id: 1 } }];
    let t = Transport::default();
    let (mut midi, mut params) = (OutBuf::with_capacity(4), OutBuf::with_capacity(4));
    let mut block = ProcessBlock { frames: FRAMES, inputs, outputs, events: &note, param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    p.process(&mut block).unwrap();
    assert_eq!(midi.as_slice(), &note, "the note came back out");
    // The same through AudioOut::none().
    let (inputs, _) = table.audio(&ins, &mut none);
    let mut block = ProcessBlock { frames: FRAMES, inputs, outputs: AudioOut::none(), events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    p.process(&mut block).unwrap();
}

#[test]
fn more_waiting_values_than_parameter_queues_arrive_over_two_blocks() {
    let mut c = load_fixture("Fixture Thru");
    let mut p = c.activate(&setup()).unwrap();
    // 300 values waiting; a block has queues for 256 parameters.
    for id in 0..300u32 {
        c.set_param(10_000 + id, 0.5);
    }
    let _ = run(&mut *p, 0.0, &[], &[]);
    assert!(c.has_pending_params(), "the 44 that did not fit are still waiting");
    let _ = run(&mut *p, 0.0, &[], &[]);
    assert!(!c.has_pending_params());
    assert_eq!(p.take_dropped().param_points, 0, "nothing was dropped on the way in");
}
