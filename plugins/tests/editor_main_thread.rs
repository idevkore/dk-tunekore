//! A plugin editor attached to a native view, on the process main thread (`harness = false`:
//! the standard harness never runs tests there, and AppKit insists on it).
//!
//! The view the editor goes into is never put in a window, so nothing flashes on screen. The
//! window itself is exercised by `examples/editor.rs`.

mod common;

#[cfg(target_os = "macos")]
fn main() {
    use objc2::MainThreadOnly;
    use objc2_app_kit::NSView;
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tunekore_plugins::{ControllerEvent, EditorSize, NativeParent, PluginError};

    if std::env::args().any(|a| a == "--list") {
        println!("editor_main_thread: test");
        return;
    }
    let mtm = objc2::MainThreadMarker::new().expect("the test runs on the main thread");
    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 100.0)));
    let parent = NativeParent(objc2::rc::Retained::as_ptr(&view).cast_mut().cast());

    let mut gain = common::load_fixture("Fixture Gain");
    assert!(gain.has_editor());
    let resizes = Arc::new(AtomicU32::new(0));
    let seen = resizes.clone();
    let size = gain.open_editor(parent, Box::new(move |_| {
        seen.fetch_add(1, Ordering::Relaxed);
    }));
    assert_eq!(size, Ok(EditorSize { width: 320, height: 240 }), "the fixture's editor size");
    // Opening again replaces the first editor instead of stacking a second one.
    assert!(gain.open_editor(parent, Box::new(|_| {})).is_ok());
    let offered = EditorSize { width: 500, height: 400 };
    let taken = gain.resize_editor(offered).expect("an editor is open");
    assert!(taken.width > 0 && taken.height > 0);
    // The fixture's controller makes one edit through the host when it gets its handler (as if
    // the user had moved a knob); it comes back from `poll`.
    let mut events = Vec::new();
    gain.poll(&mut events);
    assert!(events.contains(&ControllerEvent::ParamEdit { id: 7, value: 0.9 }), "{events:?}");
    assert!(events.contains(&ControllerEvent::EditBegin { id: 7 }) && events.contains(&ControllerEvent::EditEnd { id: 7 }));
    gain.close_editor();
    assert_eq!(gain.resize_editor(offered), None, "no editor after closing");
    drop(gain);

    // An instrument without an editor says so.
    let mut synth = common::load_fixture("Fixture Synth");
    assert!(!synth.has_editor());
    assert!(matches!(synth.open_editor(parent, Box::new(|_| {})), Err(PluginError::Unsupported(_))));

    // Off the main thread an editor is refused rather than attached where AppKit forbids it.
    let refused = std::thread::spawn(move || {
        let mut g = common::load_fixture("Fixture Gain");
        g.open_editor(NativeParent(1 as *mut _), Box::new(|_| {})).is_err()
    });
    assert!(refused.join().unwrap(), "editors are main-thread only");
    println!("editor_main_thread: ok");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("editor_main_thread: skipped (editors are tested on macOS only so far)");
}
