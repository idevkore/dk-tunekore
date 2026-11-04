use super::*;
use crate::graph::{Mixer, TRACK_EVENTS};
use crate::testing::realtime;
use crate::transport::{TempoMap, TimeSignature, Transport, TransportShared};

const RATE: u32 = 48_000;
/// Frames per beat at 120 BPM.
const BEAT: u64 = 24_000;

fn note(key: u8, start: f64, length: f64) -> MidiNote {
    MidiNote { key, start, length, velocity: 0.8 }
}

/// A clip that plays its notes once, from its start.
fn clip(start: f64, length: f64, notes: Vec<MidiNote>) -> MidiClipSpec {
    MidiClipSpec { start, offset: 0.0, content: length, phase: 0.0, length, notes }
}

/// A note event at a frame from the first block's start.
#[derive(Debug, Clone, Copy, PartialEq)]
enum At {
    On(u8, u64),
    Off(u8, u64),
}

struct Rig {
    player: MidiPlayer,
    feed: MidiFeed,
    shared: TransportShared,
    clock: Transport,
    events: Events,
    left: Vec<f32>,
    right: Vec<f32>,
    /// Frames run so far.
    frames: u64,
    log: Vec<At>,
    /// Keys sounding on the instrument, as the log leaves them.
    sounding: [bool; KEYS],
    /// The clips last published, which the player plays from the next block on.
    clips: Vec<MidiClipSpec>,
    /// Where the last piece ended and its beats per frame, when it was playing.
    end: Option<(f64, f64)>,
}

impl Rig {
    fn new(clips: Vec<MidiClipSpec>) -> Self {
        Self::at(RATE, clips)
    }

    fn at(rate: u32, clips: Vec<MidiClipSpec>) -> Self {
        let (mut player, mut feed) = player();
        player.prepare(rate, 4096);
        feed.publish(MidiClipSet::new(clips.clone()));
        Self {
            player,
            feed,
            shared: TransportShared::default(),
            clock: Transport::new(rate),
            events: Events::with_capacity(TRACK_EVENTS),
            left: vec![0.0; 4096],
            right: vec![0.0; 4096],
            frames: 0,
            log: Vec::with_capacity(1 << 16),
            sounding: [false; KEYS],
            clips,
            end: None,
        }
    }

    fn publish(&mut self, clips: Vec<MidiClipSpec>) {
        self.feed.publish(MidiClipSet::new(clips.clone()));
        self.clips = clips;
    }

    /// Runs one block, as the graph does, and checks every piece's events:
    /// in frame order, inside the piece, and never a note started twice or
    /// ended while silent.
    fn block(&mut self, frames: usize) {
        let Rig { player, shared, clock, events, left, right, log, sounding, end, .. } = self;
        let base = self.frames;
        clock.run(shared, frames, |ctx, off| {
            *end = ctx.playing.then(|| (ctx.map.advance(ctx.beat, ctx.frames as f64), ctx.beats_per_frame));
            events.clear();
            player.process(ctx, events, Buffers { left: &mut left[off..off + ctx.frames], right: &mut right[off..off + ctx.frames] });
            let mut last = 0;
            for e in events.as_slice() {
                assert!(e.frame() >= last && (e.frame() as usize) < ctx.frames, "{e:?} in a piece of {}", ctx.frames);
                last = e.frame();
                let at = base + (off + e.frame() as usize) as u64;
                match *e {
                    Event::NoteOn { key, .. } => {
                        assert!(!sounding[usize::from(key)], "key {key} started again at {at} while it sounds");
                        sounding[usize::from(key)] = true;
                        log.push(At::On(key, at));
                    }
                    Event::NoteOff { key, .. } => {
                        assert!(sounding[usize::from(key)], "key {key} ended at {at} while silent");
                        sounding[usize::from(key)] = false;
                        log.push(At::Off(key, at));
                    }
                    Event::Control { .. } => panic!("a control from the clip player"),
                }
            }
        });
        self.frames += frames as u64;
    }

    fn run(&mut self, frames: u64, block: usize) {
        let end = self.frames + frames;
        while self.frames < end {
            self.block(block.min((end - self.frames) as usize));
        }
    }

    fn ons(&self) -> Vec<(u8, u64)> {
        self.log.iter().filter_map(|e| if let At::On(k, f) = *e { Some((k, f)) } else { None }).collect()
    }

    fn offs(&self) -> Vec<(u8, u64)> {
        self.log.iter().filter_map(|e| if let At::Off(k, f) = *e { Some((k, f)) } else { None }).collect()
    }

    fn silent(&self) -> bool {
        self.sounding.iter().all(|s| !s)
    }

    /// Checks that every clip key sounding (of `keys`) at the end of the last
    /// block is held by a note of the clips in effect that plays there,
    /// within two frames: no note sounds on past every note that holds it.
    fn held_by_playing_notes(&self, keys: std::ops::Range<u8>) {
        let Some((beat, bpf)) = self.end else { return };
        let tol = 2.0 * bpf;
        // A key the hand holds is the hand's, not a clip's.
        for key in keys.filter(|&k| self.sounding[usize::from(k)] && self.player.hand & (1u128 << k) == 0) {
            let held = self.clips.iter().filter(|c| c.length > 0.0 && c.content > 0.0).any(|c| {
                let end = c.start + c.length;
                let base = c.start - c.phase;
                let k0 = ((beat - base) / c.content).floor();
                (-1..=1).any(|dk| {
                    let pass = base + (k0 + f64::from(dk)) * c.content;
                    c.notes.iter().filter(|n| n.key == key).any(|n| {
                        let at = n.start - c.offset;
                        let on = pass + at;
                        if at < 0.0 || at >= c.content || on < c.start || on >= end {
                            return false;
                        }
                        let off = (on + n.length.max(MIN_BEATS)).min(pass + c.content).min(end);
                        on <= beat + tol && beat < off + tol
                    })
                })
            });
            assert!(held, "key {key} sounds at beat {beat} with no note of the clips holding it: {:?}", self.clips);
        }
    }
}

#[test]
fn notes_start_and_end_on_their_frames_whatever_the_blocks() {
    for block in [1, 64, 500, 512, 1000, 4096] {
        // A bar from beat 1; the last note runs past the clip's end and stops there.
        let mut rig = Rig::new(vec![clip(1.0, 4.0, vec![note(60, 0.0, 1.0), note(62, 1.5, 0.25), note(64, 3.5, 2.0)])]);
        rig.shared.play();
        rig.run(6 * BEAT, block);
        assert_eq!(rig.ons(), [(60, BEAT), (62, BEAT * 5 / 2), (64, BEAT * 9 / 2)], "block {block}");
        assert_eq!(rig.offs(), [(60, 2 * BEAT), (62, BEAT * 11 / 4), (64, 5 * BEAT)], "block {block}: the last at the clip's end");
        assert!(rig.silent());
    }
}

#[test]
fn a_note_between_frames_lands_on_the_next_one_as_the_audio_clips_do() {
    // 1.00001 beats is 24,000.24 frames: the note's frame is 24,001.
    let mut rig = Rig::new(vec![clip(1.00001, 1.0, vec![note(60, 0.0, 0.5)])]);
    rig.shared.play();
    rig.run(2 * BEAT, 512);
    assert_eq!(rig.ons(), [(60, BEAT + 1)]);
    // At rates and tempos where beats aren't whole frames, the first frame at or after the beat.
    for (rate, tempo, beat) in [(44_100u32, 97.0, 1.37), (96_000, 133.3, 2.0), (48_000, 61.0, 0.123)] {
        let mut rig = Rig::at(rate, vec![clip(beat, 1.0, vec![note(60, 0.0, 0.5)])]);
        rig.shared.set_tempo(tempo);
        rig.shared.play();
        let map = TempoMap::constant(tempo, TimeSignature::default(), rate);
        let want = (map.frame_of_beat(beat) - EDGE_FRAMES).ceil() as u64;
        rig.run(want + u64::from(rate), 333);
        assert_eq!(rig.ons()[0], (60, want), "{rate} Hz, {tempo} BPM");
    }
}

#[test]
fn a_trimmed_clip_plays_only_the_notes_in_its_content() {
    // Content from beat 1 of the clip's own time to beat 3: the notes at 0.5 and 3.0 are outside it.
    let c = MidiClipSpec { start: 4.0, offset: 1.0, content: 2.0, phase: 0.0, length: 2.0, notes: vec![note(48, 0.5, 4.0), note(60, 1.0, 0.5), note(62, 2.5, 4.0), note(64, 3.0, 1.0)] };
    let mut rig = Rig::new(vec![c]);
    rig.shared.play();
    rig.run(8 * BEAT, 512);
    assert_eq!(rig.ons(), [(60, 4 * BEAT), (62, BEAT * 11 / 2)]);
    assert_eq!(rig.offs(), [(60, BEAT * 9 / 2), (62, 6 * BEAT)], "the long note stops at the content's end");
}

#[test]
fn a_looped_clip_plays_every_pass_and_cuts_notes_at_its_seam() {
    // Two beats of content played for seven, starting half a beat into it: passes start at -0.5, 1.5, 3.5 and 5.5.
    let c = MidiClipSpec { start: 0.0, offset: 0.0, content: 2.0, phase: 0.5, length: 7.0, notes: vec![note(60, 0.0, 0.5), note(67, 1.5, 1.0), note(60, 1.75, 1.0)] };
    for block in [64, 512, 777] {
        let mut rig = Rig::new(vec![c.clone()]);
        rig.shared.play();
        rig.run(8 * BEAT, block);
        let b = |beats: f64| (beats * BEAT as f64) as u64;
        // Pass 0's note at its start falls before the clip's; pass 3's 67 falls on its end.
        assert_eq!(rig.ons(), [(67, b(1.0)), (60, b(1.25)), (60, b(1.5)), (67, b(3.0)), (60, b(3.25)), (60, b(3.5)), (67, b(5.0)), (60, b(5.25)), (60, b(5.5))], "block {block}");
        // The notes that cross the seam stop on it; the 60 there ends just before the next pass's 60 starts.
        let seam_offs: Vec<(u8, u64)> = rig.offs().into_iter().filter(|&(_, f)| f == b(1.5) || f == b(3.5) || f == b(5.5)).collect();
        assert_eq!(seam_offs, [(60, b(1.5)), (67, b(1.5)), (60, b(3.5)), (67, b(3.5)), (60, b(5.5)), (67, b(5.5))], "block {block}");
        let at_seam: Vec<At> = rig.log.iter().copied().filter(|e| matches!(*e, At::On(60, f) | At::Off(60, f) if f == b(3.5))).collect();
        assert_eq!(at_seam, [At::Off(60, b(3.5)), At::On(60, b(3.5))], "the end before the new start, on one frame");
        assert!(rig.silent(), "block {block}: the last note stopped at the clip's end");
    }
}

#[test]
fn the_transport_loop_wrap_ends_notes_and_starts_the_loop_on_its_frame() {
    // Loop beats 0 to 2; a note that runs over the loop's end, and one on its start.
    let mut rig = Rig::new(vec![clip(0.0, 4.0, vec![note(62, 0.0, 0.25), note(60, 1.5, 2.0)])]);
    rig.shared.set_loop(0.0, 2.0);
    rig.shared.set_looping(true);
    rig.shared.play();
    rig.run(3 * 2 * BEAT, 700);
    let mut want = Vec::new();
    for pass in 0..3u64 {
        let at = pass * 2 * BEAT;
        if pass > 0 {
            want.push(At::Off(60, at));
        }
        want.push(At::On(62, at));
        want.push(At::Off(62, at + BEAT / 4));
        want.push(At::On(60, at + BEAT * 3 / 2));
    }
    assert_eq!(rig.log, want);
    rig.shared.stop();
    rig.run(1_000, 700);
    assert_eq!(rig.log.last(), Some(&At::Off(60, 6 * BEAT)), "stopping ends it");
}

#[test]
fn a_seek_ends_sounding_notes_and_plays_on_from_where_it_lands() {
    let mut rig = Rig::new(vec![clip(0.0, 8.0, vec![note(48, 0.0, 8.0), note(62, 4.0, 1.0), note(64, 5.0, 1.0), note(66, 3.0, 2.0)])]);
    rig.shared.play();
    rig.run(10_000, 500);
    rig.shared.seek(4.0);
    rig.run(2 * BEAT, 500);
    // The long note ends at the jump; the one at 3 began before beat 4, so it isn't played (no chasing).
    assert_eq!(rig.log, [At::On(48, 0), At::Off(48, 10_000), At::On(62, 10_000), At::Off(62, 10_000 + BEAT), At::On(64, 10_000 + BEAT)]);
}

#[test]
fn a_tempo_change_moves_the_notes_that_follow() {
    let mut rig = Rig::new(vec![clip(0.0, 8.0, vec![note(60, 1.0, 2.0), note(62, 4.0, 1.0)])]);
    rig.shared.play();
    rig.run(2 * BEAT, 480);
    rig.shared.set_tempo(60.0);
    rig.run(200_000, 480);
    // To beat 2 at 120 (48,000 frames), then a beat is 48,000 frames.
    assert_eq!(rig.log, [At::On(60, BEAT), At::Off(60, 96_000), At::On(62, 144_000), At::Off(62, 192_000)]);
}

#[test]
fn stopping_ends_every_sounding_note_and_play_starts_again() {
    let mut rig = Rig::new(vec![clip(0.0, 8.0, vec![note(60, 0.0, 4.0), note(64, 0.5, 4.0), note(67, 6.0, 1.0)])]);
    rig.shared.play();
    rig.run(BEAT, 512);
    rig.shared.stop();
    rig.run(4_000, 512);
    assert!(rig.silent());
    assert_eq!(rig.offs(), [(60, BEAT), (64, BEAT)], "on the stop's frame");
    rig.shared.play();
    rig.run(BEAT, 512);
    assert_eq!(rig.ons()[2..], [(60, BEAT + 4_000), (64, BEAT + 4_000 + BEAT / 2)], "from the start again");
}

#[test]
fn an_edit_keeps_the_notes_the_new_clips_still_hold_and_ends_the_rest() {
    let mut rig = Rig::new(vec![clip(0.0, 8.0, vec![note(60, 0.0, 8.0), note(62, 0.0, 8.0), note(65, 0.0, 8.0)])]);
    rig.shared.play();
    rig.run(10_000, 500);
    // 60 now ends at beat 4, 62 is gone, 65 moved to start later, and 64 is new at beat 2.
    rig.publish(vec![clip(0.0, 8.0, vec![note(60, 0.0, 4.0), note(64, 2.0, 1.0), note(65, 3.0, 1.0)])]);
    rig.run(5 * BEAT, 500);
    let b = |beats: u64| beats * BEAT;
    assert_eq!(
        rig.log[3..],
        [At::Off(62, 10_000), At::Off(65, 10_000), At::On(64, b(2)), At::Off(64, b(3)), At::On(65, b(3)), At::Off(60, b(4)), At::Off(65, b(4))],
        "60 plays on to its new end"
    );
    // Removing every clip ends what sounds.
    rig.shared.seek(0.0);
    rig.run(1_000, 500);
    rig.publish(vec![]);
    rig.run(1_000, 500);
    assert!(rig.silent());
}

#[test]
fn notes_played_by_hand_go_out_at_once_and_all_off_lets_go() {
    let mut rig = Rig::new(vec![]);
    assert!(rig.feed.preview(Preview::On { key: 60, velocity: 0.5 }));
    rig.feed.preview(Preview::On { key: 64, velocity: 0.5 });
    rig.run(512, 512);
    rig.feed.preview(Preview::Off { key: 60 });
    rig.feed.preview(Preview::Off { key: 61 }); // never held: nothing
    rig.run(512, 512);
    rig.feed.preview(Preview::On { key: 64, velocity: 0.5 }); // again while held: let go first
    rig.run(512, 512);
    rig.feed.preview(Preview::AllOff);
    rig.run(512, 512);
    assert_eq!(rig.log, [At::On(60, 0), At::On(64, 0), At::Off(60, 512), At::Off(64, 1_024), At::On(64, 1_024), At::Off(64, 1_536)]);
    assert!(rig.silent());
}

#[test]
fn a_note_cut_to_less_than_a_frame_still_ends_after_it_starts() {
    // It starts a hair before the clip's end, on the end's own frame: it ends on the next one,
    // in the next piece when its start is the last frame of a block.
    // With a first block of one frame, the blocks of 4,000 after it end on frame 24,000.
    for (first, block) in [(512, 512), (1, 4_000)] {
        let mut rig = Rig::new(vec![clip(0.0, 1.0, vec![note(60, 1.0 - 1e-7, 1.0)])]);
        rig.shared.play();
        rig.run(first, first as usize);
        rig.run(2 * BEAT, block);
        assert_eq!(rig.log, [At::On(60, BEAT), At::Off(60, BEAT + 1)], "block {block}");
    }
}

#[test]
fn a_key_played_by_hand_and_by_a_clip_is_never_started_twice() {
    let mut rig = Rig::new(vec![clip(0.0, 4.0, vec![note(60, 0.0, 4.0), note(62, 1.0, 1.0)])]);
    rig.shared.play();
    rig.run(1_000, 500);
    // The hand takes 60 over from the clip, which then doesn't end it.
    rig.feed.preview(Preview::On { key: 60, velocity: 1.0 });
    rig.feed.preview(Preview::On { key: 62, velocity: 1.0 });
    rig.run(5 * BEAT, 500);
    // The clip takes 62 back on its beat, so the hand's let-go of it does nothing.
    rig.feed.preview(Preview::Off { key: 62 });
    rig.feed.preview(Preview::Off { key: 60 });
    rig.run(500, 500);
    assert_eq!(
        rig.log,
        [At::On(60, 0), At::Off(60, 1_000), At::On(60, 1_000), At::On(62, 1_000), At::Off(62, BEAT), At::On(62, BEAT), At::Off(62, 2 * BEAT), At::Off(60, 1_000 + 5 * BEAT)]
    );
}

#[test]
fn notes_by_hand_that_waited_for_the_output_are_dropped_but_their_ends_kept() {
    let mut rig = Rig::new(vec![]);
    rig.feed.preview(Preview::On { key: 60, velocity: 1.0 });
    rig.run(512, 512);
    // The output closes and opens again; meanwhile one key is played and 60 let go.
    rig.feed.preview(Preview::On { key: 64, velocity: 1.0 });
    rig.feed.preview(Preview::Off { key: 64 });
    rig.feed.preview(Preview::On { key: 67, velocity: 1.0 });
    rig.feed.preview(Preview::Off { key: 60 });
    rig.player.prepare(RATE, 4096);
    rig.run(512, 512);
    assert_eq!(rig.log, [At::On(60, 0), At::Off(60, 512)], "no burst of 64 and 67; 60 still ends");
    assert!(rig.silent());
}

#[test]
fn a_split_plays_on_as_one_clip() {
    // Not looped: the note over the cut ends there and goes on in the second half.
    let c = clip(1.0, 4.0, vec![note(60, 0.0, 1.0), note(62, 1.5, 1.0), note(64, 3.0, 0.5)]);
    let (a, b) = split(&c, 3.0).unwrap();
    assert_eq!((a.length, b.start, b.offset, b.phase, b.length), (2.0, 3.0, 2.0, 0.0, 2.0));
    assert_eq!(a.notes, [note(60, 0.0, 1.0), note(62, 1.5, 0.5)]);
    assert_eq!(b.notes, [note(62, 2.0, 0.5), note(64, 3.0, 0.5)]);
    let mut rig = Rig::new(vec![a, b]);
    rig.shared.play();
    rig.run(6 * BEAT, 512);
    let at = |beats: f64| (beats * BEAT as f64) as u64;
    assert_eq!(rig.log, [At::On(60, at(1.0)), At::Off(60, at(2.0)), At::On(62, at(2.5)), At::Off(62, at(3.0)), At::On(62, at(3.0)), At::Off(62, at(3.5)), At::On(64, at(4.0)), At::Off(64, at(4.5))]);

    // Looped, cut between notes: the halves play what the whole clip did.
    let looped = MidiClipSpec { start: 0.0, offset: 0.5, content: 1.0, phase: 0.25, length: 5.0, notes: vec![note(60, 0.5, 0.25), note(62, 1.0, 0.25)] };
    let whole = {
        let mut rig = Rig::new(vec![looped.clone()]);
        rig.shared.play();
        rig.run(6 * BEAT, 512);
        rig.log
    };
    let (a, b) = split(&looped, 2.1).unwrap();
    let mut rig = Rig::new(vec![a, b]);
    rig.shared.play();
    rig.run(6 * BEAT, 512);
    assert_eq!(rig.log, whole);
    assert!(split(&looped, 0.0).is_none() && split(&looped, 5.0).is_none() && split(&looped, -1.0).is_none());
}

#[test]
fn a_split_of_a_trimmed_clip_plays_only_what_the_clip_did() {
    // The long note starts before the content (offset 2) or before the clip's
    // first beat (phase 2): the clip never plays it, nor do its halves.
    let trimmed = [
        MidiClipSpec { start: 0.0, offset: 2.0, content: 6.0, phase: 0.0, length: 6.0, notes: vec![note(60, 0.0, 8.0), note(64, 3.0, 0.5)] },
        MidiClipSpec { start: 0.0, offset: 0.0, content: 6.0, phase: 2.0, length: 4.0, notes: vec![note(60, 1.0, 8.0), note(64, 3.0, 0.5)] },
    ];
    for c in trimmed {
        let play = |clips: Vec<MidiClipSpec>| {
            let mut rig = Rig::new(clips);
            rig.shared.play();
            rig.run(8 * BEAT, 512);
            rig.log
        };
        let whole = play(vec![c.clone()]);
        assert_eq!(whole, [At::On(64, BEAT), At::Off(64, BEAT * 3 / 2)], "{c:?}");
        let (a, b) = split(&c, 2.0).unwrap();
        assert_eq!(play(vec![a, b]), whole, "{c:?}");
    }
}

#[test]
fn an_edit_ends_a_note_that_now_falls_before_its_clip() {
    // A looped clip's start trimmed past a sounding note while it plays, as the Timeline trims.
    let c = MidiClipSpec { start: 0.0, offset: 0.0, content: 4.0, phase: 0.0, length: 8.0, notes: vec![note(60, 0.0, 3.0)] };
    let mut rig = Rig::new(vec![c.clone()]);
    rig.shared.play();
    rig.run(BEAT, 500);
    rig.publish(vec![MidiClipSpec { start: 0.5, phase: 0.5, length: 7.5, ..c }]);
    rig.run(4 * BEAT, 500);
    assert_eq!(rig.log, [At::On(60, 0), At::Off(60, BEAT), At::On(60, 4 * BEAT)], "off at the edit, and on again in the next pass");
}

#[test]
fn bad_clips_and_notes_are_left_out_or_clamped() {
    let set = MidiClipSet::new(vec![
        MidiClipSpec { start: f64::NAN, ..clip(0.0, 1.0, vec![]) },
        clip(0.0, 0.0, vec![]),
        MidiClipSpec { content: f64::INFINITY, phase: -0.5, ..clip(0.0, 2.0, vec![note(200, 0.0, 1.0), note(60, f64::NAN, 1.0), note(61, 0.5, 0.0), note(62, 0.0, 9.0)]) },
    ]);
    assert_eq!(set.len(), 1);
    let c = &set.clips[0];
    assert_eq!((c.content, c.phase), (2.0, 1.5));
    let notes: Vec<(u8, f64)> = c.notes.iter().map(|x| (x.key, x.len)).collect();
    assert_eq!(notes, [(62, 2.0), (61, MIN_BEATS)], "cut at the content's end, and never shorter than the shortest note");
}

/// A small, repeatable random source for the soak test.
struct Dice(u64);

impl Dice {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn beats(&mut self, most: f64) -> f64 {
        (self.below(1_000_000) as f64 / 1_000_000.0) * most
    }
}

fn random_clips(d: &mut Dice) -> Vec<MidiClipSpec> {
    (0..d.below(4))
        .map(|_| {
            let content = 0.25 + d.beats(4.0);
            let notes = (0..d.below(24)).map(|_| note(40 + d.below(12) as u8, d.beats(content + 1.0), 0.01 + d.beats(3.0))).collect();
            MidiClipSpec { start: d.beats(12.0), offset: d.beats(0.5), content, phase: d.beats(content), length: 0.25 + d.beats(8.0), notes }
        })
        .collect()
}

#[test]
fn edits_jumps_and_tempo_changes_never_leave_a_note_sounding() {
    let mut d = Dice(0x9e37_79b9_7f4a_7c15);
    let mut rig = Rig::new(random_clips(&mut d));
    rig.shared.set_loop(1.0, 9.0);
    rig.shared.play();
    // Every piece is checked as it runs (see `Rig::block`): no note starts while it sounds or ends while silent.
    for _ in 0..12_000 {
        match d.below(12) {
            0 => rig.publish(random_clips(&mut d)),
            1 => rig.shared.seek(d.beats(14.0)),
            2 => rig.shared.set_tempo(60.0 + d.beats(180.0)),
            3 => rig.shared.stop(),
            4 => rig.shared.play(),
            5 => rig.shared.set_looping(d.below(2) == 0),
            // By hand, on the clips' keys too (hand and clip take keys over from each other).
            6 => {
                rig.feed.preview(Preview::On { key: 40 + d.below(16) as u8, velocity: 0.7 });
            }
            7 => {
                rig.feed.preview(Preview::Off { key: 40 + d.below(16) as u8 });
            }
            _ => {}
        }
        rig.block(1 + d.below(1_500) as usize);
        rig.held_by_playing_notes(40..52);
    }
    rig.shared.stop();
    rig.feed.preview(Preview::AllOff);
    rig.run(512, 512);
    assert!(rig.silent(), "every note ended");
    assert!(rig.ons().len() > 500, "and the test played a good many: {}", rig.ons().len());
}

#[test]
fn the_player_never_allocates_on_the_audio_thread() {
    let mut d = Dice(7);
    let mut rig = Rig::new(random_clips(&mut d));
    rig.shared.set_loop(0.0, 4.0);
    rig.shared.set_looping(true);
    rig.shared.play();
    let mut total = 0;
    for i in 0..600 {
        match i % 8 {
            // The main thread edits between blocks, as the note editor does.
            0 => rig.feed.publish(MidiClipSet::new(random_clips(&mut d))),
            1 => rig.shared.seek((i % 3) as f64),
            2 => rig.shared.set_tempo(90.0 + (i % 5) as f64 * 20.0),
            3 => rig.shared.stop(),
            4 => rig.shared.play(),
            5 => {
                rig.feed.preview(Preview::On { key: 60, velocity: 1.0 });
            }
            6 => {
                rig.feed.preview(Preview::AllOff);
            }
            _ => rig.feed.collect(),
        }
        let Rig { player, shared, clock, events, left, right, .. } = &mut rig;
        let (_, n) = realtime(|| {
            clock.run(shared, 333, |ctx, off| {
                events.clear();
                player.process(ctx, events, Buffers { left: &mut left[off..off + ctx.frames], right: &mut right[off..off + ctx.frames] });
            })
        });
        total += n;
    }
    assert_eq!(total, 0, "the MIDI clip player allocated or freed {total} times");
}

/// A stand-in instrument: 1.0 out while any note is held, as a plugin
/// instrument after the player on the track would play.
struct Held([bool; KEYS]);

impl Device for Held {
    fn process(&mut self, _: &ProcessContext, events: &mut Events, audio: Buffers<'_>) {
        let mut at = 0;
        for i in 0..audio.frames() {
            while let Some(e) = events.as_slice().get(at).filter(|e| e.frame() as usize == i) {
                match *e {
                    Event::NoteOn { key, .. } => self.0[usize::from(key)] = true,
                    Event::NoteOff { key, .. } => self.0[usize::from(key)] = false,
                    _ => {}
                }
                at += 1;
            }
            let v = if self.0.iter().any(|&k| k) { 1.0 } else { 0.0 };
            audio.left[i] += v;
            audio.right[i] += v;
        }
    }
}

#[test]
fn on_a_track_it_plays_the_instrument_after_it_on_the_frame() {
    let mut mixer = Mixer::new();
    let t = mixer.add_track("Keys").unwrap();
    let (player, mut feed) = player();
    let mut graph = mixer.start(RATE);
    // The player in the notes slot, ahead of the instrument in the source slot.
    mixer.set_notes(t, Some(Box::new(player))).unwrap();
    mixer.set_source(t, Some(Box::new(Held([false; KEYS])))).unwrap();
    let shared = TransportShared::default();
    let mut clock = Transport::new(RATE);
    feed.publish(MidiClipSet::new(vec![clip(1.0, 2.0, vec![note(60, 0.0, 0.5)])]));
    shared.play();
    let (mut out, mut total) = (Vec::new(), 0);
    for i in 0..200 {
        if i % 7 == 0 {
            feed.publish(MidiClipSet::new(vec![clip(1.0, 2.0, vec![note(60, 0.0, 0.5)])]));
        }
        let (_, n) = realtime(|| graph.run(&mut clock, &shared, 512));
        total += n;
        out.extend_from_slice(graph.master().0);
        mixer.collect();
        feed.collect();
    }
    assert_eq!(total, 0);
    assert_eq!(out.iter().position(|&x| x != 0.0), Some(BEAT as usize), "the note starts on beat 1's frame");
    assert_eq!(out.iter().rposition(|&x| x != 0.0), Some((BEAT * 3 / 2) as usize - 1), "and ends half a beat later");
}
