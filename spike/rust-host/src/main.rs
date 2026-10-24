//! musiKore Rust plugin-hosting spike. See ../FINDINGS.md.
//!
//!   rust_host vst3 <path.vst3> [--seconds N] [--no-editor] [--offline]
//!   rust_host au   <path.component> [--seconds N] [--no-editor] [--offline]
//!
//! Loads the plugin, opens its editor in a native window, plays a fixed note
//! pattern at 120 BPM with transport sent every block, and prints the output
//! level once a second.

mod audio;
mod auhost;
mod meter;
mod transport;
mod vst3host;

use meter::{db, Meter};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use transport::SharedTime;

struct Args {
    format: String,
    path: PathBuf,
    seconds: f64,
    editor: bool,
    offline: bool,
}

fn args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let format = it.next().ok_or("usage: rust_host vst3|au <path> [--seconds N] [--no-editor] [--offline]")?;
    let path = PathBuf::from(it.next().ok_or("missing plugin path")?);
    let mut a = Args { format, path, seconds: 8.0, editor: true, offline: false };
    while let Some(x) = it.next() {
        match x.as_str() {
            "--seconds" => a.seconds = it.next().and_then(|s| s.parse().ok()).ok_or("--seconds N")?,
            "--no-editor" => a.editor = false,
            "--offline" => a.offline = true,
            o => return Err(format!("unknown option {o}")),
        }
    }
    Ok(a)
}

fn make_window(mtm: MainThreadMarker, title: &str, w: f64, h: f64) -> Retained<NSWindow> {
    unsafe {
        let rect = NSRect::new(NSPoint::new(200.0, 200.0), NSSize::new(w, h));
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable;
        let win = NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm), rect, style, NSBackingStoreType::Buffered, false,
        );
        win.setReleasedWhenClosed(false);
        win.setTitle(&NSString::from_str(title));
        win.makeKeyAndOrderFront(None);
        win
    }
}

/// Pumps AppKit events for `secs`, printing the meter once a second.
fn run_for(app: &NSApplication, secs: f64, meter: &Meter, mut every_second: impl FnMut()) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    let mut next_print = Instant::now() + Duration::from_secs(1);
    let mut t = 1;
    while Instant::now() < end {
        unsafe {
            let until = NSDate::dateWithTimeIntervalSinceNow(0.05);
            if let Some(ev) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any, Some(&until), NSDefaultRunLoopMode, true,
            ) {
                app.sendEvent(&ev);
            }
        }
        if Instant::now() >= next_print {
            let r = meter.take();
            println!(
                "t={t:>2}s  peak {:>6.1} dBFS  rms {:>6.1} dBFS  blocks {:>4}  worst callback load {:>5.1}%",
                db(r.peak), db(r.rms), r.blocks, r.worst_load * 100.0
            );
            every_second();
            t += 1;
            next_print += Duration::from_secs(1);
        }
    }
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), String> {
    let a = args()?;
    let mtm = MainThreadMarker::new().ok_or("must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    unsafe { app.finishLaunching() };

    let (device, rate) = if a.offline {
        (None, 48000.0)
    } else {
        let (d, c) = audio::device_rate()?;
        let r = c.sample_rate as f64;
        (Some((d, c)), r)
    };
    let meter = Arc::new(Meter::default());
    let time = Arc::new(SharedTime::default());
    println!("sample rate {rate} Hz, block 512");

    let t0 = Instant::now();
    match a.format.as_str() {
        "vst3" => {
            let (plugin, rep) = vst3host::Plugin::load(&a.path, rate)?;
            println!("loaded VST3 '{}' in {:?}", plugin.name, t0.elapsed());
            println!("  classes: {}", rep.classes.join("; "));
            println!(
                "  separate controller: {}, audio buses in/out: {}/{}, event inputs: {}, latency: {} samples, params: {}, state: {} bytes",
                rep.separate_controller, rep.audio_in_buses, rep.audio_out_buses, rep.event_in_buses,
                rep.latency, rep.params, rep.state_bytes
            );

            let mut window = None;
            let editor = if a.editor {
                let mut win_slot: Option<Retained<NSWindow>> = None;
                let ed = plugin.open_editor(|w, h| {
                    let win = make_window(mtm, &format!("{} (VST3)", plugin.name), w as f64, h as f64);
                    let content: Retained<NSView> = win.contentView().unwrap();
                    let p = Retained::as_ptr(&content) as *mut std::ffi::c_void;
                    win_slot = Some(win);
                    p
                })?;
                println!("  editor open: {}x{} (NSView)", ed.width, ed.height);
                let win = win_slot.unwrap();
                let w2 = win.clone();
                *plugin.frame.on_resize.borrow_mut() = Some(Box::new(move |w, h| {
                    w2.setContentSize(NSSize::new(w as f64, h as f64));
                }));
                window = Some(win);
                Some(ed)
            } else {
                None
            };

            let renderer = plugin.renderer(rate, time.clone());
            if let Some((d, c)) = &device {
                let _stream = audio::play(d, c, renderer, meter.clone())?;
                run_for(&app, a.seconds, &meter, || {});
                drop(_stream);
            } else {
                let t = Instant::now();
                audio::offline(renderer, &meter, rate, a.seconds);
                let r = meter.take();
                println!(
                    "offline: {:.1}s rendered in {:?}, peak {:.1} dBFS, rms {:.1} dBFS, worst block load {:.1}%",
                    a.seconds, t.elapsed(), db(r.peak), db(r.rms), r.worst_load * 100.0
                );
            }
            println!("frames rendered: {}, param edits from the editor: {}, editor resizes: {}",
                meter.total_frames(), plugin.param_edits(), plugin.frame.resizes.get());
            match plugin.round_trip_state() {
                Ok(n) => println!("state save + restore: ok ({n} bytes)"),
                Err(e) => println!("state save + restore: {e}"),
            }
            drop(editor);
            if let Some(w) = window { w.close(); }
            drop(plugin);
            println!("unloaded cleanly");
        }
        "au" => {
            let (unit, rep) = auhost::Unit::load(&a.path, rate, time.clone())?;
            println!("loaded AU {} in {:?}", rep.description, t0.elapsed());
            println!("  Cocoa UI: {}, latency: {:.4}s", rep.has_cocoa_ui, rep.latency_s);
            let mut window = None;
            if a.editor {
                let view = unit.editor_view()?;
                let view: &NSView = unsafe { &*(view as *const NSView) };
                let size = view.frame().size;
                let win = make_window(mtm, &format!("{} (AU)", unit.name), size.width, size.height);
                win.setContentView(Some(view));
                println!("  editor open: {}x{} (Cocoa view)", size.width, size.height);
                window = Some(win);
            }
            let renderer = unit.renderer();
            if let Some((d, c)) = &device {
                let _stream = audio::play(d, c, renderer, meter.clone())?;
                run_for(&app, a.seconds, &meter, || {});
                drop(_stream);
            } else {
                let t = Instant::now();
                audio::offline(renderer, &meter, rate, a.seconds);
                let r = meter.take();
                println!(
                    "offline: {:.1}s rendered in {:?}, peak {:.1} dBFS, rms {:.1} dBFS, worst block load {:.1}%",
                    a.seconds, t.elapsed(), db(r.peak), db(r.rms), r.worst_load * 100.0
                );
            }
            println!("frames rendered: {}, tempo/position queries from the plugin: {}",
                meter.total_frames(), time.queries.load(std::sync::atomic::Ordering::Relaxed));
            match unit.round_trip_state() {
                Ok(()) => println!("state save + restore: ok"),
                Err(e) => println!("state save + restore: {e}"),
            }
            if let Some(w) = window { w.close(); }
            drop(unit);
            println!("unloaded cleanly");
        }
        f => return Err(format!("unknown format {f}")),
    }
    let _: Option<&AnyObject> = None;
    Ok(())
}
