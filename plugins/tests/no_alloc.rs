//! `Plugin::process` must not allocate on our side: this counts every allocation made on the
//! test's thread while `process` runs, against the fixture's "Fixture Thru", which allocates
//! nothing itself. Notes, parameter ramps, edits from the UI, plugin output events and output
//! parameter changes all go through the counted path.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use tunekore_plugins::*;

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

fn note() {
    if COUNTING.with(Cell::get) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: forwards every call to the system allocator unchanged; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn process_allocates_nothing() {
    const FRAMES: usize = 256;
    let mut c = common::load_fixture("Fixture Thru");
    let mut p = c.activate(&ProcessSetup { max_block_size: FRAMES as u32, ..ProcessSetup::default() }).unwrap();
    let layout = p.layout().clone();
    assert_eq!((layout.input_channels(), layout.output_channels()), (2, 2));

    let ins: Vec<Vec<f32>> = (0..2).map(|ch| (0..FRAMES).map(|i| (i + ch) as f32 / FRAMES as f32).collect()).collect();
    let mut outs = vec![vec![0.0f32; FRAMES]; 2];
    let mut table = ChannelTable::with_capacity(2, 2);
    let events: Vec<Event> = (0..32u32).map(|i| Event { offset: i * 4, kind: EventKind::NoteOn { channel: 0, key: 40 + i as u8, velocity: 0.5, note_id: i as i32 } }).collect();
    let ramp: Vec<ParamChange> = (0..8u32).flat_map(|i| [ParamChange { id: 100 + i, offset: 0, value: 0.0 }, ParamChange { id: 100 + i, offset: 128, value: 1.0 }]).collect();
    let (mut midi, mut params) = (OutBuf::with_capacity(64), OutBuf::with_capacity(64));
    let mut transport = Transport { playing: true, ..Transport::default() };

    for block in 0..2000usize {
        if block % 100 == 0 {
            // An edit from the UI, queued on the main thread side for the next block.
            c.set_param(7, (block / 100) as f64 / 20.0);
        }
        midi.clear();
        params.clear();
        transport.project_samples = (block * FRAMES) as i64;
        transport.continuous_samples = transport.project_samples;
        transport.project_beats = transport.project_samples as f64 / 24_000.0;
        // Everything from building the block to the plugin returning is counted.
        COUNTING.with(|f| f.set(true));
        let (inputs, outputs) = table.audio(&ins, &mut outs);
        let mut b = ProcessBlock { frames: FRAMES, inputs, outputs, events: &events, param_changes: &ramp, transport: &transport, midi_out: &mut midi, param_out: &mut params };
        let r = p.process(&mut b);
        let dropped = p.take_dropped();
        COUNTING.with(|f| f.set(false));
        r.unwrap();
        assert_eq!(dropped, Dropped::default());
        assert_eq!(outs, ins, "audio passes through");
        assert_eq!(midi.as_slice().len(), 32, "every note echoed back out");
        // The ramps echo their last points; the UI edit shows up in its block too.
        assert!(params.as_slice().len() >= 8);
    }
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0, "allocations inside process");
}

#[test]
fn a_plugin_that_throws_in_process_is_switched_off_without_allocating() {
    let dir = common::scratch_dir("throws-in-process-alloc");
    let Some(bundle) = common::bad_plugin(&dir, "ThrowsInProcess", common::Misbehaviour::ThrowsInProcess) else {
        return eprintln!("skipped: no C++ compiler");
    };
    let info = vst3::classes(&bundle).unwrap().remove(0);
    let mut c = vst3::load(&info).unwrap();
    let mut p = c.activate(&ProcessSetup { max_block_size: 64, ..ProcessSetup::default() }).unwrap();
    assert_eq!(p.layout().output_channels(), 2);
    let none: [Vec<f32>; 0] = [];
    let mut outs = vec![vec![1.0f32; 64]; 2];
    let mut table = ChannelTable::with_capacity(0, 2);
    let (mut midi, mut params) = (OutBuf::with_capacity(4), OutBuf::with_capacity(4));
    let t = Transport::default();
    let note = [Event { offset: 0, kind: EventKind::NoteOn { channel: 0, key: 60, velocity: 1.0, note_id: 1 } }];

    // The throw, its message, switching the plugin off and silencing the outputs: none of it may
    // allocate or free on our side. (The C++ runtime allocates the exception object itself; that
    // is malloc, not Rust's allocator, and outside our control.)
    COUNTING.with(|f| f.set(true));
    let (inputs, outputs) = table.audio(&none, &mut outs);
    let mut b = ProcessBlock { frames: 64, inputs, outputs, events: &note, param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    let first = p.process(&mut b);
    let (inputs, outputs) = table.audio(&none, &mut outs);
    let mut b = ProcessBlock { frames: 64, inputs, outputs, events: &note, param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    let second = p.process(&mut b);
    COUNTING.with(|f| f.set(false));
    assert_eq!(first, Err(ProcessError::Failed));
    assert_eq!(second, Err(ProcessError::Failed), "not called again");
    assert!(outs.iter().all(|ch| ch.iter().all(|&x| x == 0.0)), "silence");
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0, "allocations on the exception path");

    // The controller tells the main thread, once, and says why.
    let mut events = Vec::new();
    c.poll(&mut events);
    assert!(events.contains(&ControllerEvent::Failed));
    events.clear();
    c.poll(&mut events);
    assert!(!events.contains(&ControllerEvent::Failed), "reported once");
    let why = c.take_error().unwrap();
    assert!(why.contains("IAudioProcessor::process") && why.contains("boom from process") && why.contains("std::runtime_error"), "{why}");
    // And it never calls the plugin again: these do nothing rather than reach it.
    c.set_param(1, 0.5);
    assert_eq!(c.param_text(1, 0.5), None);
    assert!(c.save_state().is_err());
    assert!(c.activate(&ProcessSetup::default()).is_err());
    drop(p);
    drop(c); // leaks the plugin's objects instead of releasing them
}
