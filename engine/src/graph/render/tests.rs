use super::*;
use crate::clip::{self, ClipFeed, ClipSet, ClipSpec};
use crate::device::{Buffers, Device, Events};
use crate::player::Track;
use crate::sampler::PadParams;
use crate::testing::realtime;
use crate::track::TrackMix;
use crate::transport::ProcessContext;
use crate::{Engine, SAMPLER_TRACK};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
/// Frames per beat at 120 BPM and 48 kHz.
const BEAT: usize = 24_000;

/// Adds a constant to both sides: a stand-in instrument that never goes quiet.
struct Dc(f32);
impl Device for Dc {
    fn process(&mut self, _: &ProcessContext, _: &mut Events, audio: Buffers<'_>) {
        audio.left.iter_mut().for_each(|x| *x += self.0);
        audio.right.iter_mut().for_each(|x| *x += self.0);
    }
}

/// Scales both sides: a stand-in effect.
struct Gain(f32);
impl Device for Gain {
    fn process(&mut self, _: &ProcessContext, _: &mut Events, audio: Buffers<'_>) {
        audio.left.iter_mut().for_each(|x| *x *= self.0);
        audio.right.iter_mut().for_each(|x| *x *= self.0);
    }
}

/// Writes each frame's position as the transport numbers it on the left, and
/// on the right 1 when its beat is inside `start..end`, -1 when not.
struct Probe {
    start: f64,
    end: f64,
}
impl Device for Probe {
    fn process(&mut self, ctx: &ProcessContext, _: &mut Events, audio: Buffers<'_>) {
        for i in 0..ctx.frames {
            audio.left[i] += (ctx.frame + i as u64) as f32;
            let b = ctx.beat_at(i);
            audio.right[i] += if b >= self.start && b < self.end { 1.0 } else { -1.0 };
        }
    }
}

/// 0.5 while the transport plays; after a stop it dies away, below the floor
/// after about half a second at 48 kHz: a stand-in reverb.
struct Ring(f32);
impl Device for Ring {
    fn process(&mut self, ctx: &ProcessContext, _: &mut Events, audio: Buffers<'_>) {
        for i in 0..ctx.frames {
            self.0 = if ctx.playing { 0.5 } else { self.0 * 0.999_55 };
            audio.left[i] += self.0;
            audio.right[i] += self.0;
        }
    }
}

/// Rises to 0.5 over 1,000 frames while playing and falls back to 0 when
/// stopped: a device whose sound depends on what it played before.
struct Swell(f32);
impl Device for Swell {
    fn process(&mut self, ctx: &ProcessContext, _: &mut Events, audio: Buffers<'_>) {
        for i in 0..ctx.frames {
            self.0 = if ctx.playing { (self.0 + 0.0005).min(0.5) } else { (self.0 - 0.0005).max(0.0) };
            audio.left[i] += self.0;
            audio.right[i] += self.0;
        }
    }
}

/// What a render wrote: the master and each stem, left and right, cut to
/// the render's length.
struct Files {
    master: [Vec<f32>; 2],
    stems: Vec<[Vec<f32>; 2]>,
}

fn append(file: &mut [Vec<f32>; 2], part: Part<'_>) {
    if part.left.is_empty() {
        return;
    }
    assert_eq!(part.first as usize, file[0].len(), "each block carries on where the last stopped");
    file[0].extend_from_slice(part.left);
    file[1].extend_from_slice(part.right);
}

/// Runs a render to its end, as an export does, and cuts the files to its length.
fn collect(render: &mut Render) -> Files {
    let mut f = Files { master: [Vec::new(), Vec::new()], stems: (0..render.stems()).map(|_| [Vec::new(), Vec::new()]).collect() };
    while render.next() {
        append(&mut f.master, render.master());
        for i in 0..render.stems() {
            append(&mut f.stems[i], render.stem(i));
        }
    }
    let len = render.length().expect("the render finished") as usize;
    for file in std::iter::once(&mut f.master).chain(f.stems.iter_mut()) {
        assert!(file[0].len() >= len, "every file reached the end: {} < {len}", file[0].len());
        file[0].truncate(len);
        file[1].truncate(len);
    }
    f
}

/// A project with no plugins: the Sampler playing two samples (one at
/// another rate, resampled), an audio track with two clips, and a track with
/// a chain, a level and a pan. Returns the clip track's feed and the tracks.
fn project(engine: &mut Engine) -> (ClipFeed, Vec<TrackId>) {
    let kick: Track = Arc::new(vec![(0..6_000).map(|i| (i as f32 * 0.03).sin() * (1.0 - i as f32 / 6_000.0)).collect()]);
    let hat: Track = Arc::new(vec![
        (0..900).map(|i| if i % 7 < 3 { 0.4 } else { -0.3 }).collect(),
        (0..900).map(|i| if i % 5 < 2 { -0.4 } else { 0.2 }).collect(),
    ]);
    engine.load_sample(0, Some((RATE, kick))).unwrap();
    engine.load_sample(1, Some((44_100, hat))).unwrap();
    let pads = engine.sampler();
    pads.set_pattern(0, 0x1111);
    pads.set_pattern(1, 0x4444);
    pads.set_pad(1, &PadParams { gain: 0.8, pan: -0.4, ..Default::default() });

    let clips = engine.mixer().add_track("Clips").unwrap();
    let (player, mut feed) = clip::player();
    engine.mixer().set_source(clips, Some(Box::new(player))).unwrap();
    let tone: Track = Arc::new(vec![(0..RATE as usize).map(|i| 0.4 * (i as f32 * 0.0577).sin()).collect()]);
    let a = ClipSpec { start: 0.5, offset: 0.1, content: 0.3, phase: 0.0, length: 0.8, fade_in: 0.01, fade_out: 0.02, gain: 0.9 };
    let b = ClipSpec { start: 2.75, offset: 0.0, content: 1.0, phase: 0.0, length: 0.6, fade_in: 0.0, fade_out: 0.0, gain: 1.1 };
    feed.publish(ClipSet::new(RATE, [(a, tone.clone()), (b, tone)]));

    let chain = engine.mixer().add_track("Chain").unwrap();
    engine.mixer().set_source(chain, Some(Box::new(Dc(0.2)))).unwrap();
    engine.mixer().insert(chain, 0, Box::new(Gain(0.5))).unwrap();
    engine.mixer().set_mix(chain, TrackMix { volume: 0.7, pan: 0.3, ..Default::default() }).unwrap();
    (feed, vec![SAMPLER_TRACK, clips, chain])
}

fn spec(start: f64, end: f64, stems: Vec<TrackId>, block: usize) -> RenderSpec {
    RenderSpec { start, end, tail_seconds: 0.0, stems, block }
}

/// The output callback lends its graph and takes it back; this plays one
/// in a thread of its own, as the device's audio thread would.
struct Callback {
    stop: Arc<AtomicBool>,
    /// Callbacks that heard the graph (the Dc track), and callbacks in all.
    heard: Arc<AtomicUsize>,
    runs: Arc<AtomicUsize>,
    thread: std::thread::JoinHandle<(crate::Output, usize)>,
}

impl Callback {
    fn start(mut out: crate::Output, level: f32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let heard = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let (s, h, r) = (stop.clone(), heard.clone(), runs.clone());
        let thread = std::thread::spawn(move || {
            let mut buf = vec![0.0f32; 256 * 2];
            let mut allocs = 0;
            while !s.load(Ordering::Relaxed) {
                let (_, n) = realtime(|| out.render(&mut buf));
                allocs += n;
                if buf.iter().all(|&x| x == level) {
                    h.fetch_add(1, Ordering::Relaxed);
                }
                r.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_micros(500));
            }
            (out, allocs)
        });
        Self { stop, heard, runs, thread }
    }

    /// Waits until the callback has heard the graph `n` more times.
    fn hears(&self, n: usize) {
        let from = self.heard.load(Ordering::Relaxed);
        let t = Instant::now();
        while self.heard.load(Ordering::Relaxed) < from + n {
            assert!(t.elapsed() < Duration::from_secs(5), "the callback never played the graph");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn stop(self) -> (crate::Output, usize) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().unwrap()
    }
}

#[test]
fn the_render_equals_the_live_callback_sample_for_sample() {
    // Blocks that end on the range's end, where the render stops its transport.
    const BLOCK: usize = 500;
    const BEATS: f64 = 4.0;
    let blocks = BEATS as usize * BEAT / BLOCK;

    // Live: the output callback as a device runs it, from play to a stop on
    // the range's last frame, then on for the limiter's lookahead.
    let mut live = Engine::new();
    let (mut out, _voice) = live.output(RATE, 2);
    let _feed_a = project(&mut live);
    live.set_master(1.6); // loud enough that the limiter works
    live.transport().play();
    let mut heard = Vec::with_capacity((blocks + 1) * BLOCK * 2);
    let mut buf = vec![0.0f32; BLOCK * 2];
    for b in 0..=blocks {
        if b == blocks {
            live.transport().stop();
        }
        out.render(&mut buf);
        heard.extend_from_slice(&buf);
    }
    let limiting = live.meter().take_limiting();
    assert!(limiting < 0.9, "the limiter worked: {limiting}");

    // Offline: the same project, rendered in the same blocks.
    let mut offline = Engine::new();
    let (_feed_b, tracks) = project(&mut offline);
    offline.set_master(1.6);
    let mut render = offline.start_render(spec(0.0, BEATS, tracks, BLOCK)).unwrap();
    let latency = render.master_latency as usize;
    assert_eq!(latency, Limiter::new(RATE, 2).latency(), "the limiter's lookahead, no delay compensation yet");
    let files = collect(&mut render);
    drop(render);
    offline.finish_render();

    let [l, r] = &files.master;
    assert_eq!(l.len(), BEATS as usize * BEAT);
    // The file starts on the range's first frame: the callback's output is
    // the same sound, later by the lookahead.
    let frames = l.len();
    assert!(heard.len() / 2 >= frames + latency);
    let mut differ = 0;
    for k in 0..frames {
        if l[k] != heard[(k + latency) * 2] || r[k] != heard[(k + latency) * 2 + 1] {
            differ += 1;
        }
    }
    assert_eq!(differ, 0, "{differ} of {frames} frames differ from the live callback");
    assert!(l.iter().any(|&x| x.abs() > 0.5), "the project made sound");
    assert_eq!(heard[..latency * 2].iter().filter(|&&x| x != 0.0).count(), 0, "nothing comes out of the limiter before its lookahead");
    drop(out);
    assert!(live.mixer.stopped());
    assert!(offline.mixer().track(SAMPLER_TRACK).is_some_and(|t| t.has_source), "the devices came home");
}

#[test]
fn the_stems_line_up_with_the_master_and_add_up_to_it() {
    let mut engine = Engine::new();
    let (_feed, tracks) = project(&mut engine);
    engine.set_master(1.0);
    // Quiet enough that the limiter leaves it alone.
    engine.mixer().set_mix(SAMPLER_TRACK, TrackMix { volume: 0.25, ..Default::default() }).unwrap();
    engine.mixer().set_mix(tracks[1], TrackMix { volume: 0.5, pan: -0.6, ..Default::default() }).unwrap();
    let mut render = engine.start_render(spec(0.0, 4.0, tracks, MAX_FRAMES)).unwrap();
    let f = collect(&mut render);
    let len = 4 * BEAT;
    assert_eq!(f.master[0].len(), len);
    assert!(f.stems.iter().all(|s| s[0].len() == len && s[1].len() == len), "every file is as long as the master");
    for side in 0..2 {
        for k in 0..len {
            // Summed in the graph's order, from silence, as the master is.
            let sum = f.stems.iter().fold(0.0f32, |acc, s| acc + s[side][k]);
            assert_eq!(sum, f.master[side][k], "side {side}, frame {k}");
        }
    }
    for (i, s) in f.stems.iter().enumerate() {
        assert!(s[0].iter().chain(&s[1]).any(|&x| x != 0.0), "stem {i} has sound");
    }
    // Post-fader: the chain track at 0.7, panned right by 0.3 (its left side falls).
    assert_eq!(f.stems[2][1][100], 0.2 * 0.5 * 0.7);
    assert!((f.stems[2][0][100] - 0.2 * 0.5 * 0.7 * 0.7).abs() < 1e-7);
}

#[test]
fn a_range_is_cut_at_its_exact_frames() {
    for (rate, tempo, start, end) in [(44_100, 133.0, 1.37, 5.11), (48_000, 120.0, 4.0, 8.0), (96_000, 97.5, 0.333, 0.9)] {
        let mut engine = Engine::new();
        // An output once opened at `rate`: renders run at the rate the devices last ran at.
        let (out, _voice) = engine.output(rate, 2);
        drop(out);
        engine.after_stream();
        engine.transport().set_tempo(tempo);
        let t = engine.mixer().add_track("Probe").unwrap();
        engine.mixer().set_source(t, Some(Box::new(Probe { start, end }))).unwrap();
        let mut render = engine.start_render(spec(start, end, vec![t], 1_000)).unwrap();
        assert_eq!(render.rate(), rate);
        let f = collect(&mut render);
        drop(render);
        engine.finish_render();

        let map = engine.transport().tempo_map(rate);
        let len = range_frames(&map, start, end) as usize;
        let exact = map.frames_between(start, end);
        assert!(len as f64 >= exact && (len as f64) < exact + 1.0, "{len} frames for {exact}");
        let [pos, inside] = &f.stems[0];
        assert_eq!((pos.len(), f.master[0].len()), (len, len), "rate {rate}: the master and the stem are the range's length");
        assert_eq!(pos[0], map.frame_of_beat(start).round() as f32, "the first frame is the start beat's");
        assert!(pos.windows(2).all(|w| w[1] == w[0] + 1.0), "no frame dropped or doubled");
        assert!(inside.iter().all(|&x| x == 1.0), "every frame's beat is inside the range, and none is missing at either end");
    }
}

#[test]
fn a_step_on_the_ranges_start_lands_on_its_first_frame() {
    let mut engine = Engine::new();
    let hit: Track = Arc::new(vec![(0..3_000).map(|i| 0.5 * (i as f32 * 0.02).sin() + 0.1).collect()]);
    engine.load_sample(0, Some((RATE, hit))).unwrap();
    engine.sampler().set_pattern(0, 0x1111); // a hit on every beat
    let whole = collect(&mut engine.start_render(spec(0.0, 4.0, vec![SAMPLER_TRACK], 700)).unwrap());
    engine.finish_render();
    let bar = collect(&mut engine.start_render(spec(1.0, 3.0, vec![SAMPLER_TRACK], 700)).unwrap());
    engine.finish_render();
    let (w, b) = (&whole.stems[0], &bar.stems[0]);
    assert_eq!(b[0].len(), 2 * BEAT);
    assert!(b[0][0] != 0.0, "the hit on beat 1 starts the file");
    for k in 0..2 * BEAT {
        assert!((b[0][k] - w[0][BEAT + k]).abs() < 1e-6 && (b[1][k] - w[1][BEAT + k]).abs() < 1e-6, "frame {k}");
    }
}

#[test]
fn the_tail_rings_out_and_ends_at_the_last_sound() {
    let mut engine = Engine::new();
    let t = engine.mixer().add_track("Reverb").unwrap();
    engine.mixer().set_source(t, Some(Box::new(Ring(0.0)))).unwrap();
    let mut render = engine.start_render(RenderSpec { start: 0.0, end: 2.0, tail_seconds: 10.0, stems: vec![t], block: MAX_FRAMES }).unwrap();
    let f = collect(&mut render);
    let len = f.master[0].len();
    // 0.5 × 0.99955^n falls under the floor after about 24,000 frames.
    let ring = ((TAIL_FLOOR as f64 / 0.5).ln() / 0.999_55f64.ln()).ceil() as usize;
    assert!(len.abs_diff(2 * BEAT + ring) < 50, "{len} frames, the ring lasting about {ring}");
    assert!(f.stems[0][0][len - 1] > TAIL_FLOOR && f.master[0][len - 1] > TAIL_FLOOR, "it ends on its last sound");
    assert_eq!(f.master[0][2 * BEAT - 1], 0.5, "played to the range's end");
    assert!(f.master[0][2 * BEAT + 1] < 0.5, "then the transport stopped");
    let rendered = render.ran as usize - render.master_latency as usize;
    assert!(rendered < len + (TAIL_QUIET * f64::from(RATE)) as usize + MAX_FRAMES, "it stopped once quiet, not at the longest tail");
}

#[test]
fn a_tail_that_never_ends_stops_at_its_longest_and_fades_out() {
    let mut engine = Engine::new();
    let t = engine.mixer().add_track("Feedback").unwrap();
    engine.mixer().set_source(t, Some(Box::new(Dc(0.25)))).unwrap();
    let mut render = engine.start_render(RenderSpec { start: 0.0, end: 1.0, tail_seconds: 0.5, stems: vec![t], block: MAX_FRAMES }).unwrap();
    let f = collect(&mut render);
    let s = &f.stems[0][0];
    let fade = (TAIL_FADE * f64::from(RATE)).round() as usize;
    assert_eq!(s.len(), BEAT + RATE as usize / 2);
    assert_eq!(s[s.len() - fade - 1], 0.25, "full level until the fade");
    assert_eq!(s[s.len() - fade], 0.25, "the fade's first frame");
    assert_eq!(s[s.len() - 1], 0.25 / fade as f32, "faded to nearly nothing on the last frame");
    assert_eq!(f.master[0][s.len() - 1], 0.25 / fade as f32, "the master fades with it");
    drop(render);
    engine.finish_render();

    // With no tail the file stops on the range's last frame, uncut by any fade.
    let f = collect(&mut engine.start_render(spec(0.0, 1.0, vec![t], MAX_FRAMES)).unwrap());
    assert_eq!(f.stems[0][0].len(), BEAT);
    assert_eq!((f.stems[0][0][BEAT - 1], f.master[0][BEAT - 1]), (0.25, 0.25));
}

#[test]
fn a_render_never_allocates_per_block() {
    let mut engine = Engine::new();
    let (_feed, tracks) = project(&mut engine);
    let ring = engine.mixer().add_track("Ring").unwrap();
    engine.mixer().set_source(ring, Some(Box::new(Ring(0.0)))).unwrap();
    let mut all = tracks.clone();
    all.push(ring);
    let mut render = engine.start_render(RenderSpec { start: 0.5, end: 6.0, tail_seconds: 3.0, stems: all, block: 333 }).unwrap();
    let (mut blocks, mut allocs) = (0, 0);
    loop {
        let (more, n) = realtime(|| render.next());
        allocs += n;
        if !more {
            break;
        }
        blocks += 1;
        // Reading the block's files allocates nothing either.
        let (_, n) = realtime(|| {
            let mut peak = render.master().left.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            for i in 0..render.stems() {
                peak = render.stem(i).right.iter().fold(peak, |m, x| m.max(x.abs()));
            }
            std::hint::black_box(peak)
        });
        allocs += n;
    }
    assert!(blocks > 100);
    assert_eq!(allocs, 0, "the render allocated or freed {allocs} times");
    assert_eq!(render.progress().phase, Phase::Done);
}

#[test]
fn a_render_borrows_the_running_graph_and_the_output_plays_on_after() {
    let mut engine = Engine::new();
    let (out, _voice) = engine.output(RATE, 2);
    let t = engine.mixer().add_track("Dc").unwrap();
    engine.mixer().set_source(t, Some(Box::new(Dc(0.25)))).unwrap();
    // The Sampler's track is silent: the callback hears 0.25 while it has the graph.
    let cb = Callback::start(out, 0.25);
    cb.hears(3);
    engine.transport().play();
    let mut render = engine.start_render(spec(0.0, 1.0, vec![t], MAX_FRAMES)).unwrap();
    assert!(engine.is_rendering());
    assert!(!engine.transport().is_playing(), "a render stops playback");
    assert!(engine.start_render(spec(0.0, 1.0, vec![], MAX_FRAMES)).is_err(), "one render at a time");
    let heard = cb.heard.load(Ordering::Relaxed);
    let runs = cb.runs.load(Ordering::Relaxed);
    let f = collect(&mut render);
    assert!(f.stems[0][0].iter().all(|&x| x == 0.25), "the render ran the output's devices");
    // While the render held the graph, the callback ran on without it.
    let t0 = Instant::now();
    while cb.runs.load(Ordering::Relaxed) < runs + 5 {
        assert!(t0.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(cb.heard.load(Ordering::Relaxed), heard, "it never heard the graph while lent");
    drop(render);
    assert!(!engine.is_rendering());
    cb.hears(3); // the graph is back in the callback
    let (out, allocs) = cb.stop();
    assert_eq!(allocs, 0, "lending and taking back the graph allocated nothing on the audio thread");
    drop(out);
    engine.after_stream();
    assert_eq!(engine.mixer().track(t).map(|i| i.has_source), Some(true), "the devices came home");
}

#[test]
fn closing_the_output_cancels_a_render_and_its_devices_come_home() {
    let mut engine = Engine::new();
    let (out, _voice) = engine.output(RATE, 2);
    let t = engine.mixer().add_track("Dc").unwrap();
    engine.mixer().set_source(t, Some(Box::new(Dc(0.25)))).unwrap();
    let cb = Callback::start(out, 0.25);
    cb.hears(2);
    let render = engine.start_render(spec(0.0, 100_000.0, vec![t], 64)).unwrap();
    let worker = std::thread::spawn(move || {
        let mut render = render;
        let mut blocks = 0;
        while render.next() {
            blocks += 1;
        }
        (render.cancelled(), blocks, render.length())
    });
    // The output closes while the render runs: its callback goes first.
    std::thread::sleep(Duration::from_millis(30));
    let (out, _) = cb.stop();
    drop(out);
    engine.close_output();
    let (cancelled, blocks, length) = worker.join().unwrap();
    assert!(cancelled && blocks > 0 && length.is_none(), "the render stopped part way: {blocks} blocks");
    assert!(!engine.is_rendering());
    // As closing a stream does: the graph the render gave back, never taken
    // in by the gone callback, is dropped and its devices come home.
    engine.after_stream();
    assert_eq!(engine.mixer().track(t).map(|i| i.has_source), Some(true), "no device was lost");
    assert!(engine.mixer().track(SAMPLER_TRACK).is_some_and(|i| i.has_source));
}

#[test]
fn what_still_sounds_from_live_play_never_reaches_the_render() {
    let fresh = {
        let mut engine = Engine::new();
        let t = engine.mixer().add_track("Swell").unwrap();
        engine.mixer().set_source(t, Some(Box::new(Swell(0.0)))).unwrap();
        collect(&mut engine.start_render(spec(0.0, 1.0, vec![t], MAX_FRAMES)).unwrap())
    };
    let mut engine = Engine::new();
    let (out, _voice) = engine.output(RATE, 2);
    let t = engine.mixer().add_track("Swell").unwrap();
    engine.mixer().set_source(t, Some(Box::new(Swell(0.0)))).unwrap();
    engine.transport().play();
    let cb = Callback::start(out, 0.5);
    cb.hears(2); // played long enough to swell all the way up
    let after_play = collect(&mut engine.start_render(spec(0.0, 1.0, vec![t], MAX_FRAMES)).unwrap());
    assert_eq!(fresh.stems[0][0][0], 0.0005, "a fresh render swells from nothing");
    assert_eq!(after_play.stems[0][0], fresh.stems[0][0], "the swell from live play died away before the render started");
    let (out, _) = cb.stop();
    drop(out);
    engine.after_stream();
}

/// Builds the repo's VST3 test plugin (`plugins/tests/fixture`, a cdylib
/// outside the workspace) once, as the plugin host's own tests do, and
/// installs it as a `.vst3` bundle.
#[cfg(target_os = "macos")]
fn fixture_bundle() -> &'static std::path::Path {
    use std::path::{Path, PathBuf};
    static BUNDLE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUNDLE.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/tests/fixture/Cargo.toml");
        // This binary is <target>/debug/deps/...; cargo's scratch space is <target>/tmp.
        let exe = std::env::current_exe().unwrap();
        let tmp = exe.ancestors().nth(3).unwrap().join("tmp");
        let target = tmp.join("vst3-fixture-target");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let st = std::process::Command::new(cargo)
            .args(["build", "--quiet", "--locked", "--manifest-path"])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&target)
            .env_remove("RUSTFLAGS")
            .status()
            .expect("run cargo");
        assert!(st.success(), "building the VST3 fixture failed");
        let bundle = tmp.join(format!("vst3-fixture-engine-{}", std::process::id())).join("Fixture.vst3");
        let bin = bundle.join("Contents/MacOS");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::copy(target.join("debug/libvst3_fixture.dylib"), bin.join("Fixture")).unwrap();
        bundle
    })
}

#[cfg(target_os = "macos")]
#[test]
fn a_project_with_plugins_renders_through_them_in_offline_mode() {
    use crate::plugin_device::{plugin_device, plugin_device_for_pad, Role};
    use tunekore_plugins::{vst3, ProcessSetup};

    let classes = vst3::classes(fixture_bundle()).unwrap();
    let load = |name: &str| vst3::load(classes.iter().find(|c| c.name == name).unwrap()).unwrap();
    // As an export will activate them (kOffline): see the report's follow-up for the shell's part.
    let offline = ProcessSetup { sample_rate: f64::from(RATE), max_block_size: MAX_FRAMES as u32, realtime: false, ..Default::default() };
    let mut gain = load("Fixture Gain");
    let mut synth = load("Fixture Synth");

    let mut engine = Engine::new();
    // An effect: a constant through Fixture Gain, x1.8 from the edit its controller makes on load.
    let fx = engine.mixer().add_track("Fx").unwrap();
    engine.mixer().set_source(fx, Some(Box::new(Dc(0.25)))).unwrap();
    let (mut effect, _effect_link) = plugin_device(Role::Effect, Box::new(|| {}));
    effect.install(gain.activate(&offline).unwrap(), RATE);
    engine.mixer().insert(fx, 0, Box::new(effect)).unwrap();
    // An instrument: Fixture Synth on Sampler pad 3, played by the pattern's first step.
    let pads = engine.sampler();
    pads.set_plugin_pad(2, true);
    pads.set_pattern(2, 1);
    let p = pads.clone();
    let (mut pad, _pad_link) = plugin_device_for_pad(2, Box::new(move || p.pad_gains(2)), Box::new(|| {}));
    pad.install(synth.activate(&offline).unwrap(), RATE);
    engine.mixer().insert(SAMPLER_TRACK, 0, Box::new(pad)).unwrap();

    let mut render = engine.start_render(spec(0.0, 2.0, vec![SAMPLER_TRACK, fx], MAX_FRAMES)).unwrap();
    let f = collect(&mut render);
    drop(render);
    engine.finish_render();

    assert_eq!(f.master[0].len(), 2 * BEAT);
    let wet = &f.stems[1][0];
    assert!(wet.iter().all(|&x| (x - 0.45).abs() < 1e-6), "the effect plugin ran on its track: {}", wet[0]);
    // The synth plays the held note's velocity: a sixteenth (6,000 frames at 120 BPM) from the first frame.
    let sixteenth = BEAT / 4;
    let s = &f.stems[0][0];
    assert!(s[0] > 0.0 && s[..sixteenth].iter().all(|&x| x == s[0]), "the synth sounds from the step's first frame: {}", s[0]);
    assert!(s[sixteenth..].iter().all(|&x| x == 0.0), "and stops with the step");
    assert_eq!(engine.mixer().track(fx).map(|t| t.chain), Some(1), "the plugin came home on its track");
    // The processors go before their controllers.
    drop(engine);
    drop((gain, synth));
}

/// The same through a real output device's callback. Skips on CI and on
/// machines with no output, as the engine's other device tests do.
#[test]
fn a_render_borrows_the_graph_from_a_real_output() {
    if std::env::var_os("CI").is_some() {
        return;
    }
    let mut engine = Engine::new();
    if engine.open_output(None).is_err() {
        eprintln!("no output device; skipping");
        return;
    }
    let t = engine.mixer().add_track("Dc").unwrap();
    engine.mixer().set_source(t, Some(Box::new(Dc(0.05)))).unwrap();
    let meter = engine.meter();
    std::thread::sleep(Duration::from_millis(150));
    assert!(meter.take_peak() > 0.04, "the output plays the graph");
    let mut render = engine.start_render(spec(0.0, 2.0, vec![t], MAX_FRAMES)).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    meter.take_peak();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(meter.take_peak(), 0.0, "while lent, the output plays nothing of the graph");
    let f = collect(&mut render);
    assert!(f.stems[0][0].iter().all(|&x| x == 0.05));
    assert_eq!(render.rate(), engine.output_rate(), "at the output's rate");
    drop(render);
    std::thread::sleep(Duration::from_millis(150));
    assert!(meter.take_peak() > 0.04, "the graph is back on the output");
    // Closing while a render holds the graph takes it back too.
    let render = engine.start_render(spec(0.0, 100_000.0, vec![t], 64)).unwrap();
    let worker = std::thread::spawn(move || {
        let mut render = render;
        while render.next() {}
        render.cancelled()
    });
    std::thread::sleep(Duration::from_millis(30));
    engine.close_output();
    assert!(worker.join().unwrap(), "the render was cancelled");
    assert_eq!(engine.mixer().track(t).map(|i| i.has_source), Some(true), "no device was lost");
}
