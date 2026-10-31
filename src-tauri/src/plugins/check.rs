//! The plugin test list (`docs/PLAN.md` section 5) in the running app, in
//! debug builds: set `TUNEKORE_PLUGINS_CHECK` to a report file and start the
//! app. It needs no window and no clicks:
//!
//! 1. waits for the scan, with the master level off (the check is silent);
//! 2. puts each instrument on a new instrument track and Guitar Rig 7 on
//!    Serum 2's track, plays a chord into each and reads the track's meter;
//! 3. sets one parameter on each, saves the project, starts a new one, opens
//!    the saved one again, and checks every plugin came back on its track
//!    with that parameter and plays again;
//! 4. opens and closes each editor (Kontakt 7's last: on a Mac without its
//!    licence it shows a dialog that waits for a click);
//! 5. writes the report and quits, which drops every plugin and runs
//!    `vst3::shutdown` as a real quit does.
//!
//! Run it with a dev identifier so it can't touch the installed app's files:
//!
//! ```sh
//! TAURI_CONFIG='{"identifier":"com.devkore.tunekore.plugins-check"}' \
//!   TUNEKORE_PLUGINS_CHECK=/tmp/plugins-check.txt cargo run -p tunekore
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};
use tunekore_engine::plugins::EventKind;
use tunekore_engine::{PluginLink, SamplerHandle, TrackId, SAMPLER_TRACK};

use super::{add, close_editor, lock, open_editor, with_doc, Plugins, Role, Target};
use crate::project;

/// The test list, in order, and whether each is an instrument.
const LIST: [(&str, bool); 6] = [("Serum 2", true), ("Serum", true), ("Analog Lab V", true), ("Jup-8 V4", true), ("Kontakt 7", true), ("Guitar Rig 7", false)];
/// The parameter value the save-and-reopen step looks for.
const MARK: f64 = 0.37;

pub fn start(app: &AppHandle) {
    let Some(report) = std::env::var_os("TUNEKORE_PLUGINS_CHECK").map(PathBuf::from) else { return };
    let app = app.clone();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let started = Instant::now();
        if let Err(e) = run(&app, &mut out) {
            out.push(format!("STOPPED: {e}"));
        }
        out.push(format!("took {:.0} s", started.elapsed().as_secs_f64()));
        let _ = std::fs::write(&report, out.join("\n") + "\n");
        app.exit(0);
    });
}

/// What the check knows about one plugin on a track.
struct Placed {
    name: String,
    /// The Sampler pad it plays, for a pad's instrument.
    pad: Option<usize>,
    id: u64,
    track: TrackId,
    link: Arc<PluginLink>,
    param: Option<u32>,
}

fn wait_for(what: &str, limit: Duration, mut ready: impl FnMut() -> bool) -> Result<(), String> {
    let until = Instant::now() + limit;
    while !ready() {
        if Instant::now() > until {
            return Err(format!("{what} took longer than {} s", limit.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// What the check plays a plugin through.
enum Through<'a> {
    /// Its link's live notes (an instrument, or the instrument before an effect).
    Link(&'a PluginLink),
    /// Pressing its Sampler pad.
    Pad(usize),
}

/// Plays a chord (or holds a pad) for a moment and returns the track's peak.
fn play(app: &AppHandle, track: TrackId, through: Through<'_>) -> f32 {
    let meter = lock(crate::audio(app)).engine.track_meter(track);
    let Some(meter) = meter else { return 0.0 };
    meter.take_peak();
    let pads = app.state::<SamplerHandle>().inner().clone();
    let link = match through {
        Through::Link(l) => Some(l),
        Through::Pad(p) => {
            pads.trigger_velocity(p, 0.8);
            None
        }
    };
    for key in [48, 52, 55, 60] {
        if let Some(link) = link {
            link.send_live(EventKind::NoteOn { channel: 0, key, velocity: 0.8, note_id: -1 });
        }
    }
    let mut peak = 0.0f32;
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
        peak = peak.max(meter.take_peak());
    }
    match (through, link) {
        (_, Some(link)) => {
            for key in [48, 52, 55, 60] {
                link.send_live(EventKind::NoteOff { channel: 0, key, velocity: 0.0, note_id: -1 });
            }
        }
        (Through::Pad(p), None) => pads.release(p),
        _ => {}
    }
    std::thread::sleep(Duration::from_millis(300));
    peak
}

/// The device's link and the first continuous automatable parameter.
fn handles(app: &AppHandle, id: u64) -> Result<(Arc<PluginLink>, Option<u32>, TrackId), String> {
    with_doc(app, move |_, d| {
        let i = d.find(id)?;
        let s = &mut d.slots[i];
        let l = s.loaded.as_mut().ok_or("not loaded")?;
        let param = l.controller.params().iter().find(|p| p.automatable && !p.hidden && !p.read_only && !p.is_bypass && p.step_count == 0).map(|p| p.id);
        Ok((l.link.clone(), param, s.track))
    })?
}

fn run(app: &AppHandle, out: &mut Vec<String>) -> Result<(), String> {
    let state = app.state::<Plugins>();
    wait_for("opening the output", Duration::from_secs(10), || lock(crate::audio(app)).engine.is_open())?;
    lock(crate::audio(app)).engine.set_master(0.0);
    out.push(format!("output at {} Hz, master off", lock(crate::audio(app)).engine.output_rate()));
    wait_for("the plugin scan", Duration::from_secs(900), || !lock(&state.catalog).scanning)?;
    {
        let c = lock(&state.catalog);
        let cache = c.cache.as_ref().ok_or_else(|| c.error.clone().unwrap_or_else(|| "no scan".into()))?;
        out.push(format!("scan: {} plugins, {} failed", cache.plugins().count(), cache.failures().count()));
    }

    // Load and play.
    let mut placed: Vec<Placed> = Vec::new();
    for (name, instrument) in LIST {
        let info = {
            let c = lock(&state.catalog);
            c.cache.as_ref().and_then(|cache| cache.plugins().find(|e| e.info.name == name).map(|e| e.info.clone()))
        };
        let Some(info) = info else {
            out.push(format!("{name}: NOT INSTALLED (not in the scan)"));
            continue;
        };
        let track = if instrument { None } else { placed.first().map(|p| p.track) };
        if !instrument && track.is_none() {
            out.push(format!("{name}: skipped, no instrument to put it after"));
            continue;
        }
        let t0 = Instant::now();
        let target = match track {
            Some(t) => Target::Track(t, None),
            None => Target::NewTrack,
        };
        let id = match with_doc(app, move |app, d| add(app, d, info, target))? {
            Ok(id) => id,
            Err(e) => {
                out.push(format!("{name}: LOAD FAILED: {e}"));
                continue;
            }
        };
        let loaded = t0.elapsed().as_secs_f64();
        let (link, param, track) = handles(app, id)?;
        wait_for(&format!("{name} getting ready"), Duration::from_secs(10), || link.is_ready())?;
        // A moment to settle, as anyone takes before playing (some load their sound in the background).
        std::thread::sleep(Duration::from_millis(700));
        // An effect is heard through the instrument before it.
        let source = if instrument { link.clone() } else { placed[0].link.clone() };
        let peak = play(app, track, Through::Link(&source));
        out.push(format!("{name}: loaded in {loaded:.1} s on track {}, {} after a chord: peak {peak:.3}", track.0, if peak > 0.001 { "PLAYS" } else { "SILENT" }));
        placed.push(Placed { name: name.to_string(), pad: None, id, track, link, param });
    }

    // An instrument on a Sampler pad: Serum 2 on pad 1, played by pressing the pad.
    let serum = {
        let c = lock(&state.catalog);
        c.cache.as_ref().and_then(|cache| cache.plugins().find(|e| e.info.name == "Serum 2").map(|e| e.info.clone()))
    };
    if let Some(info) = serum {
        match with_doc(app, move |app, d| add(app, d, info, Target::Pad(0)))? {
            Ok(id) => {
                let (link, param, _) = handles(app, id)?;
                wait_for("Serum 2 on pad 1 getting ready", Duration::from_secs(10), || link.is_ready())?;
                let peak = play(app, SAMPLER_TRACK, Through::Pad(0));
                out.push(format!("Serum 2 on pad 1: {} when the pad is pressed: peak {peak:.3}", if peak > 0.001 { "PLAYS" } else { "SILENT" }));
                placed.push(Placed { name: "Serum 2 on pad 1".into(), pad: Some(0), id, track: SAMPLER_TRACK, link, param });
            }
            Err(e) => out.push(format!("Serum 2 on pad 1: LOAD FAILED: {e}")),
        }
    }

    // Mark a parameter on each, note every parameter's value and whether the
    // plugin gives its state, then save, empty and reopen the project.
    let marks: Vec<(u64, u32)> = placed.iter().filter_map(|p| p.param.map(|q| (p.id, q))).collect();
    with_doc(app, move |_, d| {
        for (id, param) in marks {
            if let Ok(i) = d.find(id) {
                if let Some(l) = &mut d.slots[i].loaded {
                    l.controller.set_param(param, MARK);
                }
            }
        }
    })?;
    std::thread::sleep(Duration::from_millis(300));
    let mut before: HashMap<u64, Vec<(u32, f64)>> = HashMap::new();
    for p in &placed {
        let id = p.id;
        let (values, state) = with_doc(app, move |_, d| {
            let i = d.find(id).ok()?;
            let s = &mut d.slots[i];
            let l = s.loaded.as_mut()?;
            super::flush(l, true);
            let ids: Vec<u32> = l.controller.params().iter().filter(|q| !q.hidden).map(|q| q.id).collect();
            let values: Vec<(u32, f64)> = ids.into_iter().filter_map(|q| l.controller.param_value(q).map(|v| (q, v))).collect();
            let state = l.controller.save_state().map(|b| b.len()).map_err(|e| e.to_string());
            Some((values, state))
        })?
        .ok_or("gone before saving")?;
        let name = p.param.and_then(|q| with_doc(app, move |_, d| d.find(id).ok().and_then(|i| d.slots[i].loaded.as_ref().and_then(|l| l.controller.params().iter().find(|x| x.id == q).map(|x| x.name.clone())))).ok().flatten());
        out.push(format!(
            "{}: marked {} = {MARK}; {}",
            p.name,
            name.as_deref().unwrap_or("no parameter"),
            match state {
                Ok(n) => format!("state {} KB", n.div_ceil(1024)),
                Err(e) => format!("STATE REFUSED: {e}"),
            }
        ));
        before.insert(id, values);
    }
    let dir = std::env::temp_dir().join(format!("tunekore-plugins-check-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join("check.tunek");
    let t0 = Instant::now();
    project::save(app, Some(file.clone()))?;
    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    out.push(format!("saved {} ({} KB) in {:.1} s", file.display(), size / 1024, t0.elapsed().as_secs_f64()));
    project::new_project(app)?;
    let left = with_doc(app, |_, d| d.slots.len())?;
    let tracks = lock(crate::audio(app)).engine.tracks().len();
    out.push(format!("new project: {left} plugins and {tracks} track(s) left"));
    let t0 = Instant::now();
    project::open(app, &file, Some(file.clone()), false)?;
    out.push(format!("reopened in {:.1} s", t0.elapsed().as_secs_f64()));
    for p in &placed {
        let (id, param) = (p.id, p.param);
        let back = with_doc(app, move |_, d| {
            let i = d.find(id).ok()?;
            let s = &mut d.slots[i];
            let status = s.status();
            let (track, role) = (s.track, s.role);
            let value = match (&mut s.loaded, param) {
                (Some(l), Some(q)) => l.controller.param_value(q),
                _ => None,
            };
            Some((status, track, role, value, s.loaded.as_ref().map(|l| l.link.clone())))
        })?;
        let Some((status, track, role, value, link)) = back else {
            out.push(format!("{}: NOT BACK after reopening", p.name));
            continue;
        };
        let was = before.remove(&id).unwrap_or_default();
        let total = was.len();
        let matching = with_doc(app, move |_, d| {
            let Ok(i) = d.find(id) else { return 0 };
            let Some(l) = d.slots[i].loaded.as_mut() else { return 0 };
            was.iter().filter(|(q, v)| l.controller.param_value(*q).is_some_and(|now| (now - v).abs() < 1e-3)).count()
        })?;
        let same = match value {
            Some(v) => format!("parameter {:.3} ({})", v, if (v - MARK).abs() < 0.01 { "KEPT" } else { "CHANGED" }),
            None => "no parameter to compare".into(),
        };
        let mut heard = "not played".to_string();
        if let Some(link) = link {
            let ready = wait_for("ready", Duration::from_secs(10), || link.is_ready()).is_ok();
            let peak = match (ready, role, p.pad) {
                (false, _, _) => None,
                (true, Role::Pad, Some(pad)) => Some(play(app, track, Through::Pad(pad))),
                (true, Role::Instrument, _) => Some(play(app, track, Through::Link(&link))),
                (true, _, _) => first_instrument(app, track).map(|src| play(app, track, Through::Link(&src))),
            };
            if let Some(peak) = peak {
                heard = format!("{} (peak {peak:.3})", if peak > 0.001 { "PLAYS" } else { "SILENT" });
            }
        }
        out.push(format!("{}: back as {status} on track {}, {same}, {matching} of {total} parameters as saved, {heard}", p.name, track.0));
    }

    // Editors, Kontakt's last.
    let mut order: Vec<&Placed> = placed.iter().filter(|p| p.name != "Kontakt 7" && p.pad.is_none()).collect();
    order.extend(placed.iter().filter(|p| p.name == "Kontakt 7"));
    for p in order {
        let id = p.id;
        let t0 = Instant::now();
        let opened = with_doc(app, move |app, d| {
            let i = d.find(id)?;
            open_editor(app, d, i)?;
            Ok::<_, String>(d.slots[i].window.is_some())
        });
        match opened {
            Ok(Ok(true)) => {
                std::thread::sleep(Duration::from_millis(1500));
                with_doc(app, move |_, d| {
                    if let Ok(i) = d.find(id) {
                        close_editor(&mut d.slots[i]);
                    }
                })?;
                out.push(format!("{}: EDITOR opened in {:.1} s and closed", p.name, t0.elapsed().as_secs_f64()));
            }
            Ok(Ok(false)) => out.push(format!("{}: EDITOR NOT OPEN", p.name)),
            Ok(Err(e)) => out.push(format!("{}: EDITOR FAILED: {e}", p.name)),
            Err(e) => out.push(format!("{}: EDITOR BLOCKED: {e}", p.name)),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    out.push("quitting: every plugin goes, then vst3::shutdown".into());
    Ok(())
}

/// The link of `track`'s instrument, to play an effect after it.
fn first_instrument(app: &AppHandle, track: TrackId) -> Option<Arc<PluginLink>> {
    with_doc(app, move |_, d| d.instrument_of(track).and_then(|i| d.slots[i].loaded.as_ref().map(|l| l.link.clone()))).ok().flatten()
}
