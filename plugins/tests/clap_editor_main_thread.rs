//! A CLAP plugin's editor attached to a native view, on the process main thread (`harness =
//! false`: the standard harness never runs tests there, and AppKit insists on it), with the
//! requests the plugin makes of the main thread: a callback, a timer, a parameter flush, a
//! resize.
//!
//! The view the editor goes into is never put in a window, so nothing shows on screen.

mod common;

#[cfg(target_os = "macos")]
fn main() {
    use objc2::MainThreadOnly;
    use objc2_app_kit::NSView;
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tunekore_plugins::*;

    if std::env::args().any(|a| a == "--list") {
        println!("clap_editor_main_thread: test");
        return;
    }
    let mtm = objc2::MainThreadMarker::new().expect("the test runs on the main thread");
    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 100.0)));
    let parent = NativeParent(objc2::rc::Retained::as_ptr(&view).cast_mut().cast());
    let counter = |c: &mut dyn PluginController, id: ParamId| -> u32 {
        let n = c.param_value(id).expect("a counter");
        c.param_plain(id, n).unwrap().round() as u32
    };

    let mut gain = common::load_clap_fixture("Fixture CLAP Gain");
    assert!(gain.has_editor());
    let asked: Arc<Mutex<Vec<EditorSize>>> = Arc::default();
    let seen = asked.clone();
    let size = gain.open_editor(parent, Box::new(move |s| seen.lock().unwrap().push(s)));
    assert_eq!(size, Ok(EditorSize { width: 320, height: 240 }), "the fixture's editor size");

    // Showing the editor turned its gain knob (the plugin is off, so the host flushes it on the
    // main thread) and asked for 400 x 300; making it asked for a main-thread callback and
    // started a 20 ms timer. `poll` serves all of it.
    std::thread::sleep(Duration::from_millis(30));
    let mut events = Vec::new();
    gain.poll(&mut events);
    assert_eq!(events, vec![ControllerEvent::EditBegin { id: 7 }, ControllerEvent::ParamEdit { id: 7, value: 0.9 }, ControllerEvent::EditEnd { id: 7 }], "the knob, from the editor");
    assert_eq!(*asked.lock().unwrap(), vec![EditorSize { width: 400, height: 300 }], "its resize request reached the window");
    assert_eq!(counter(&mut *gain, 21), 1, "on_main_thread ran once");
    let ticks = counter(&mut *gain, 20);
    assert!(ticks >= 1, "the timer ran ({ticks})");

    // The user resizes the window: the editor takes what fits it.
    assert_eq!(gain.resize_editor(EditorSize { width: 500, height: 400 }), Some(EditorSize { width: 500, height: 400 }));
    assert_eq!(gain.resize_editor(EditorSize { width: 5000, height: 50 }), Some(EditorSize { width: 1000, height: 150 }), "adjusted to its limits");
    assert!(gain.editor_resizable());

    // While it processes, an edit in its window comes back in the next block instead.
    let mut p = gain.activate(&ProcessSetup { max_block_size: 64, ..ProcessSetup::default() }).unwrap();
    assert!(gain.open_editor(parent, Box::new(|_| {})).is_ok(), "opening again replaces the editor");
    events.clear();
    gain.poll(&mut events);
    assert!(!events.contains(&ControllerEvent::EditEnd { id: 7 }), "not flushed on the main thread while active");
    let ins = vec![vec![0.5f32; 64]; 2];
    let mut outs = vec![vec![0.0f32; 64]; 2];
    let mut table = ChannelTable::default();
    let (inputs, outputs) = table.audio(&ins, &mut outs);
    let (mut midi, mut params) = (OutBuf::with_capacity(4), OutBuf::with_capacity(4));
    let t = Transport::default();
    let mut block = ProcessBlock { frames: 64, inputs, outputs, events: &[], param_changes: &[], transport: &t, midi_out: &mut midi, param_out: &mut params };
    p.process(&mut block).unwrap();
    assert_eq!(params.as_slice(), &[ParamChange { id: 7, offset: 0, value: 0.9 }], "the plugin's own change, in the block");
    assert!(outs[0].iter().all(|&x| (x - 0.9).abs() < 1e-6), "and it plays it");
    events.clear();
    gain.poll(&mut events);
    assert!(events.contains(&ControllerEvent::ParamEdit { id: 7, value: 0.9 }), "{events:?}");

    gain.close_editor();
    assert_eq!(gain.resize_editor(EditorSize { width: 500, height: 400 }), None, "no editor after closing");
    // Its timer went with it.
    let before = counter(&mut *gain, 20);
    std::thread::sleep(Duration::from_millis(50));
    gain.poll(&mut events);
    assert_eq!(counter(&mut *gain, 20), before, "no ticks once the editor is gone");
    assert_eq!(counter(&mut *gain, 22), 0, "every call came on the thread CLAP says");
    drop(p);
    drop(gain);

    // An instrument without an editor says so.
    let mut synth = common::load_clap_fixture("Fixture CLAP Synth");
    assert!(!synth.has_editor());
    assert!(matches!(synth.open_editor(parent, Box::new(|_| {})), Err(PluginError::Unsupported(_))));

    // Off the main thread an editor is refused rather than attached where AppKit forbids it.
    let refused = std::thread::spawn(move || {
        let mut g = common::load_clap_fixture("Fixture CLAP Gain");
        g.open_editor(NativeParent(std::ptr::dangling_mut()), Box::new(|_| {})).is_err()
    });
    assert!(refused.join().unwrap(), "editors are main-thread only");
    println!("clap_editor_main_thread: ok");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("clap_editor_main_thread: skipped (editors are tested on macOS only so far)");
}
