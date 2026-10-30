//! Opens a VST3 plugin's editor in a window, keeps the plugin processing while it is open, and
//! reports what the editor did (resizes, edits) when it closes.
//!
//!     cargo run --release --example editor -- <plugin.vst3> [--name NAME] [--seconds N] [--resize WxH]
//!
//! macOS only for now. The window closes by itself after `--seconds` (default 5); closing it
//! sooner ends the run too.

#[cfg(target_os = "macos")]
fn main() {
    if let Err(e) = mac::run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("the editor example runs on macOS only so far");
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tunekore_plugins::editor_window::EditorWindow;
    use tunekore_plugins::*;

    pub fn run() -> Result<(), String> {
        let mut args = std::env::args().skip(1);
        let bundle = PathBuf::from(args.next().ok_or("usage: editor <plugin.vst3> [--name NAME] [--seconds N] [--resize WxH]")?);
        let (mut name, mut seconds, mut resize) = (None, 5.0f64, None);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--name" => name = args.next(),
                "--seconds" => seconds = args.next().and_then(|s| s.parse().ok()).ok_or("--seconds needs a number")?,
                "--resize" => {
                    let v = args.next().ok_or("--resize needs WxH")?;
                    let (w, h) = v.split_once('x').ok_or("--resize needs WxH")?;
                    resize = Some(EditorSize { width: w.parse().map_err(|_| "bad width")?, height: h.parse().map_err(|_| "bad height")? });
                }
                o => return Err(format!("unknown option {o}")),
            }
        }
        let mtm = MainThreadMarker::new().ok_or("must run on the main thread")?;
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        app.finishLaunching();

        let infos = vst3::classes(&bundle).map_err(|e| e.to_string())?;
        let info = match &name {
            Some(n) => infos.iter().find(|i| i.name.eq_ignore_ascii_case(n)),
            None => infos.first(),
        }
        .ok_or("no such plugin in the bundle")?
        .clone();
        let t = Instant::now();
        let mut c = vst3::load(&info).map_err(|e| e.to_string())?;
        println!("plugin: {} ({}) loaded in {} ms", info.name, info.vendor, t.elapsed().as_millis());
        let setup = ProcessSetup { sample_rate: 48_000.0, max_block_size: 512, ..ProcessSetup::default() };
        let mut plugin = c.activate(&setup).map_err(|e| e.to_string())?;

        let window = EditorWindow::new(&info.name, EditorSize { width: 400, height: 300 }).map_err(|e| e.to_string())?;
        let handle = window.handle();
        let resizes = Arc::new(AtomicU32::new(0));
        let counter = resizes.clone();
        let t = Instant::now();
        let size = c
            .open_editor(
                window.parent(),
                Box::new(move |s| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    println!("editor asked for {}x{}", s.width, s.height);
                    handle.resize(s);
                }),
            )
            .map_err(|e| e.to_string())?;
        window.resize(size);
        window.raise();
        println!("editor: attached to an NSView in {} ms, {}x{}, resizable: {}", t.elapsed().as_millis(), size.width, size.height, if c.editor_resizable() { "yes" } else { "no" });

        // The processor goes to its own thread, as it would in the engine, and processes silence
        // at real-time pace while the editor is up. (Some plugins wait on the main thread from
        // inside `process`, so processing on the main thread would deadlock them.)
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let audio = {
            let stop = stop.clone();
            let rate = setup.sample_rate;
            std::thread::Builder::new()
                .name("audio".into())
                .spawn(move || -> Result<(Box<dyn Plugin>, usize), String> {
                    let layout = plugin.layout().clone();
                    let ins = vec![vec![0.0f32; 512]; layout.input_channels()];
                    let mut outs = vec![vec![0.0f32; 512]; layout.output_channels()];
                    let mut table = ChannelTable::with_capacity(ins.len(), outs.len());
                    let (mut midi, mut params) = (OutBuf::with_capacity(64), OutBuf::with_capacity(64));
                    let (start, mut rendered) = (Instant::now(), 0usize);
                    while !stop.load(Ordering::Relaxed) {
                        let due = (start.elapsed().as_secs_f64() * rate) as usize;
                        while rendered + 512 <= due {
                            let (inputs, outputs) = table.audio(&ins, &mut outs);
                            midi.clear();
                            params.clear();
                            let transport = Transport { project_samples: rendered as i64, continuous_samples: rendered as i64, ..Transport::default() };
                            let mut block = ProcessBlock { frames: 512, inputs, outputs, events: &[], param_changes: &[], transport: &transport, midi_out: &mut midi, param_out: &mut params };
                            plugin.process(&mut block).map_err(|e| format!("process failed: {e}"))?;
                            rendered += 512;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Ok((plugin, rendered / 512))
                })
                .map_err(|e| e.to_string())?
        };

        // The main thread pumps AppKit events so the window draws and takes clicks.
        let (start, mut events) = (Instant::now(), Vec::new());
        let (mut edits, mut resized_once) = (0usize, false);
        while start.elapsed() < Duration::from_secs_f64(seconds) && window.is_visible() && !audio.is_finished() {
            // SAFETY: AppKit event pumping on the main thread.
            unsafe {
                let until = NSDate::dateWithTimeIntervalSinceNow(0.01);
                if let Some(ev) = app.nextEventMatchingMask_untilDate_inMode_dequeue(NSEventMask::Any, Some(&until), NSDefaultRunLoopMode, true) {
                    app.sendEvent(&ev);
                }
            }
            c.poll(&mut events);
            edits += events.iter().filter(|e| matches!(e, ControllerEvent::ParamEdit { .. })).count();
            events.clear();
            if let Some(s) = resize
                && !resized_once
                && start.elapsed() > Duration::from_secs_f64(seconds / 2.0)
            {
                resized_once = true;
                match c.resize_editor(s) {
                    Some(took) => {
                        window.resize(took);
                        println!("offered {}x{}, the editor took {}x{}", s.width, s.height, took.width, took.height);
                    }
                    None => println!("no editor to resize"),
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        let (plugin, blocks) = audio.join().map_err(|_| "the audio thread panicked".to_string())??;
        println!("ran {:.1} s: {} blocks processed on the audio thread, {} parameter edit(s) from the editor, {} resize request(s) from the editor", start.elapsed().as_secs_f64(), blocks, edits, resizes.load(Ordering::Relaxed));
        c.close_editor();
        window.close();
        drop(plugin);
        drop(c);
        vst3::shutdown();
        println!("closed cleanly");
        Ok(())
    }
}
