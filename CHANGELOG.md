# Changelog

Every pull request that adds to tuneKore bumps the patch number
(`node scripts/version.mjs patch`) and adds its entry here, newest first.
Weekly releases, once they start, bump the minor number (see `docs/PLAN.md`
section 4).

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
