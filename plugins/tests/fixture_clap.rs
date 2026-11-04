//! End to end against the CLAP test plugins in `tests/clap_fixture`: list -> load -> activate ->
//! process -> parameters -> notes -> state, through the same traits as VST3.

mod common;

use common::{clap_fixture_bundle, clap_fixture_info, load_clap_fixture};
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

/// A zero-frame block: what a stopped engine runs to hand over parameter values.
fn flush(plugin: &mut dyn Plugin) -> Result<(), ProcessError> {
    let layout = plugin.layout().clone();
    let (ins, mut outs) = (vec![Vec::<f32>::new(); layout.input_channels()], vec![Vec::<f32>::new(); layout.output_channels()]);
    let mut table = ChannelTable::default();
    let (inputs, outputs) = table.audio(&ins, &mut outs);
    let (mut midi, mut params) = (OutBuf::with_capacity(4), OutBuf::with_capacity(4));
    let t = Transport::default();
    let mut block = ProcessBlock { frames: 0, inputs, outputs, events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    plugin.process(&mut block)
}

fn setup() -> ProcessSetup {
    ProcessSetup { max_block_size: FRAMES as u32, ..ProcessSetup::default() }
}

/// One of the Gain's read-only counters (20 timer ticks, 21 main-thread calls, 22 thread errors).
fn counter(c: &mut dyn PluginController, id: ParamId) -> u32 {
    let n = c.param_value(id).expect("a counter");
    c.param_plain(id, n).unwrap().round() as u32
}

#[test]
fn the_file_lists_its_plugins_with_their_features() {
    let found = clap::classes(clap_fixture_bundle()).unwrap();
    assert_eq!(found.len(), 3, "{found:?}");
    let g = found.iter().find(|d| d.name == "Fixture CLAP Gain").unwrap();
    assert_eq!((g.vendor.as_str(), g.version.as_str()), ("tuneKore tests", "1.2.3"));
    assert_eq!((g.format, g.category), (Format::Clap, Category::Effect));
    assert_eq!(g.sub_categories, vec!["audio-effect".to_string(), "stereo".to_string()]);
    assert_eq!(g.class_id, "dev.tunekore.fixture.gain");
    assert!(g.path.ends_with("Fixture.clap"));
    let s = found.iter().find(|d| d.name == "Fixture CLAP Synth").unwrap();
    assert_eq!(s.category, Category::Instrument);
    // The crate's own entry points pick the CLAP host from the format.
    assert_eq!(classes(Format::Clap, clap_fixture_bundle()).unwrap(), found);
    assert!(load(s).is_ok());
}

#[test]
fn an_effect_has_ports_parameters_latency_and_changes_the_audio() {
    let mut c = load_clap_fixture("Fixture CLAP Gain");
    let layout = c.default_layout().clone();
    assert_eq!((layout.audio_inputs.len(), layout.audio_outputs.len(), layout.event_inputs), (2, 1, 0));
    assert_eq!((layout.audio_inputs[0].name.as_str(), layout.audio_inputs[0].channels, layout.audio_inputs[0].kind, layout.audio_inputs[0].active), ("Main In", 2, BusKind::Main, true));
    assert_eq!((layout.audio_inputs[1].name.as_str(), layout.audio_inputs[1].kind), ("Sidechain", BusKind::Aux));
    let ids: Vec<ParamId> = c.params().iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![7, 3, 9, 11, 12, 20, 21, 22], "in the plugin's order, hidden and read-only ones included");
    let gain = c.params().iter().find(|p| p.id == 7).unwrap().clone();
    assert_eq!((gain.name.as_str(), gain.step_count, gain.automatable, gain.hidden, gain.read_only), ("Gain", 0, true, false, false));
    assert_eq!((gain.min, gain.max, gain.default), (0.0, 2.0, 0.5), "range in the plugin's own units, default normalized");
    let mode = c.params().iter().find(|p| p.id == 3).unwrap();
    assert_eq!((mode.step_count, mode.min, mode.max), (2, 0.0, 2.0), "a three-way choice");
    assert!(c.params().iter().find(|p| p.id == 9).unwrap().hidden);
    assert!(c.params().iter().find(|p| p.id == 11).unwrap().is_bypass);
    assert!(c.params().iter().find(|p| p.id == 22).unwrap().read_only);
    let freq = c.params().iter().find(|p| p.id == 12).unwrap();
    assert_eq!((freq.min, freq.max), (20.0, 20_000.0));

    let mut p = c.activate(&setup()).unwrap();
    // The main ports only: the sidechain is off, and the plugin gets silence there (it checks).
    assert_eq!((p.layout().input_channels(), p.layout().output_channels()), (2, 2));
    assert!(!p.layout().audio_inputs[1].active);
    assert_eq!(c.latency_samples(), 3);
    assert_eq!(p.latency_samples(), 3, "the processor carries the latency it was activated with");
    assert_eq!(c.tail_samples(), 100);
    assert_eq!(run(&mut *p, 0.25, &[], &[]), vec![0.25; FRAMES], "gain 1 by default");
    // A parameter change in the block reaches the plugin, in its own units (1.0 is gain 2).
    let change = ParamChange { id: 7, offset: 0, value: 1.0 };
    assert_eq!(run(&mut *p, 0.25, &[], &[change]), vec![0.5; FRAMES]);
    // So does a change from the UI side, at the next block.
    c.set_param(7, 0.25);
    assert!(c.has_pending_params());
    assert_eq!(c.param_value(7), Some(0.25), "the value about to reach the plugin");
    assert_eq!(run(&mut *p, 0.5, &[], &[]), vec![0.25; FRAMES]);
    assert!(!c.has_pending_params());
    assert_eq!(c.param_value(7), Some(0.25), "and the plugin has it");
    assert_eq!(c.param_text(7, 0.25).as_deref(), Some("x0.50"), "the plugin's own text, for the plain value");
    assert_eq!(c.param_text(3, 0.5).as_deref(), Some("B"));
    assert_eq!(c.param_plain(12, 1.0), Some(20_000.0));
    assert_eq!(c.param_plain(99, 1.0), None, "not one of its parameters");
    // A ramp inside the block: gain 0 from frame 0, gain 2 from frame 32.
    let ramp = [ParamChange { id: 7, offset: 0, value: 0.0 }, ParamChange { id: 7, offset: 32, value: 1.0 }];
    let out = run(&mut *p, 0.5, &[], &ramp);
    assert!(out[..32].iter().all(|&x| x == 0.0) && out[32..].iter().all(|&x| x == 1.0), "sample-accurate: {out:?}");
    assert_eq!(counter(&mut *c, 22), 0, "every call came on the thread CLAP says");
}

#[test]
fn an_instrument_plays_notes_at_the_sample_offsets_given() {
    let mut c = load_clap_fixture("Fixture CLAP Synth");
    assert_eq!(c.default_layout().event_inputs, 1);
    assert_eq!(c.default_layout().audio_inputs.len(), 0);
    let mut p = c.activate(&setup()).unwrap();
    let on = Event { offset: 10, kind: EventKind::NoteOn { channel: 0, key: 60, velocity: 0.75, note_id: 1 } };
    let off = Event { offset: 40, kind: EventKind::NoteOff { channel: 0, key: 60, velocity: 0.0, note_id: 1 } };
    let out = run(&mut *p, 0.0, &[on, off], &[]);
    assert!(out[..10].iter().all(|&x| x == 0.0), "silent before the note");
    assert!(out[10..40].iter().all(|&x| x == 0.75), "the note's velocity from its offset (a CLAP note)");
    assert!(out[40..].iter().all(|&x| x == 0.0), "silent after the note off");
    // MIDI CC 7 reaches it as a MIDI message: 0.5 is 64 of 127.
    let cc = Event { offset: 5, kind: EventKind::ControlChange { channel: 0, controller: 7, value: 0.5 } };
    let out = run(&mut *p, 0.0, &[cc], &[]);
    let level = 64.0f32 / 127.0;
    assert!(out[..5].iter().all(|&x| x == 0.0) && out[5..].iter().all(|&x| x == level), "{out:?}");
    // Its volume parameter scales it.
    let half = ParamChange { id: 7, offset: 0, value: 0.5 };
    assert!(run(&mut *p, 0.0, &[], &[half]).iter().all(|&x| x == level * 0.5));
}

#[test]
fn a_block_that_does_not_fit_is_refused_and_a_replaced_processor_goes_stale() {
    let mut c = load_clap_fixture("Fixture CLAP Gain");
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
    let too_long = vec![vec![0.5f32; FRAMES * 2]; 2];
    let mut long_out = vec![vec![0.0f32; FRAMES * 2]; 2];
    let (inputs, outputs) = table.audio(&too_long, &mut long_out);
    let mut block = ProcessBlock { frames: FRAMES * 2, inputs, outputs, events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    assert_eq!(p.process(&mut block), Err(ProcessError::BadBlock), "more frames than it was activated for");

    let mut q = c.activate(&setup()).unwrap();
    assert_eq!(run(&mut *q, 0.5, &[], &[]), vec![0.5; FRAMES]);
    let ins = vec![vec![0.5f32; FRAMES]; 2];
    let mut outs = vec![vec![1.0f32; FRAMES]; 2];
    let (inputs, outputs) = table.audio(&ins, &mut outs);
    let mut block = ProcessBlock { frames: FRAMES, inputs, outputs, events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    assert_eq!(p.process(&mut block), Err(ProcessError::Stale), "the first processor was replaced");
}

#[test]
fn state_saves_and_restores_and_a_garbage_blob_is_refused() {
    let mut c = load_clap_fixture("Fixture CLAP Gain");
    let mut p = c.activate(&setup()).unwrap();
    c.set_param(7, 0.75);
    let _ = run(&mut *p, 0.0, &[], &[]);
    let blob = c.save_state().unwrap();
    assert_eq!(&blob[..4], b"FXCG", "the plugin's own state, as it wrote it");
    c.set_param(7, 0.1);
    let _ = run(&mut *p, 0.0, &[], &[]);
    let mut events = Vec::new();
    c.poll(&mut events);
    c.load_state(&blob).unwrap();
    assert_eq!(c.param_value(7), Some(0.75), "the plugin shows the restored value");
    assert_eq!(run(&mut *p, 0.5, &[], &[]), vec![0.75; FRAMES], "and plays it");
    // It told the host its values changed.
    events.clear();
    c.poll(&mut events);
    assert!(events.contains(&ControllerEvent::ParamsChanged), "{events:?}");
    assert!(c.load_state(b"").is_err());
    assert!(c.load_state(b"FXCG\xff\xff").is_err());
    assert_eq!(c.param_value(7), Some(0.75), "a refused state changes nothing");
}

#[test]
fn a_plugin_without_state_saves_nothing_and_takes_nothing() {
    let mut c = load_clap_fixture("Fixture CLAP Synth");
    assert_eq!(c.save_state().unwrap(), Vec::<u8>::new());
    assert!(c.load_state(&[]).is_ok());
    assert!(matches!(c.load_state(b"something"), Err(PluginError::Unsupported(_))));
}

#[test]
fn loading_an_unknown_plugin_is_an_error() {
    let mut info = clap_fixture_info("Fixture CLAP Gain");
    info.class_id = "dev.tunekore.nothing".into();
    assert!(matches!(clap::load(&info), Err(PluginError::NotFound { .. })));
    let mut vst = clap_fixture_info("Fixture CLAP Gain");
    vst.format = Format::Vst3;
    assert!(matches!(clap::load(&vst), Err(PluginError::Unsupported(_))));
    assert!(clap::load_class(clap_fixture_bundle(), "dev.tunekore.fixture.thru").is_ok());
}

#[test]
fn an_event_past_the_end_of_the_block_lands_on_its_last_frame() {
    let mut c = load_clap_fixture("Fixture CLAP Synth");
    let mut p = c.activate(&setup()).unwrap();
    let late = Event { offset: 10_000, kind: EventKind::NoteOn { channel: 0, key: 60, velocity: 0.5, note_id: 1 } };
    let out = run(&mut *p, 0.0, &[late], &[]);
    assert!(out[..FRAMES - 1].iter().all(|&x| x == 0.0));
    assert_eq!(out[FRAMES - 1], 0.5);
}

#[test]
fn values_set_while_the_plugin_is_off_reach_it_at_once_and_a_zero_frame_block_delivers_the_rest() {
    let mut c = load_clap_fixture("Fixture CLAP Gain");
    let mut p = c.activate(&setup()).unwrap();
    let _ = run(&mut *p, 0.0, &[], &[]);
    c.deactivate();
    // Off: CLAP flushes parameters on the main thread, so nothing waits.
    c.set_param(7, 0.25);
    c.set_param(7, 0.75);
    assert!(!c.has_pending_params());
    assert_eq!(c.param_value(7), Some(0.75));
    // On, with nothing processing: a zero-frame block flushes them.
    let mut p = c.activate(&setup()).unwrap();
    c.set_param(7, 0.5);
    assert!(c.has_pending_params());
    flush(&mut *p).unwrap();
    assert!(!c.has_pending_params());
    let state = c.save_state().unwrap();
    let mut fresh = load_clap_fixture("Fixture CLAP Gain");
    fresh.load_state(&state).unwrap();
    assert_eq!(fresh.param_value(7), Some(0.5));
    assert_eq!(counter(&mut *c, 22), 0, "flushes came on the right threads");
}

#[test]
fn a_stale_processor_cannot_unblock_a_live_one() {
    let mut c = load_clap_fixture("Fixture CLAP Gain");
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
fn ports_can_be_chosen_one_by_one_and_ports_that_are_off_are_silent() {
    let mut c = load_clap_fixture("Fixture CLAP Gain");
    let only_out = ProcessSetup { inputs: BusSelection::Only(vec![]), outputs: BusSelection::Only(vec![0]), ..setup() };
    let mut p = c.activate(&only_out).unwrap();
    assert_eq!((p.layout().input_channels(), p.layout().output_channels()), (0, 2));
    // Both inputs are off: the plugin gets silence, marked as such, and makes silence of it.
    assert_eq!(run(&mut *p, 0.0, &[], &[]), vec![0.0; FRAMES]);
    let mut p = c.activate(&ProcessSetup { inputs: BusSelection::All, ..setup() }).unwrap();
    assert_eq!(p.layout().input_channels(), 4, "main and sidechain");
    assert_eq!(run(&mut *p, 0.5, &[], &[]), vec![0.5; FRAMES]);
}

#[test]
fn an_input_that_is_also_an_output_is_read_before_it_is_written() {
    let mut c = load_clap_fixture("Fixture CLAP Gain");
    let mut p = c.activate(&setup()).unwrap();
    let mut bufs = vec![vec![0.5f32; FRAMES]; 2];
    let mut table = ChannelTable::default();
    let (inputs, outputs) = table.in_place(&mut bufs, 2);
    let (mut midi, mut params) = (OutBuf::with_capacity(4), OutBuf::with_capacity(4));
    let t = Transport::default();
    let change = [ParamChange { id: 7, offset: 0, value: 1.0 }];
    let mut block = ProcessBlock { frames: FRAMES, inputs, outputs, events: &[], param_changes: &change, transport: &t, midi_out: &mut midi, param_out: &mut params };
    p.process(&mut block).unwrap();
    assert_eq!(bufs, vec![vec![1.0f32; FRAMES]; 2], "x2, in place");
}

#[test]
fn a_plugin_with_no_audio_outputs_still_processes() {
    // The Thru with its outputs off: like a MIDI-only plugin, it only sends notes.
    let mut c = load_clap_fixture("Fixture CLAP Thru");
    let mut p = c.activate(&ProcessSetup { outputs: BusSelection::Only(vec![]), ..setup() }).unwrap();
    assert_eq!(p.layout().output_channels(), 0);
    let ins = vec![vec![0.5f32; FRAMES]; 2];
    let mut table = ChannelTable::default();
    let mut none: [Vec<f32>; 0] = [];
    let (inputs, outputs) = table.audio(&ins, &mut none);
    let note = [Event { offset: 3, kind: EventKind::NoteOn { channel: 1, key: 60, velocity: 0.5, note_id: 1 } }];
    let t = Transport::default();
    let (mut midi, mut params) = (OutBuf::with_capacity(4), OutBuf::with_capacity(4));
    let mut block = ProcessBlock { frames: FRAMES, inputs, outputs, events: &note, param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    p.process(&mut block).unwrap();
    // It takes MIDI only, so the note went there and back as MIDI: 7-bit velocity, no note id.
    match midi.as_slice() {
        [Event { offset: 3, kind: EventKind::NoteOn { channel: 1, key: 60, velocity, note_id: -1 } }] => assert!((velocity - 0.5).abs() < 0.01),
        other => panic!("{other:?}"),
    }
    let (inputs, _) = table.audio(&ins, &mut none);
    let mut block = ProcessBlock { frames: FRAMES, inputs, outputs: AudioOut::none(), events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    p.process(&mut block).unwrap();
}

#[test]
fn more_waiting_values_than_a_block_takes_arrive_over_two_blocks() {
    let mut c = load_clap_fixture("Fixture CLAP Thru");
    let mut p = c.activate(&setup()).unwrap();
    // 300 values waiting; a block takes 256 parameters.
    for id in 0..300u32 {
        c.set_param(10_000 + id, 0.5);
    }
    let _ = run(&mut *p, 0.0, &[], &[]);
    assert!(c.has_pending_params(), "the 44 that did not fit are still waiting");
    let _ = run(&mut *p, 0.0, &[], &[]);
    assert!(!c.has_pending_params());
    assert_eq!(p.take_dropped().param_points, 0, "nothing was dropped on the way in");
}

#[test]
fn the_plugins_own_output_comes_back_as_edits() {
    // The Thru hands back every parameter value it gets: as the plugin's own changes in the
    // block's output, and as edits for the UI.
    let mut c = load_clap_fixture("Fixture CLAP Thru");
    let mut p = c.activate(&setup()).unwrap();
    let ins = vec![vec![0.0f32; FRAMES]; 2];
    let mut outs = vec![vec![0.0f32; FRAMES]; 2];
    let mut table = ChannelTable::default();
    let (inputs, outputs) = table.audio(&ins, &mut outs);
    let (mut midi, mut params) = (OutBuf::with_capacity(4), OutBuf::with_capacity(4));
    let t = Transport::default();
    let change = [ParamChange { id: 5, offset: 8, value: 0.25 }];
    let mut block = ProcessBlock { frames: FRAMES, inputs, outputs, events: &[], param_changes: &change, transport: &t, midi_out: &mut midi, param_out: &mut params };
    p.process(&mut block).unwrap();
    assert_eq!(params.as_slice(), &change);
    let mut events = Vec::new();
    c.poll(&mut events);
    assert_eq!(events, vec![ControllerEvent::ParamEdit { id: 5, value: 0.25 }]);
}

#[test]
fn the_processor_runs_on_its_own_thread_and_the_plugins_thread_checks_pass() {
    let mut c = load_clap_fixture("Fixture CLAP Gain");
    let mut p = c.activate(&setup()).unwrap();
    let p = std::thread::spawn(move || {
        for _ in 0..10 {
            let _ = run(&mut *p, 0.25, &[], &[]);
        }
        p
    })
    .join()
    .unwrap();
    // Stopping processing happens here, standing in for the audio thread.
    c.deactivate();
    drop(p);
    let _ = c.activate(&setup()).unwrap();
    assert_eq!(counter(&mut *c, 22), 0, "the plugin's thread checks all held");
}

#[test]
fn a_clap_device_saved_with_its_project_comes_back_the_same() {
    // What the app's project section keeps: the plugin (with its format) and its state.
    let mut c = load_clap_fixture("Fixture CLAP Gain");
    let mut p = c.activate(&setup()).unwrap();
    c.set_param(7, 0.75);
    c.set_param(3, 0.5);
    let _ = run(&mut *p, 0.0, &[], &[]);
    let saved = serde_json::json!({ "plugin": serde_json::to_value(c.info()).unwrap(), "state": c.save_state().unwrap() });
    assert_eq!(saved["plugin"]["format"], "clap");
    drop(p);
    drop(c);

    // Opened again: loaded through the format it was saved with, its sound restored.
    let info: PluginInfo = serde_json::from_value(saved["plugin"].clone()).unwrap();
    let state: Vec<u8> = serde_json::from_value(saved["state"].clone()).unwrap();
    let mut c = load(&info).unwrap();
    c.load_state(&state).unwrap();
    assert_eq!((c.param_value(7), c.param_value(3)), (Some(0.75), Some(0.5)));
    let mut p = c.activate(&setup()).unwrap();
    assert_eq!(run(&mut *p, 0.5, &[], &[]), vec![0.75; FRAMES], "it plays as it did");
}

#[test]
fn a_restart_the_plugin_asks_for_reaches_poll_and_its_new_latency_comes_with_the_next_activation() {
    let mut c = load_clap_fixture("Fixture CLAP Gain");
    let mut p = c.activate(&setup()).unwrap();
    assert_eq!(p.latency_samples(), 3);
    let mut events = Vec::new();
    c.poll(&mut events);
    assert!(events.is_empty(), "{events:?}");
    // Mode C needs more lookahead: the plugin asks for a restart, from the audio thread.
    let _ = run(&mut *p, 0.0, &[], &[ParamChange { id: 3, offset: 0, value: 1.0 }]);
    c.poll(&mut events);
    assert_eq!(events, vec![ControllerEvent::ReloadRequested]);
    // What the app does then: activate again. The plugin says its latency changed while
    // activating; that is the latency read then, not news for later.
    let p = c.activate(&setup()).unwrap();
    assert_eq!((p.latency_samples(), c.latency_samples()), (5, 5));
    events.clear();
    c.poll(&mut events);
    assert!(events.is_empty(), "{events:?}");
    // While it is off, it says so itself: the latency changed, and it wants a restart.
    c.deactivate();
    c.set_param(3, 0.0);
    c.poll(&mut events);
    assert!(events.contains(&ControllerEvent::LatencyChanged) && events.contains(&ControllerEvent::ReloadRequested), "{events:?}");
    let p = c.activate(&setup()).unwrap();
    assert_eq!(p.latency_samples(), 3);
}
