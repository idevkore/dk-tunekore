# Changelog

Every pull request that adds to tuneKore bumps the patch number
(`node scripts/version.mjs patch`) and adds its entry here, newest first.
Weekly releases, once they start, bump the minor number (see `docs/PLAN.md`
section 4).

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
