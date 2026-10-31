# Changelog

Every pull request that adds to tuneKore bumps the patch number
(`node scripts/version.mjs patch`) and adds its entry here, newest first.
Weekly releases, once they start, bump the minor number (see `docs/PLAN.md`
section 4).

## 0.1.18: plugins on tracks and pads

idevkore/dk-tunekore#37

- **Instrument tracks.** "+ Instrument" in the Timeline picks a VST3
  instrument (Serum, Arturia, Kontakt…) and puts it on a new track of its own.
  A MIDI controller's keys play the selected instrument track while the
  Timeline shows, with pitch bend, pressure, the mod wheel and the sustain
  pedal; otherwise they play the pads as before.
- **Effects on any track.** Select a track and its devices show under the
  tracks: switch each on or off, open its window, move it along the chain,
  remove it, or add one with "+ Effect". The Sampler's track takes effects too.
- **A plugin on a pad.** "Plugin…" in a pad's settings makes the pad play a
  plugin instrument instead of its sample: its steps play C3 plus its pitch
  for a step, and by hand for as long as you hold it, at the pad's level and
  pan. The sample stays, for when you take the plugin off. Exports and bounces
  of the pattern leave plugin pads out for now.
- **Plugin windows.** Each plugin opens in a window of its own, titled with its
  track, floating over tuneKore and hidden while you're in another app. Several
  can be open at once.
- **The plugin list** comes from a scan in a helper program, so a plugin that
  crashes while being looked at can't take tuneKore down. Rescan looks at
  every plugin again; the ones that failed are listed with why, with Retry.
- **Saved with the project.** Every plugin's sound comes back when the project
  opens. One that isn't installed any more stays in its place, with its sound
  kept, until you Reload it. One whose saved sound doesn't load says so and
  keeps that sound in the project until you choose Keep. One that won't give
  its sound when you save (an Arturia plugin that isn't activated) says so, and
  the project keeps the sound it had. Plugin parameters can be learned for MIDI
  knobs from Audio settings' list.
- A project can only load plugins from the plugin list or the plugin folders,
  never from anywhere else its file names.
- The plugin host joined the main build, CI and the licence check, and the app
  bundle gets the entitlements that hosting needs. Quitting lets every plugin
  close down first (Serum 2 used to crash at exit).
- Found on this Mac: the Arturia plugins aren't activated, so they won't save
  their sound and Analog Lab V is silent; Kontakt's and Solid Bus Comp's extra
  inputs and outputs wait for the Mixer.

## 0.1.17: open projects from the Finder

idevkore/dk-tunekore#51

- Double-clicking a `.tunek` file opens it in tuneKore, whether tuneKore is
  running or not. The app now registers as the owner of `.tunek` files, and
  macOS gives them the tuneKore project type. Dropping one on the Dock icon
  works too. If the open project has unsaved changes, it asks first.
- Fixed: a project you only looked at no longer asks to be saved when you
  quit. Clicking into the tempo box and out again, or picking the length or
  pattern already set, no longer counts as a change or adds an undo step.

## 0.1.16: 64-step patterns

idevkore/dk-tunekore#60

- The Steps picker now goes up to 64: four bars in one pattern, enough for a
  four-chord progression with one chord to a bar. The engine already played
  64; the picker stopped at 32.
- A pattern longer than 32 steps shows two bars of the grid at a time. Pick
  Bars 1–2 or Bars 3–4 beside the Steps picker; the page that is playing has
  a lime foot, and Follow (on by default) turns the page to keep the
  playhead in view. The arrow keys move on to the next page, and the velocity
  lane and the right-click step editor work on every step.
- Engine tests play a 64-step pattern through two passes (with velocity,
  chance and a ratchet on the last bar), switch patterns on the bar and on the
  next step, cut a 64-step pattern short, render a song of 64-step patterns,
  and run it all on the audio thread without allocating (also under RTSAN).
  A project test saves and reads back a pattern that uses all 64 steps.

## 0.1.15: colour themes

idevkore/dk-tunekore#56

- Pick a colour theme from the new button beside the view switch: tuneKore
  (the default, unchanged), Dracula, Nord, Gruvbox Dark and Tokyo Night, or
  the light Solarized Light, GitHub Light and Gruvbox Light. It applies at
  once and is remembered for the next launch.
- Every colour in the window comes from the theme, the Timeline's lanes,
  ruler and clips included. On a light theme, clips take a tint of their
  track's colour, waveforms are shaded darker, and the logo's TUNE is in ink.
- Text in each theme reads at 4.5:1 or better against its backgrounds.

## 0.1.14: the Timeline, with tracks and audio clips

idevkore/dk-tunekore#36, PR #47

- The Timeline (⌘2) shows the Sampler as its first track, with audio tracks
  below it: each has a colour, a name, volume, pan, mute, solo, a meter and a
  height you can drag. Tracks are now audio, instrument or the Sampler.
- Drag samples from the Library or files from Finder onto a track, or below
  the tracks to give each its own track, lined up. The Analyzer sends a stem,
  or all four, to new tracks at bar 1, never overwriting an earlier send.
- Move clips (⌥ copies), trim either edge, loop a clip by dragging the top of
  its right edge, fade it in and out from its top corners, set its gain, split
  at the playhead (⌘E), duplicate (⌘D) and delete (⌫). Edits snap to the grid
  you pick; hold ⌘ (Ctrl on Windows and Linux) to ignore it.
- Clips play in time with the Sampler, starting on the exact sample. Short
  fades keep edits, seeks, loops and a clip's own loop point from clicking,
  and a clip that's playing plays straight on through a tempo change.
- The ruler moves the playhead and sets the loop. Zoom with ⌘-scroll, a pinch,
  or = and -. Rename a track with F2 or Enter.
- Undo and redo cover the Timeline's edits, and an undo only puts back the
  faders that step changed. The project keeps the tracks, clips, loop and
  view; track numbers are never reused; collecting samples copies the clips'
  files too.

## 0.1.13: MIDI learn

idevkore/dk-tunekore#20, PR #50

- MIDI learn: click the MIDI light (it reads LEARN), click a control, move a
  knob or fader, and it controls it. Stay in the mode to map more; Esc leaves.
  Or right-click a control to learn it, learn it for one pad only, or forget it.
- Pad settings for the selected pad or one pad, tempo, swing, the master level,
  and the Sampler track's volume and pan. 7-bit or 14-bit controllers, each
  control with its own curve, and one undo step per knob turn.
- When a knob and its value disagree: pick-up (the default), catch up gradually,
  or jump, set in Audio settings, where the list of mappings is too.
- Mappings save with the project; older projects open with none.
- Turning a pad's cutoff while it rings now glides instead of stepping.
- Fixed: a MIDI controller plugged in after tuneKore starts now connects (macOS).

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
