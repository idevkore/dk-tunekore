# Changelog

Every pull request that adds to tuneKore bumps the patch number
(`node scripts/version.mjs patch`) and adds its entry here, newest first.
Weekly releases, once they start, bump the minor number (see `docs/PLAN.md`
section 4).

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
- Autosave with recovery after a crash; collect a project's samples into
  a folder beside it; export stems, one WAV per pad.
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
