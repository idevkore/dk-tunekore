use super::wav::{part_path, Writer};
use super::*;
use std::sync::atomic::AtomicUsize;
use tunekore_engine::{Buffers, Device, Engine, Events, ProcessContext, TrackKind, TrackMix};

const RATE: u32 = 48_000;
/// Frames per beat at 120 BPM.
const BEAT: usize = 24_000;

/// A constant on both sides while the transport plays: a stand-in instrument.
struct Dc(f32);
impl Device for Dc {
    fn process(&mut self, ctx: &ProcessContext, _: &mut Events, audio: Buffers<'_>) {
        if ctx.playing {
            audio.left.iter_mut().for_each(|x| *x += self.0);
            audio.right.iter_mut().for_each(|x| *x -= self.0);
        }
    }
}

/// A sine, by the transport's position: different on every frame.
struct Sine(f32);
impl Device for Sine {
    fn process(&mut self, ctx: &ProcessContext, _: &mut Events, audio: Buffers<'_>) {
        if ctx.playing {
            for i in 0..ctx.frames {
                let v = self.0 * ((ctx.frame + i as u64) as f32 * 0.013).sin();
                audio.left[i] += v;
                audio.right[i] += 0.5 * v;
            }
        }
    }
}

fn track(id: u32, name: &str, mix: TrackMix) -> TrackInfo {
    TrackInfo { id: TrackId(id), name: name.into(), kind: TrackKind::Audio, mix, has_source: true, chain: 0 }
}

fn request(path: &Path, master: bool, stems: bool, format: Format) -> Request {
    Request { range: Range::Song, format, master, stems, tail: false, path: path.display().to_string() }
}

/// A WAV file read back: (format tag, channels, rate, bits, the `fact` frame
/// count if any, the frames as f32 left and right).
struct Read {
    tag: u16,
    channels: u16,
    rate: u32,
    bits: u16,
    fact: Option<u32>,
    left: Vec<f32>,
    right: Vec<f32>,
}

fn read(path: &Path) -> Read {
    let b = std::fs::read(path).unwrap();
    assert_eq!((&b[..4], &b[8..12]), (&b"RIFF"[..], &b"WAVE"[..]));
    assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize, b.len() - 8, "the RIFF size is the file's");
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    let (mut at, mut fmt, mut fact, mut data) = (12, None, None, None);
    while at + 8 <= b.len() {
        let size = u32_at(at + 4) as usize;
        match &b[at..at + 4] {
            b"fmt " => fmt = Some(at + 8),
            b"fact" => fact = Some(u32_at(at + 8)),
            b"data" => data = Some((at + 8, size)),
            _ => {}
        }
        at += 8 + size + size % 2;
    }
    let f = fmt.unwrap();
    let (tag, channels, rate, bits) = (u16_at(f), u16_at(f + 2), u32_at(f + 4), u16_at(f + 14));
    assert_eq!(u32_at(f + 8), rate * u32::from(channels) * u32::from(bits / 8), "bytes per second");
    let (start, size) = data.unwrap();
    assert_eq!(start + size, b.len(), "the data chunk runs to the end");
    let bytes = usize::from(bits / 8);
    let samples: Vec<f32> = b[start..start + size]
        .chunks_exact(bytes)
        .map(|s| match (tag, bits) {
            (3, 32) => f32::from_le_bytes(s.try_into().unwrap()),
            (1, 16) => f32::from(i16::from_le_bytes([s[0], s[1]])) / 32_767.0,
            (1, 24) => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_607.0,
            other => panic!("unexpected format {other:?}"),
        })
        .collect();
    let left = samples.iter().step_by(2).copied().collect();
    let right = samples.iter().skip(1).step_by(2).copied().collect();
    Read { tag, channels, rate, bits, fact, left, right }
}

/// Files in `dir` and below, as paths relative to it, sorted.
fn listing(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            out.push(p.strip_prefix(dir).unwrap().display().to_string());
            if p.is_dir() {
                stack.push(p);
            }
        }
    }
    out.sort();
    out
}

// ---- Names and plans ----

#[test]
fn files_are_named_after_the_export_and_each_heard_track() {
    let dir = Path::new("/music/out");
    let tracks = [
        track(1, "Sampler", TrackMix::default()),
        track(2, "Bass: low/sub", TrackMix::default()),
        track(5, "Muted", TrackMix { mute: true, ..Default::default() }),
        track(6, "  ", TrackMix::default()),
    ];
    let p = plan(&request(&dir.join("Song.wav"), true, true, Format::Pcm24), &tracks, 0.0, 32.0, "Untitled").unwrap();
    assert_eq!(p.master.as_deref(), Some(dir.join("Song.wav").as_path()));
    assert_eq!(p.folder.as_deref(), Some(dir.join("Song stems").as_path()));
    let names: Vec<_> = p.stems.iter().map(|s| s.file_name().unwrap().to_string_lossy().to_string()).collect();
    assert_eq!(names, ["Song - 01 Sampler.wav", "Song - 02 Bass- low-sub.wav", "Song - 04 Track 4.wav"], "muted tracks are left out; numbers are places");
    assert_eq!(p.spec.stems, [TrackId(1), TrackId(2), TrackId(6)]);
    assert_eq!((p.spec.start, p.spec.end, p.spec.tail_seconds), (0.0, 32.0, 0.0));

    // Stems only: straight into the folder chosen, named after the project.
    let p = plan(&request(dir, false, true, Format::Pcm16), &tracks, 4.0, 8.0, "My: Beat").unwrap();
    assert_eq!((p.master.is_none(), p.folder.as_deref()), (true, Some(dir)));
    assert_eq!(p.stems[0], dir.join("My- Beat - 01 Sampler.wav"));

    // The master's name gets .wav; a tail asked for is the longest.
    let mut r = request(&dir.join("Mix.aiff"), true, false, Format::Float32);
    r.tail = true;
    let p = plan(&r, &tracks, 0.0, 4.0, "x").unwrap();
    assert_eq!((p.master.unwrap(), p.stems.len(), p.spec.tail_seconds), (dir.join("Mix.wav"), 0, TAIL_SECONDS));
}

#[test]
fn soloed_tracks_are_the_only_stems_and_an_empty_choice_is_refused() {
    let tracks = [track(1, "A", TrackMix::default()), track(2, "B", TrackMix { solo: true, ..Default::default() })];
    let p = plan(&request(Path::new("/x/S.wav"), true, true, Format::Pcm24), &tracks, 0.0, 1.0, "x").unwrap();
    assert_eq!(p.spec.stems, [TrackId(2)]);
    assert!(plan(&request(Path::new("/x/S.wav"), false, false, Format::Pcm24), &tracks, 0.0, 1.0, "x").is_err());
    assert!(plan(&request(Path::new(""), true, false, Format::Pcm24), &tracks, 0.0, 1.0, "x").is_err());
    let muted = [track(1, "A", TrackMix { mute: true, ..Default::default() })];
    assert!(plan(&request(Path::new("/x"), false, true, Format::Pcm24), &muted, 0.0, 1.0, "x").is_err(), "no stems to make");
}

#[test]
fn the_song_ends_at_the_last_heard_clip_or_the_samplers_song() {
    let view = serde_json::json!({ "tracks": [
        { "id": 1, "clips": [] },
        { "id": 2, "clips": [{ "start": 4.0, "length": 2.0 }, { "start": 0.0, "length": 1.0 }] },
        { "id": 3, "clips": [{ "start": 30.0, "length": 1.0 }] },
    ]});
    // 120 BPM: two seconds are four beats.
    assert_eq!(clips_end(&view, 120.0, |_| true), Some(32.0));
    assert_eq!(clips_end(&view, 120.0, |id| id != 3), Some(8.0), "a muted track's clips don't count");
    assert_eq!(clips_end(&view, 60.0, |id| id == 2), Some(6.0), "a clip's length in beats follows the tempo");
    assert_eq!(clips_end(&serde_json::json!({}), 120.0, |_| true), None);
    assert_eq!(song_end(Some(8.0), Some(16.0), Some(4.0)), Some(16.0));
    assert_eq!(song_end(Some(8.0), None, Some(4.0)), Some(8.0));
    assert_eq!(song_end(None, Some(16.0), Some(4.0)), Some(16.0));
    assert_eq!(song_end(None, None, Some(4.0)), Some(4.0), "failing both, one pass of the pattern");
    assert_eq!(song_end(None, None, None), None);

    let pads = SamplerShared::default();
    assert_eq!(sampler_lengths(&pads, true), (None, None), "an empty Sampler sets no end");
    pads.set_pattern(0, 1);
    pads.set_chain(&[0, 1, 0]);
    assert_eq!(sampler_lengths(&pads, true), (None, Some(4.0)), "16 steps: a bar");
    pads.set_song_mode(true);
    assert_eq!(sampler_lengths(&pads, true), (Some(12.0), Some(4.0)), "three entries of a bar");
    assert_eq!(sampler_lengths(&pads, false), (None, None), "a muted Sampler sets no end");
}

// ---- WAV files ----

#[test]
fn each_format_writes_a_wav_that_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let left: Vec<f32> = (0..1_000).map(|i| (i as f32 * 0.01).sin() * 0.9).collect();
    let right: Vec<f32> = left.iter().map(|x| -x * 0.5).collect();
    for (format, tag, bits, step) in [(Format::Pcm16, 1, 16, 1.0 / 32_767.0), (Format::Pcm24, 1, 24, 1.0 / 8_388_607.0), (Format::Float32, 3, 32, 0.0)] {
        let path = dir.path().join(format!("{format:?}.wav"));
        let mut w = Writer::create(&path, 44_100, format).unwrap();
        w.write(&left[..300], &right[..300]).unwrap();
        w.write(&left[300..], &right[300..]).unwrap();
        // Cut to 900 frames, as a tail that went quiet early is.
        let part = w.finish(900).unwrap();
        assert!(!path.exists(), "only the export moves it in place");
        std::fs::rename(&part, &path).unwrap();
        let r = read(&path);
        assert_eq!((r.tag, r.channels, r.rate, r.bits), (tag, 2, 44_100, bits), "{format:?}");
        assert_eq!(r.fact, (format == Format::Float32).then_some(900), "float files say their length in a fact chunk");
        assert_eq!(r.left.len(), 900);
        // 16-bit is dithered by up to a step either way, plus rounding.
        let tolerance = if format == Format::Pcm16 { step * 1.5 + 1e-7 } else { step * 0.5 + 1e-7 };
        for i in 0..900 {
            assert!((r.left[i] - left[i]).abs() <= tolerance && (r.right[i] - right[i]).abs() <= tolerance, "{format:?} frame {i}");
        }
    }
}

#[test]
fn integer_formats_count_what_they_clip_and_float_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let loud = [1.5f32, -2.0, 0.5];
    for (format, clipped, first) in [(Format::Pcm24, 2, 1.0), (Format::Float32, 0, 1.5)] {
        let path = dir.path().join("loud.wav");
        let mut w = Writer::create(&path, RATE, format).unwrap();
        w.write(&loud, &loud).unwrap();
        assert_eq!(w.clipped(), clipped * 2);
        std::fs::rename(w.finish(3).unwrap(), &path).unwrap();
        assert_eq!(read(&path).left[0], first);
    }
}

#[test]
fn a_writer_dropped_unfinished_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("half.wav");
    let mut w = Writer::create(&path, RATE, Format::Pcm24).unwrap();
    w.write(&[0.1; 64], &[0.1; 64]).unwrap();
    assert!(part_path(&path).exists());
    drop(w);
    assert!(listing(dir.path()).is_empty(), "{:?}", listing(dir.path()));
}

// ---- Whole exports ----

/// An engine with no output open (it renders on a graph of its own) and
/// three tracks: the Sampler (silent), a constant, and a muted sine.
fn engine() -> (Engine, Vec<TrackInfo>) {
    let mut e = Engine::new();
    let a = e.mixer().add_track("Bass").unwrap();
    e.mixer().set_source(a, Some(Box::new(Sine(0.3)))).unwrap();
    e.mixer().set_mix(a, TrackMix { volume: 0.8, pan: 0.25, ..Default::default() }).unwrap();
    let b = e.mixer().add_track("Pad").unwrap();
    e.mixer().set_source(b, Some(Box::new(Dc(0.125)))).unwrap();
    let c = e.mixer().add_track("Muted").unwrap();
    e.mixer().set_source(c, Some(Box::new(Sine(0.5)))).unwrap();
    e.mixer().set_mix(c, TrackMix { mute: true, ..Default::default() }).unwrap();
    let tracks = e.tracks();
    (e, tracks)
}

#[test]
fn an_export_writes_the_master_and_a_stem_per_heard_track_all_lined_up() {
    let dir = tempfile::tempdir().unwrap();
    let (mut engine, tracks) = engine();
    let p = plan(&request(&dir.path().join("Song.wav"), true, true, Format::Float32), &tracks, 0.0, 4.0, "x").unwrap();
    let mut render = engine.start_render(p.spec.clone()).unwrap();
    let calls = AtomicUsize::new(0);
    let w = write(&mut render, &p, &AtomicBool::new(false), |_| {
        calls.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    drop(render);
    engine.finish_render();
    assert!(calls.load(Ordering::Relaxed) > 0, "progress was reported");
    assert_eq!(w.frames, 4 * BEAT as u64);
    assert_eq!(
        listing(dir.path()),
        ["Song stems", "Song stems/Song - 01 Sampler.wav", "Song stems/Song - 02 Bass.wav", "Song stems/Song - 03 Pad.wav", "Song.wav"],
        "no part files left, the muted track left out"
    );
    let master = read(&dir.path().join("Song.wav"));
    let stems: Vec<Read> = p.stems.iter().map(|s| read(s)).collect();
    assert_eq!(master.left.len(), 4 * BEAT);
    assert!(stems.iter().all(|s| s.left.len() == master.left.len() && s.rate == RATE), "every file is as long as the master, at the project's rate");
    for k in 0..master.left.len() {
        let l = stems.iter().fold(0.0f32, |a, s| a + s.left[k]);
        let r = stems.iter().fold(0.0f32, |a, s| a + s.right[k]);
        assert_eq!((l, r), (master.left[k], master.right[k]), "frame {k}: the stems add up to the master, on the same frame");
    }
    assert_eq!(stems[2].left[0], 0.125, "a stem starts on the song's first frame");
    assert!(stems[0].left.iter().all(|&x| x == 0.0), "the Sampler's track is there, silent");
    assert!(w.clipped.is_empty());
}

#[test]
fn a_cancelled_export_leaves_no_half_written_file_and_keeps_what_was_there() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("Song.wav");
    std::fs::write(&old, b"the export before").unwrap();
    let (mut engine, tracks) = engine();
    let p = plan(&request(&old, true, true, Format::Pcm16), &tracks, 0.0, 400.0, "x").unwrap();
    let mut render = engine.start_render(RenderSpec { block: 512, ..p.spec.clone() }).unwrap();
    let cancel = AtomicBool::new(false);
    let mut playing = 0;
    let r = write(&mut render, &p, &cancel, |progress| {
        if progress.phase == RenderPhase::Playing {
            playing += 1;
        }
        if playing == 50 {
            // Part way through the song, every file has been started.
            assert!(progress.frames > 0 && part_path(&p.stems[0]).exists() && part_path(&old).exists());
            cancel.store(true, Ordering::Relaxed);
        }
    });
    drop(render);
    engine.finish_render();
    assert_eq!(r.unwrap_err(), Stop::Cancelled);
    assert_eq!(listing(dir.path()), ["Song.wav"], "no part files, and the stems folder it made is gone");
    assert_eq!(std::fs::read(&old).unwrap(), b"the export before", "the file that was there is untouched");
    assert!(engine.mixer().tracks().iter().all(|t| t.has_source), "the devices came home");
}

#[test]
fn an_export_stops_cleanly_when_the_output_closes_under_it() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("Stems");
    std::fs::create_dir(&folder).unwrap();
    let (engine, tracks) = engine();
    let engine = Arc::new(Mutex::new(engine));
    let p = plan(&request(&folder, false, true, Format::Pcm24), &tracks, 0.0, 10_000.0, "Song").unwrap();
    let mut render = engine.lock().unwrap().start_render(RenderSpec { block: 256, ..p.spec.clone() }).unwrap();
    let closer = {
        let engine = engine.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            // Closing (or changing) the output takes the graph back first.
            engine.lock().unwrap().close_output();
        })
    };
    let r = write(&mut render, &p, &AtomicBool::new(false), |_| {});
    drop(render);
    closer.join().unwrap();
    assert_eq!(r.unwrap_err(), Stop::Interrupted);
    assert!(listing(&folder).is_empty(), "the folder that was there stays, empty: {:?}", listing(&folder));
    let mut e = engine.lock().unwrap();
    e.finish_render();
    assert!(!e.is_rendering());
    assert!(e.mixer().tracks().iter().all(|t| t.has_source), "the devices came home");
}
