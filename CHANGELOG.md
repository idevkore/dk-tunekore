# Changelog

Every pull request that adds to tuneKore bumps the patch number
(`node scripts/version.mjs patch`) and adds its entry here, newest first.
Weekly releases, once they start, bump the minor number (see `docs/PLAN.md`
section 4).

## 0.1.13: open projects from the Finder

idevkore/dk-tunekore#51

- Double-clicking a `.tunek` file opens it in tuneKore, whether tuneKore is
  running or not. The app now registers as the owner of `.tunek` files, and
  macOS gives them the tuneKore project type. Dropping one on the Dock icon
  works too. If the open project has unsaved changes, it asks first.
- Fixed: a project you only looked at no longer asks to be saved when you
  quit. Clicking into the tempo box and out again, or picking the length or
  pattern already set, no longer counts as a change or adds an undo step.

## 0.1.12: one clock, tracks and the Timeline switch

idevkore/dk-tunekore#34, PR #45

- One beat clock in the engine (tempo, time signature, position, play, stop,
  loop) that everything runs on, and tracks with volume, pan, mute, solo and a
  meter, summed into the master. The Sampler is the first track and sounds
  exactly as before.
- A Sampler / Timeline switch in the main window: ⌘1 and ⌘2, the View menu,
  or Tab when nothing has focus. The Timeline is a placeholder until #36.
- Projects save as format v2, with a part for each feature. Older projects
  open as one Sampler track; a file that can't be read changes nothing.
- A tempo change while the pattern plays now lands at once, on the beat.
- Steps stay on their exact frame however long the pattern runs.
- Fixed: changing the audio output no longer replays the last pads hit, and a
  new project no longer counts as unsaved before you change anything.
- The engine's tests run under RealtimeSanitizer, which fails them if the
  audio thread allocates, locks or blocks.

## 0.1.11: a plugin host

idevkore/dk-tunekore#35, PR #46

- tuneKore can now host VST3 instruments and effects in the engine (the app
  gets them in #37): it finds the plugins on this Mac, loads them, plays notes
  and audio through them in time with the transport, saves and restores their
  sound exactly, and opens their own editors.
- Finding plugins runs in a separate helper, so a plugin that crashes, hangs
  or throws an error while loading is listed as failed instead of taking
  tuneKore down. Failures are remembered and skipped until the plugin changes
  or you retry them.
- A plugin that throws an error while running is switched off with a message
  instead of crashing tuneKore.
- Checked with Serum, Serum 2, all 40 Arturia plugins, Kontakt 7, Guitar Rig 7,
  Massive, Reaktor 6, Maschine 2, Komplete Kontrol, Replika and Solid Bus Comp.

## 0.1.10: touch discovery

idevkore/dk-tunekore#30, PR #31

- `docs/TOUCH.md`: what touch on the Mac means for tuneKore, written the day
  after Apple announced the touchscreen MacBook Pro. It covers what Apple
  shipped, how other DAWs handle touch, an audit of our UI, two routes to
  multi-touch, plugin touch pages, latency, design rules and a plan.

Docs only; no app change.

## 0.1.9: filter envelope and velocity

idevkore/dk-tunekore#19, idevkore/dk-tunekore#18

- A filter envelope on each pad: attack, decay, sustain, release and an
  amount of up to 8 octaves either way, which sweeps the cutoff on every hit
  and falls back when a gate pad is let go. Defaults to off.
- Velocity reaches the sound, not just the level: Velocity Cutoff makes soft
  hits darker (or brighter), and Velocity Time makes their attack and decay
  shorter (or longer), each set per pad.
- The Shape tab is grouped into Amp, Filter, Filter envelope and Velocity.
  The new settings save with the project; older projects open with them off.

## 0.1.8: pattern switches on the bar

idevkore/dk-tunekore#17

- Switching patterns A–D while playing now waits for the end of the pattern:
  the pattern playing finishes and the new one starts exactly on the wrap.
  The new pattern opens for editing at once; until it starts its button
  blinks with a dashed outline, the playing one is underlined, and a note
  says when it starts.
- Song mode switched on or off while playing lands the same way: the song
  starts, or its part plays to the end, at the wrap. The Song button blinks
  while it waits, and the part playing stays lit until it ends.
- A Switch option next to the patterns, At end or Now. Now lands pattern
  switches, and song mode turned off, on the next step, as before; a song
  still starts at the wrap, so its first part plays whole. The choice is
  saved in the `.tunek` project; older projects open with At end.
- Hits recorded while a switch waits go into the pattern you hear, not the
  queued one.
- Shortening the pattern while it plays keeps the playhead's place in the
  bar, so a waiting switch or the next song part still lands on the bar.
- Engine tests cover the timing: the wrap frame, swing and odd lengths,
  changing a queued switch, switching now, song mode on and off, stopping
  with a switch queued, a shorter pattern while a switch waits, and hits
  recorded while a switch waits or at the end of a pass.

## 0.1.5: tracker links

- `docs/PLAN.md` and `CLAUDE.md` point at the tuneKore issues on
  `idevkore/dk-tunekore` instead of their old `kore-issues` numbers
  (#24 → #11, #28 → #13, #29 → #14, #30 → #15).

## 0.1.4: big screens

idevkore/dk-tunekore#25

- The Sampler scales with the window: bigger pads, a wider pad panel and
  waveform, a wider Library, and steps that fill the window with near-square
  cells. 1280 and the 1040 minimum look as before.
- A velocity lane under the steps: drag the selected pad's bars up and down.
- Display zoom, 80–150%: View → Zoom In / Zoom Out / Actual Size (⌘+, ⌘−,
  ⌘0) or Display size in the Audio dialog; applies to every window and is
  remembered.
- Windows reopen at the size, place and display they were left at
  (tauri-plugin-window-state).

## 0.1.3: DAW essentials

idevkore/dk-tunekore#15, PR #9

- Song mode: chain patterns A–D into a song, played once or looped, and
  export the whole song.
- Per-step chance and repeats (ratchets), in an editor on right-click.
- Per-pad amp envelope (attack, decay, sustain, release) and a low, high or
  band-pass filter, on the pad's Shape tab.
- MIDI controllers play pads 1–16 (notes 36–51) at the velocity played.
- Sample from a microphone or audio interface onto a pad.
- A master level and a limiter that keeps the output under -0.3 dBFS.
- The Library opens on your Splice library wherever Splice keeps it, including
  an external drive.
- Autosave with recovery after a crash; collect a project's samples into
  a folder beside it; export stems, one WAV per pad.
- Fixed: stopping the pattern (Space or the stop button) now silences the
  pads straight away instead of letting each sample play to its end.
- Adapted from soundCraft (MIT OR Apache-2.0): the limiter, the filter and
  envelope decay, the input recorder's safeguards and the step-chance PRNG.

## 0.1.2: the Sampler's second pass

idevkore/dk-tunekore#14, PR #8

- Patterns A–D, switched while playing and copied between.
- Record pads into the steps while the pattern runs; a metronome on the beat.
- Per-step velocity, choke groups, and undo and redo for every edit.
- Chop any pad's sample at its hits onto the empty pads; bounce the pattern
  back onto a pad.
- Trim handles on the waveform, an Add a sound row, and a prompt before
  closing with unsaved changes.
- The Analyzer finds a song's tempo and key (with its Camelot code) and can
  set the Sampler's tempo.

## 0.1.1: the Sampler (MVP)

idevkore/dk-tunekore#13, PR #7

- The main window is the Sampler: a Library of samples (any folder, `~/Splice`
  included) with search and preview, 16 pads played by mouse or keyboard, and
  a 16-step sequencer with tempo and swing.
- Each pad: start and end, level, pan, pitch, gate, reverse and mute.
- Projects save as `.tunek`; the pattern exports as a 48 kHz WAV.
- The Analyzer sends a stem to a pad whole, or chopped at its hits from the
  playhead.
- The plan makes the Sampler Phase 2 and drops Audio Units: VST3 and CLAP only.

## 0.1.0: the Analyzer

PRs #1 to #6

- The app shell, the Rust engine, output devices and a test tone (Phase 0).
- The Analyzer: split a song into vocals, drums, bass and other with
  Demucs, then play, solo, mute and export the stems (Phase 1).
