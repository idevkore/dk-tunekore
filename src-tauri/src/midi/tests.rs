//! MIDI: pads from notes, and MIDI learn driven with fake controls and
//! made-up control changes (no controller needed).

use super::map::*;
use super::targets::*;
use super::*;
use crate::sampler::PadSettings;
use serde_json::json;
use std::path::PathBuf;
use tunekore_engine::TrackId;

#[test]
fn note_on_plays_pads_36_to_51() {
    assert_eq!(parse(&[0x90, 36, 100]), Some(PadEvent::On { pad: 0, velocity: 100 }));
    assert_eq!(parse(&[0x90, 51, 1]), Some(PadEvent::On { pad: 15, velocity: 1 }));
    assert_eq!(parse(&[0x90, 35, 100]), None);
    assert_eq!(parse(&[0x90, 52, 100]), None);
}

#[test]
fn any_channel_counts() {
    for ch in 0..16u8 {
        assert_eq!(parse(&[0x90 | ch, 40, 64]), Some(PadEvent::On { pad: 4, velocity: 64 }));
        assert_eq!(parse(&[0x80 | ch, 40, 0]), Some(PadEvent::Off { pad: 4 }));
    }
}

#[test]
fn note_on_with_velocity_zero_is_a_note_off() {
    assert_eq!(parse(&[0x90, 38, 0]), Some(PadEvent::Off { pad: 2 }));
    assert_eq!(parse(&[0x80, 38, 64]), Some(PadEvent::Off { pad: 2 }));
}

#[test]
fn other_messages_are_ignored() {
    assert_eq!(parse(&[0xB0, 36, 100]), None); // control change
    assert_eq!(parse(&[0xE0, 36, 100]), None); // pitch bend
    assert_eq!(parse(&[0xF8]), None); // clock
    assert_eq!(parse(&[0x90]), None); // cut short
    assert_eq!(parse(&[]), None);
}

#[test]
fn control_changes_are_read_on_every_channel_but_mode_messages_are_not() {
    assert_eq!(parse_cc(&[0xB0, 74, 64]), Some(Cc { channel: 1, cc: 74, value: 64 }));
    assert_eq!(parse_cc(&[0xBF, 7, 127]), Some(Cc { channel: 16, cc: 7, value: 127 }));
    assert_eq!(parse_cc(&[0xB0, 120, 0]), None, "all sound off");
    assert_eq!(parse_cc(&[0xB0, 123, 0]), None, "all notes off");
    assert_eq!(parse_cc(&[0x90, 74, 64]), None, "a note");
    assert_eq!(parse_cc(&[0xB0, 74]), None, "cut short");
}

// ---- Fake controls ----

/// Controls under `test.*`: a value each, and every batch it was set with.
struct Fake {
    values: Mutex<HashMap<String, f64>>,
    infos: Vec<TargetInfo>,
    sets: Mutex<Vec<(String, Vec<Move>)>>,
}

fn linear(id: &str, min: f64, max: f64) -> TargetInfo {
    TargetInfo { id: id.into(), label: id.into(), group: "Test".into(), curve: Curve::Linear { min, max }, step: 0.0, centered: false }
}

impl Fake {
    fn new(infos: Vec<TargetInfo>, values: &[(&str, f64)]) -> Arc<Self> {
        Arc::new(Self {
            values: Mutex::new(values.iter().map(|(k, v)| (k.to_string(), *v)).collect()),
            infos,
            sets: Mutex::default(),
        })
    }

    fn value(&self, id: &str) -> f64 {
        self.values.lock().unwrap()[id]
    }

    fn put(&self, id: &str, v: f64) {
        self.values.lock().unwrap().insert(id.into(), v);
    }

    /// The undo key of every batch set so far.
    fn keys(&self) -> Vec<String> {
        self.sets.lock().unwrap().iter().map(|(k, _)| k.clone()).collect()
    }
}

/// Lets a test keep its `Arc<Fake>` and hand the registry one too.
struct Shared(Arc<Fake>);

impl Targets<()> for Shared {
    fn prefixes(&self) -> &'static [&'static str] {
        &["test"]
    }
    fn list(&self, _: &()) -> Vec<TargetInfo> {
        self.0.infos.clone()
    }
    fn info(&self, _: &(), id: &str) -> Option<TargetInfo> {
        self.0.infos.iter().find(|i| i.id == id).cloned()
    }
    fn get(&self, _: &(), id: &str) -> Option<f64> {
        self.0.values.lock().unwrap().get(id).copied()
    }
    fn set(&self, _: &(), moves: &[Move], undo_key: &str) -> Result<(), String> {
        for m in moves {
            self.0.put(&m.target, m.value);
        }
        self.0.sets.lock().unwrap().push((undo_key.into(), moves.to_vec()));
        Ok(())
    }
}

struct Rig {
    mapper: Mapper,
    registry: Registry<()>,
    fake: Arc<Fake>,
    t0: Instant,
}

impl Rig {
    fn new(infos: Vec<TargetInfo>, values: &[(&str, f64)]) -> Self {
        let fake = Fake::new(infos, values);
        let mut registry = Registry::default();
        registry.register(Arc::new(Shared(fake.clone())));
        Self { mapper: Mapper::default(), registry, fake, t0: Instant::now() }
    }

    /// One level control, `test.level` 0..1, at 0.5.
    fn level() -> Self {
        Self::new(vec![linear("test.level", 0.0, 1.0)], &[("test.level", 0.5)])
    }

    fn at(&self, ms: u64) -> Instant {
        self.t0 + Duration::from_millis(ms)
    }

    /// Feeds control changes `(ms, channel, cc, value)` as one batch.
    fn send(&mut self, msgs: &[(u64, u8, u8, u8)]) -> Outcome {
        let batch: Vec<Arrived> = msgs.iter().map(|&(ms, channel, cc, value)| (self.at(ms), Cc { channel, cc, value })).collect();
        let now = batch.last().map_or(self.t0, |b| b.0);
        run(&mut self.mapper, &self.registry, &(), &batch, now)
    }

    /// Lets time pass with nothing arriving.
    fn wait_until(&mut self, ms: u64) -> Outcome {
        let now = self.at(ms);
        run(&mut self.mapper, &self.registry, &(), &[], now)
    }

    fn learn(&mut self, target: &str) {
        self.mapper.learn(Some(target.into()));
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ---- Learning ----

#[test]
fn learning_takes_the_next_control_change_and_the_knob_then_moves_the_control() {
    let mut r = Rig::level();
    r.mapper.takeover = Takeover::Jump;
    r.learn("test.level");
    let out = r.send(&[(0, 1, 74, 10)]);
    assert_eq!(out.learned, vec![Mapping { source: Source::seven(1, 74), target: "test.level".into() }]);
    assert!(out.moves.is_empty(), "the learning message itself moves nothing");
    assert_eq!(r.fake.value("test.level"), 0.5);
    assert_eq!(r.mapper.learning(), None, "learning stops once it has a controller");

    r.send(&[(10, 1, 74, 127)]);
    assert_eq!(r.fake.value("test.level"), 1.0);
    r.send(&[(20, 1, 74, 0)]);
    assert_eq!(r.fake.value("test.level"), 0.0);
    r.send(&[(30, 2, 74, 64)]);
    assert_eq!(r.fake.value("test.level"), 0.0, "another channel is another controller");
}

#[test]
fn a_burst_sets_each_control_once_at_the_latest_value() {
    let mut r = Rig::level();
    r.mapper.takeover = Takeover::Jump;
    r.mapper.assign(Source::seven(1, 74), "test.level".into());
    let out = r.send(&[(0, 1, 74, 20), (1, 1, 74, 30), (2, 1, 74, 127)]);
    assert_eq!(out.moves, vec![Move { target: "test.level".into(), value: 1.0 }]);
    assert_eq!(r.fake.sets.lock().unwrap().len(), 1);
}

#[test]
fn learning_waits_a_moment_for_the_other_half_of_a_14_bit_pair() {
    let mut r = Rig::level();
    r.mapper.takeover = Takeover::Jump;
    r.learn("test.level");
    // CC 7 is below 32: it may be the coarse half of 7 + 39.
    assert!(r.send(&[(0, 1, 7, 64)]).learned.is_empty());
    let out = r.send(&[(1, 1, 39, 0)]);
    assert_eq!(out.learned[0].source, Source::fourteen(1, 7));
    // Both halves move it, finely: 64 * 128 + 1 of 16383.
    r.send(&[(10, 1, 7, 64), (10, 1, 39, 1)]);
    assert!(close(r.fake.value("test.level"), 8193.0 / 16383.0));
    // Centred controls put 8192 exactly in the middle.
    assert_eq!(knob_position(8192, 14, true), 0.5);
}

#[test]
fn a_controller_below_64_with_no_partner_learns_as_7_bit_after_the_wait() {
    let mut r = Rig::level();
    r.learn("test.level");
    assert!(r.send(&[(0, 1, 20, 64), (5, 1, 20, 65)]).learned.is_empty(), "still waiting for 52");
    assert!(r.wait_until(20).learned.is_empty());
    let out = r.wait_until(41);
    assert_eq!(out.learned[0].source, Source::seven(1, 20));
    assert_eq!(r.mapper.deadline(), None);
}

#[test]
fn learning_skips_bank_select_and_data_entry() {
    let mut r = Rig::level();
    r.learn("test.level");
    for cc in [0, 32, 6, 38, 98, 99, 100, 101] {
        assert!(r.send(&[(0, 1, cc, 1)]).learned.is_empty(), "CC {cc}");
    }
    r.wait_until(100);
    assert_eq!(r.mapper.learning(), Some("test.level"), "still learning");
    assert_eq!(r.send(&[(200, 1, 74, 1)]).learned.len(), 1);
}

// ---- Two mappings on one control or one controller ----

#[test]
fn one_controller_moves_one_control_and_the_newest_learn_wins() {
    let mut m = Mapper::default();
    m.assign(Source::seven(1, 74), "test.a".into());
    // The same knob learns another control: it leaves the first.
    m.assign(Source::seven(1, 74), "test.b".into());
    assert_eq!(m.mappings(), &[Mapping { source: Source::seven(1, 74), target: "test.b".into() }]);
    // The same control learns another knob: the old knob lets go of it.
    m.assign(Source::seven(1, 71), "test.b".into());
    assert_eq!(m.mappings(), &[Mapping { source: Source::seven(1, 71), target: "test.b".into() }]);
    // A 14-bit pair takes both its numbers from 7-bit mappings.
    m.assign(Source::seven(1, 39), "test.c".into());
    m.assign(Source::fourteen(1, 7), "test.d".into());
    assert_eq!(m.mappings().iter().map(|x| x.target.as_str()).collect::<Vec<_>>(), ["test.b", "test.d"]);
    // Different channels don't clash.
    m.assign(Source::seven(2, 71), "test.e".into());
    assert_eq!(m.mappings().len(), 3);
}

#[test]
fn forgetting_a_mapping_stops_its_knob() {
    let mut r = Rig::level();
    r.mapper.takeover = Takeover::Jump;
    r.mapper.assign(Source::seven(1, 74), "test.level".into());
    assert!(r.mapper.forget("test.level"));
    assert!(!r.mapper.forget("test.level"), "nothing left to forget");
    assert!(r.send(&[(0, 1, 74, 127)]).moves.is_empty());
    assert_eq!(r.fake.value("test.level"), 0.5);
}

#[test]
fn a_mapping_to_a_control_that_isnt_there_waits_and_is_kept() {
    let mut r = Rig::level();
    r.mapper.takeover = Takeover::Jump;
    r.mapper.assign(Source::seven(1, 74), "test.gone".into());
    assert!(r.send(&[(0, 1, 74, 127)]).moves.is_empty());
    assert_eq!(r.mapper.mappings().len(), 1);
    assert!(r.mapper.to_file()["mappings"][0]["target"] == "test.gone");
}

// ---- Gestures and undo ----

#[test]
fn a_knob_gesture_is_one_undo_step() {
    let mut r = Rig::level();
    r.mapper.takeover = Takeover::Jump;
    r.mapper.assign(Source::seven(1, 74), "test.level".into());
    // A slow turn: a message every 100 ms for a second and a half.
    for (i, ms) in (0..1500).step_by(100).enumerate() {
        r.send(&[(ms, 1, 74, i as u8 * 8)]);
    }
    let keys = r.fake.keys();
    assert_eq!(keys.len(), 15);
    assert!(keys.iter().all(|k| *k == keys[0]), "one gesture, one undo key: {keys:?}");
    // Let go for longer than the gap, then turn again: a new step.
    r.send(&[(1400 + 600, 1, 74, 3)]);
    let keys = r.fake.keys();
    assert_ne!(keys.last(), keys.first());
    assert!(keys[0].starts_with("midi:"));
}

#[test]
fn two_knobs_turned_together_share_the_gesture() {
    let mut r = Rig::new(vec![linear("test.a", 0.0, 1.0), linear("test.b", 0.0, 1.0)], &[("test.a", 0.0), ("test.b", 0.0)]);
    r.mapper.takeover = Takeover::Jump;
    r.mapper.assign(Source::seven(1, 1), "test.a".into());
    r.mapper.assign(Source::seven(1, 2), "test.b".into());
    for i in 0..10u64 {
        r.send(&[(i * 20, 1, 1, i as u8 * 10)]);
        r.send(&[(i * 20 + 10, 1, 2, i as u8 * 10)]);
    }
    let keys = r.fake.keys();
    assert!(keys.iter().all(|k| *k == keys[0]), "{keys:?}");
}

// ---- Knob and value disagree ----

#[test]
fn pick_up_waits_until_the_knob_reaches_the_value() {
    let mut r = Rig::level(); // at 0.5, about CC 64
    r.learn("test.level");
    r.send(&[(0, 1, 74, 10)]);
    assert!(r.send(&[(10, 1, 74, 30)]).moves.is_empty(), "far below the value: nothing yet");
    assert!(r.send(&[(20, 1, 74, 50)]).moves.is_empty());
    // It passes the value: from here it follows.
    r.send(&[(30, 1, 74, 70)]);
    assert!(close(r.fake.value("test.level"), 70.0 / 127.0));
    r.send(&[(40, 1, 74, 40)]);
    assert!(close(r.fake.value("test.level"), 40.0 / 127.0));
    // Something else moves the control (the screen, undo): the knob waits again.
    r.fake.put("test.level", 0.9);
    assert!(r.send(&[(50, 1, 74, 45)]).moves.is_empty());
    r.send(&[(60, 1, 74, 126)]);
    assert!(close(r.fake.value("test.level"), 126.0 / 127.0));
}

#[test]
fn pick_up_takes_over_at_once_when_the_knob_is_already_there() {
    let mut r = Rig::level();
    r.mapper.assign(Source::seven(1, 74), "test.level".into());
    r.send(&[(0, 1, 74, 64)]);
    assert!(close(r.fake.value("test.level"), 64.0 / 127.0), "within two steps of the value");
}

#[test]
fn jump_sets_the_value_straight_away() {
    let mut r = Rig::level();
    r.mapper.takeover = Takeover::Jump;
    r.mapper.assign(Source::seven(1, 74), "test.level".into());
    r.send(&[(0, 1, 74, 0)]);
    assert_eq!(r.fake.value("test.level"), 0.0);
}

#[test]
fn scale_moves_the_value_in_proportion_until_the_knob_catches_it() {
    let mut r = Rig::new(vec![linear("test.level", 0.0, 1.0)], &[("test.level", 1.0)]);
    r.mapper.takeover = Takeover::Scale;
    r.learn("test.level");
    r.send(&[(0, 1, 74, 64)]); // the knob is halfway, the value at the top
    r.send(&[(10, 1, 74, 32)]);
    // Halfway down the knob's way to 0 is halfway down the value's.
    assert!(close(r.fake.value("test.level"), 0.5), "{}", r.fake.value("test.level"));
    r.send(&[(20, 1, 74, 0)]);
    assert_eq!(r.fake.value("test.level"), 0.0, "they meet at the end");
    r.send(&[(30, 1, 74, 64)]);
    assert!(close(r.fake.value("test.level"), 64.0 / 127.0), "and move together after");
}

// ---- Curves ----

#[test]
fn tempo_is_linear_and_rounded_to_whole_bpm() {
    let t = transport_info(TEMPO).unwrap();
    assert_eq!(t.value(0.0), 40.0);
    assert_eq!(t.value(1.0), 300.0);
    assert_eq!(t.value(0.5), 170.0);
    for raw in 0..=127u16 {
        let v = t.value(knob_position(raw, 7, false));
        assert_eq!(v.fract(), 0.0, "CC {raw} gives {v}");
    }
    // 14 bits reach every whole BPM.
    let reached: std::collections::HashSet<i64> = (0..=16383u16).map(|r| t.value(knob_position(r, 14, false)) as i64).collect();
    assert_eq!(reached.len(), 261);
}

#[test]
fn cutoff_is_logarithmic_from_20_hz_to_20_khz() {
    let (_, cutoff) = parse_pad("sampler.pad.selected.cutoff").unwrap();
    let c = cutoff.curve;
    assert!(close(c.value(0.0), 20.0) && close(c.value(1.0), 20_000.0));
    // Halfway is the geometric middle; equal turns are equal ratios.
    assert!((c.value(0.5) - 632.455).abs() < 0.01);
    assert!(close(c.value(0.6) / c.value(0.5), c.value(0.3) / c.value(0.2)));
    // A 7-bit step is under a semitone.
    let step = c.value(65.0 / 127.0) / c.value(64.0 / 127.0);
    assert!(step < 2f64.powf(1.0 / 12.0), "{step}");
}

#[test]
fn levels_feel_like_a_fader() {
    let (_, level) = parse_pad("sampler.pad.3.level").unwrap();
    let c = level.curve;
    assert_eq!(c.value(1.0), 6.0, "the top is +6 dB");
    assert_eq!(c.value(0.0), -60.0, "the bottom is off");
    assert_eq!(c.value(10.0 / 127.0), -60.0, "the bottom tenth is off");
    let zero = c.value(101.0 / 127.0);
    assert!(zero.abs() < 0.5, "0 dB about four fifths up: {zero}");
    let mut last = f64::MIN;
    for raw in 0..=127u16 {
        let v = c.value(knob_position(raw, 7, false));
        assert!(v >= last, "always rising");
        last = v;
    }
    // Near 0 dB a step is a fraction of a dB.
    assert!(c.value(102.0 / 127.0) - c.value(101.0 / 127.0) < 0.3);
}

#[test]
fn times_are_squared_so_short_times_get_most_of_the_knob() {
    let (_, attack) = parse_pad("sampler.pad.1.attack").unwrap();
    assert!(close(attack.curve.value(0.5), 0.5), "halfway is a quarter of 2 s");
    let (_, release) = parse_pad("sampler.pad.1.release").unwrap();
    assert!(close(release.curve.value(0.0), 0.001), "release never reaches 0");
    assert!(close(release.curve.value(1.0), 5.0));
}

#[test]
fn bipolar_controls_centre_on_the_knobs_middle() {
    let (_, pan) = parse_pad("sampler.pad.selected.pan").unwrap();
    let info = TargetInfo { id: "p".into(), label: "Pan".into(), group: String::new(), curve: pan.curve, step: pan.step, centered: pan.centered };
    assert_eq!(info.value(knob_position(64, 7, true)), 0.0, "CC 64 is the centre exactly");
    assert_eq!(info.value(knob_position(0, 7, true)), -1.0);
    assert_eq!(info.value(knob_position(127, 7, true)), 1.0);
    let (_, pitch) = parse_pad("sampler.pad.selected.pitch").unwrap();
    let info = TargetInfo { curve: pitch.curve, step: pitch.step, centered: pitch.centered, ..info };
    for raw in 0..=127u16 {
        assert_eq!(info.value(knob_position(raw, 7, true)).fract(), 0.0, "whole semitones");
    }
    assert_eq!(info.value(knob_position(64, 7, true)), 0.0);
}

#[test]
fn every_curve_finds_its_own_values_again() {
    let mut infos: Vec<TargetInfo> = PAD_PARAMS
        .iter()
        .map(|p| TargetInfo { id: p.name.into(), label: p.label.into(), group: String::new(), curve: p.curve, step: 0.0, centered: p.centered })
        .collect();
    infos.extend([transport_info(TEMPO).unwrap(), transport_info(SWING).unwrap()].map(|i| TargetInfo { step: 0.0, ..i }));
    for info in infos {
        for t in [0.15, 0.3, 0.5, 0.77, 1.0] {
            let back = info.position(info.value(t));
            assert!((back - t).abs() < 1e-9, "{}: {t} came back as {back}", info.id);
        }
    }
}

#[test]
fn pad_ids_name_the_selected_pad_or_one_pad() {
    assert_eq!(parse_pad("sampler.pad.selected.cutoff").map(|(p, x)| (p, x.name)), Some((PadRef::Selected, "cutoff")));
    assert_eq!(parse_pad("sampler.pad.3.fenvAmount").map(|(p, x)| (p, x.name)), Some((PadRef::Pad(2), "fenvAmount")));
    for bad in ["sampler.pad.0.level", "sampler.pad.17.level", "sampler.pad.03.level", "sampler.pad.3.loudness", "sampler.pad.3", "sampler.swing"] {
        assert!(parse_pad(bad).is_none(), "{bad}");
    }
    assert_eq!(sampler_ids().len(), 2 + 17 * PAD_PARAMS.len());
    assert_eq!(parse_track("track.1.volume"), Some((TrackId(1), true)));
    assert_eq!(parse_track("track.12.pan"), Some((TrackId(12), false)));
    assert_eq!(parse_track("track.01.pan"), None);
    assert_eq!(parse_track("track.1.mute"), None);
}

#[test]
fn moving_start_pushes_end_along_as_on_screen() {
    let (_, start) = parse_pad("sampler.pad.1.start").unwrap();
    let (_, end) = parse_pad("sampler.pad.1.end").unwrap();
    let mut s = PadSettings { start: 0.2, end: 0.5, ..Default::default() };
    (start.set)(&mut s, 0.8);
    assert!(s.end > s.start && s.end <= 1.0, "{} {}", s.start, s.end);
    (start.set)(&mut s, 1.0);
    assert!(s.start < s.end && s.end == 1.0);
    (end.set)(&mut s, 0.0);
    assert!(s.start == 0.0 && s.end > 0.0);
}

#[test]
fn two_providers_cant_share_a_prefix() {
    let mut registry: Registry<()> = Registry::default();
    registry.register(Arc::new(Shared(Fake::new(vec![], &[]))));
    let again = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| registry.register(Arc::new(Shared(Fake::new(vec![], &[]))))));
    assert!(again.is_err());
}

// ---- The project's `midi` section ----

fn sample_mapper() -> Mapper {
    let mut m = Mapper::default();
    m.assign(Source::seven(1, 74), "sampler.pad.selected.cutoff".into());
    m.assign(Source::seven(10, 71), "sampler.pad.3.level".into());
    m.assign(Source::fourteen(1, 7), "transport.tempo".into());
    m
}

#[test]
fn the_midi_section_round_trips() {
    let file = sample_mapper().to_file();
    assert_eq!(
        file,
        json!({ "mappings": [
            { "target": "sampler.pad.selected.cutoff", "channel": 1, "cc": 74 },
            { "target": "sampler.pad.3.level", "channel": 10, "cc": 71 },
            { "target": "transport.tempo", "channel": 1, "cc": 7, "bits": 14 },
        ] })
    );
    let mut back = Mapper::default();
    back.load(Mapper::read_file(Some(&file)).unwrap());
    assert_eq!(back.mappings(), sample_mapper().mappings());
    assert_eq!(back.to_file(), file);
}

#[test]
fn entries_this_build_cant_read_are_written_back_and_duplicates_resolve_to_the_last() {
    let file = json!({ "mappings": [
        { "target": "test.a", "channel": 1, "cc": 74 },
        { "target": "test.b", "channel": 1, "cc": 74 },
        { "target": "test.c", "channel": 17, "cc": 1 },
        { "target": "test.d", "nrpn": 1234, "channel": 1 },
    ] });
    let mut m = Mapper::default();
    m.load(Mapper::read_file(Some(&file)).unwrap());
    assert_eq!(m.mappings(), &[Mapping { source: Source::seven(1, 74), target: "test.b".into() }]);
    let out = m.to_file();
    assert_eq!(out["mappings"].as_array().unwrap().len(), 3, "test.b, and the two it can't read");
    assert_eq!(out["mappings"][2]["nrpn"], 1234);
}

#[test]
fn a_damaged_section_is_refused() {
    for bad in [json!(5), json!({ "mappings": 3 }), json!("mappings")] {
        assert!(Mapper::read_file(Some(&bad)).unwrap_err().contains("damaged"), "{bad}");
    }
    assert!(Mapper::read_file(Some(&json!({}))).unwrap().mappings.is_empty());
}

/// A project with only the `midi` section, as the app has it.
fn project() -> (crate::project::Project<()>, Arc<Mutex<Mapper>>, Arc<AtomicU64>) {
    let p = crate::project::Project::new();
    let (mapper, revision) = (Arc::new(Mutex::new(Mapper::default())), Arc::new(AtomicU64::new(0)));
    p.register(MidiSection::<()> { mapper: mapper.clone(), revision: revision.clone(), notify: |_| {} });
    (p, mapper, revision)
}

#[test]
fn mappings_come_back_after_save_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (p, mapper, revision) = project();
    *mapper.lock().unwrap() = sample_mapper();
    revision.fetch_add(1, Ordering::Relaxed);
    assert!(p.is_dirty(&()), "a new mapping is unsaved work");
    let file = p.save(&(), Some(dir.path().join("beat"))).unwrap();
    assert!(!p.is_dirty(&()));
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("\"midi\": {"), "{text}");

    mapper.lock().unwrap().clear();
    p.open(&(), &file, Some(file.clone()), false).unwrap();
    assert_eq!(mapper.lock().unwrap().mappings(), sample_mapper().mappings());
    assert!(!p.is_dirty(&()));
}

#[test]
fn older_projects_open_with_no_mappings() {
    let (p, mapper, _) = project();
    let old: [(&str, PathBuf); 3] = [
        (r#"{"format":"tunekore-project","version":1,"tempo":120,"pads":[]}"#, "/v1.tunek".into()),
        (r#"{"format":"tunekore-project","version":2,"midi":null}"#, "/hook.tunek".into()),
        (r#"{"format":"tunekore-project","version":2}"#, "/none.tunek".into()),
    ];
    for (text, path) in old {
        *mapper.lock().unwrap() = sample_mapper();
        p.open_text(&(), text, &path, Some(path.clone()), false).unwrap();
        assert!(mapper.lock().unwrap().mappings().is_empty(), "{text}: the last project's mappings don't carry over");
    }
}

#[test]
fn a_project_with_damaged_mappings_doesnt_open_and_changes_nothing() {
    let (p, mapper, _) = project();
    *mapper.lock().unwrap() = sample_mapper();
    let err = p.open_text(&(), r#"{"format":"tunekore-project","version":2,"midi":{"mappings":7}}"#, Path::new("/x.tunek"), None, false).unwrap_err();
    assert!(err.contains("MIDI"), "{err}");
    assert_eq!(mapper.lock().unwrap().mappings(), sample_mapper().mappings());
}

#[test]
fn a_new_project_has_no_mappings() {
    let (p, mapper, _) = project();
    *mapper.lock().unwrap() = sample_mapper();
    p.new_project(&()).unwrap();
    assert!(mapper.lock().unwrap().mappings().is_empty());
}
