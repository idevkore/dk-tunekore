//! Loads a VST3 plugin, plays notes through it offline and writes a WAV file.
//!
//!     cargo run --release --example render -- <plugin.vst3> [options]
//!
//!     --name NAME | --class ID   which plugin in the bundle (default: the first)
//!     --seconds N                length of the render (default 4)
//!     --out FILE.wav             write the audio here
//!     --no-notes                 send no notes (a silent control)
//!     --input sine               feed an effect a sine wave (default: silence)
//!     --all-buses                switch on every audio bus (Kontakt's extra outputs, sidechains)
//!     --param ID=VALUE           set a normalized parameter before rendering (repeatable)
//!     --state-check              save the state, load it into a fresh instance and render again
//!     --list-params N            print the first N visible parameters (id, name, value) and stop
//!
//! The lines it prints are for people and for grep: nothing here is a stable format.

use std::path::PathBuf;
use std::time::Instant;
use tunekore_plugins::offline::{self, Input, RenderSpec};
use tunekore_plugins::*;

struct Args {
    bundle: PathBuf,
    name: Option<String>,
    class: Option<String>,
    seconds: f64,
    out: Option<PathBuf>,
    notes: bool,
    sine: bool,
    all_buses: bool,
    params: Vec<(ParamId, f64)>,
    state_check: bool,
    list_params: usize,
}

fn parse() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let bundle = PathBuf::from(it.next().ok_or("usage: render <plugin.vst3> [options]")?);
    let mut a = Args { bundle, name: None, class: None, seconds: 4.0, out: None, notes: true, sine: false, all_buses: false, params: Vec::new(), state_check: false, list_params: 0 };
    while let Some(x) = it.next() {
        let mut value = |what: &str| it.next().ok_or(format!("{x} needs {what}"));
        match x.as_str() {
            "--name" => a.name = Some(value("a name")?),
            "--class" => a.class = Some(value("a class id")?),
            "--seconds" => a.seconds = value("a number")?.parse().map_err(|_| "--seconds needs a number")?,
            "--out" => a.out = Some(PathBuf::from(value("a file")?)),
            "--no-notes" => a.notes = false,
            "--input" => a.sine = value("sine")? == "sine",
            "--all-buses" => a.all_buses = true,
            "--param" => {
                let v = value("ID=VALUE")?;
                let (id, val) = v.split_once('=').ok_or("--param needs ID=VALUE")?;
                a.params.push((id.parse().map_err(|_| "bad parameter id")?, val.parse().map_err(|_| "bad parameter value")?));
            }
            "--state-check" => a.state_check = true,
            "--list-params" => a.list_params = value("a number")?.parse().map_err(|_| "--list-params needs a number")?,
            o => return Err(format!("unknown option {o}")),
        }
    }
    Ok(a)
}

fn pick(a: &Args) -> Result<PluginInfo, String> {
    let all = vst3::classes(&a.bundle).map_err(|e| e.to_string())?;
    if all.is_empty() {
        return Err("no audio plugins in this bundle".into());
    }
    let found = match (&a.name, &a.class) {
        (Some(n), _) => all.iter().find(|i| i.name.eq_ignore_ascii_case(n)),
        (_, Some(c)) => all.iter().find(|i| i.class_id.eq_ignore_ascii_case(c)),
        _ => all.first(),
    };
    found.cloned().ok_or_else(|| format!("not in this bundle; it has: {}", all.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join(", ")))
}

fn describe(layout: &BusLayout) -> String {
    let buses = |bs: &[BusInfo]| bs.iter().map(|b| format!("{} {}ch {:?}{}", b.name, b.channels, b.kind, if b.active { "" } else { " off" })).collect::<Vec<_>>().join("; ");
    format!("in [{}] out [{}] events in {} out {}", buses(&layout.audio_inputs), buses(&layout.audio_outputs), layout.event_inputs, layout.event_outputs)
}

fn spec(a: &Args) -> RenderSpec {
    let sel = if a.all_buses { BusSelection::All } else { BusSelection::Main };
    RenderSpec {
        seconds: a.seconds,
        notes: if a.notes { RenderSpec::arpeggio() } else { Vec::new() },
        input: if a.sine { Input::Sine { hz: 220.0, level: 0.25 } } else { Input::Silence },
        inputs: sel,
        outputs: sel,
        params: a.params.clone(),
        ..RenderSpec::default()
    }
}

fn level(r: &offline::Rendered) -> String {
    format!("peak {:.1} dBFS, rms {:.1} dBFS", offline::db(f64::from(r.peak())), offline::db(r.rms()))
}

fn main() {
    run();
    // Unload the plugins' modules before the process exits (see `vst3::shutdown`).
    vst3::shutdown();
}

fn run() {
    let a = match parse() {
        Ok(a) => a,
        Err(e) => return eprintln!("error: {e}"),
    };
    let info = match pick(&a) {
        Ok(i) => i,
        Err(e) => return eprintln!("error: {e}"),
    };
    println!("plugin: {} ({}) {} [{:?}] class {}", info.name, info.vendor, info.version, info.category, info.class_id);
    let t = Instant::now();
    let mut c = match vst3::load(&info) {
        Ok(c) => c,
        Err(e) => return println!("load failed: {e}"),
    };
    println!("load: {} ms, {} parameters, editor: {}", t.elapsed().as_millis(), c.params().len(), if c.has_editor() { "yes" } else { "no" });
    println!("buses: {}", describe(c.default_layout()));
    if a.list_params > 0 {
        let visible: Vec<ParamInfo> = c.params().iter().filter(|p| !p.hidden).take(a.list_params).cloned().collect();
        for p in visible {
            let v = c.param_value(p.id).unwrap_or(p.default);
            println!("param {:>10}  {:<24} {:.3} = {:<16} range {}..{} {}{}", p.id, p.name, v, c.param_text(p.id, v).unwrap_or_default(), p.min, p.max, p.units, if p.automatable { "" } else { " (not automatable)" });
        }
        return;
    }
    let spec = spec(&a);
    let first = match offline::render(&mut *c, &spec) {
        Ok(r) => r,
        Err(e) => return println!("render failed: {e}"),
    };
    println!("latency: {} samples, tail: {} samples", c.latency_samples(), c.tail_samples());
    println!("active: {}", describe(&first.layout));
    println!("render: {:.1} s at {} Hz, {} blocks of {}, mean {} us, slowest {} us per block (budget {} us)", a.seconds, spec.sample_rate, first.blocks, spec.block_size, first.mean_block_micros, first.max_block_micros, (spec.block_size as f64 / spec.sample_rate * 1e6) as u64);
    println!("output: {}, {} note(s) sent out", level(&first), first.midi_out);
    for bus in 0..first.layout.audio_outputs.len() {
        if first.layout.audio_outputs[bus].active && first.layout.audio_outputs.len() > 1 {
            println!("  output bus {bus} ({}): peak {:.1} dBFS", first.layout.audio_outputs[bus].name, offline::db(f64::from(first.bus_peak(bus))));
        }
    }
    if let Some(path) = &a.out {
        match wav::write_f32(path, spec.sample_rate as u32, &first.channels) {
            Ok(()) => println!("wrote {}", path.display()),
            Err(e) => println!("cannot write {}: {e}", path.display()),
        }
    }
    if a.state_check {
        let state = match c.save_state() {
            Ok(s) => s,
            Err(e) => return println!("save state failed: {e}"),
        };
        println!("state: saved {} bytes", state.len());
        let mut fresh = match vst3::load(&info) {
            Ok(c) => c,
            Err(e) => return println!("second load failed: {e}"),
        };
        if let Err(e) = fresh.load_state(&state) {
            return println!("restore failed: {e}");
        }
        // The state carries the parameters; they are not set again.
        let again = RenderSpec { params: Vec::new(), ..spec.clone() };
        match offline::render(&mut *fresh, &again) {
            Ok(second) => {
                let d = offline::max_difference(&first, &second);
                println!("state-check: restored into a fresh instance: {}; largest difference {d:e}", level(&second));
                println!("state-check: {}", if d == 0.0 { "identical" } else if d < 1e-4 { "same within 1e-4 (the plugin is not bit-exact between runs)" } else { "DIFFERENT" });
            }
            Err(e) => return println!("restored render failed: {e}"),
        }
        if !a.params.is_empty() {
            let mut plain = vst3::load(&info).expect("a third load");
            if let Ok(third) = offline::render(&mut *plain, &RenderSpec { params: Vec::new(), ..spec.clone() }) {
                println!("state-check: control without the state: largest difference from the first render {:e}", offline::max_difference(&first, &third));
            }
        }
    }
}
