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
    let in_refs: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
    let mut outs = vec![vec![0.0f32; FRAMES]; 2];
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
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
        let mut b = ProcessBlock { frames: FRAMES, inputs: &in_refs, outputs: &mut out_refs, events: &events, param_changes: &ramp, transport: &transport, midi_out: &mut midi, param_out: &mut params };
        COUNTING.with(|f| f.set(true));
        let r = p.process(&mut b);
        COUNTING.with(|f| f.set(false));
        r.unwrap();
        drop(out_refs);
        assert_eq!(outs, ins, "audio passes through");
        assert_eq!(midi.as_slice().len(), 32, "every note echoed back out");
        // The ramps echo their last points; the UI edit shows up in its block too.
        assert!(params.as_slice().len() >= 8);
    }
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0, "allocations inside process");
}
