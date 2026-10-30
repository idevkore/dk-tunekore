<picture>
  <source media="(prefers-color-scheme: dark)" srcset="brand/tunekore-logo-color.svg">
  <img alt="tuneKore" src="brand/tunekore-logo-color-on-light.svg" width="420">
</picture>

tuneKore is a lightweight DAW in the style of Ableton Live, built by devKore to
host our own instruments, effects and tools alongside the plugins we already
use (Serum, Arturia, Native Instruments). It is sampling first, and new tech
only: VST3 and CLAP plugins, no legacy formats. The main window has two
views on one beat clock, the Sampler and the Timeline (⌘1 and ⌘2); the
Analyzer splits a song into stems and sends them to the Sampler's pads.

[![CI](https://github.com/idevkore/dk-tunekore/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/idevkore/dk-tunekore/actions/workflows/ci.yml?query=branch%3Amain)

CI: builds and tests the engine, the Analyzer, the stem worker and the app on
macOS, and compiles and links them on Windows and Linux, for every push to
`main` and every pull request.

Status: the Sampler (Phase 2, the MVP) is in; the Timeline (Phase 4) has
tracks and audio clips, with MIDI clips and plugins on tracks next.

- **Sampler** (main window):
  - **Library:** pick a sample folder, click or arrow through samples to hear
    them, and drag them onto 16 pads (or select a pad and press Enter, or use
    the Add a sound row under the steps).
  - **Pads:** start and end (drag them on the waveform), level, pan, pitch,
    gate, reverse, mute and a choke group; on the Shape tab, an amp envelope
    (attack, decay, sustain, release) and a low, high or band-pass filter.
    Chop any pad's sample at its hits onto the empty pads.
  - **Steps:** four patterns (A–D) of 8, 16 or 32 steps with tempo and swing.
    Right-click a step for its velocity, chance and repeats (ratchets), or
    drag the velocity lane under the steps. Arm
    Record and play pads (keys or a MIDI controller) while it runs; a
    metronome clicks on the beat.
  - **Song:** chain patterns into a song and play it through, looped or once.
  - **Sampling:** select a pad and press Sample from input to record from a
    microphone or audio interface (pick it in Audio settings). The take is
    trimmed and saved in `Music/tuneKore/Samples/Recordings`; macOS asks for
    microphone access the first time.
  - **MIDI:** plug in a controller and notes C1 to D♯2 (36 to 51) play pads 1
    to 16 at the velocity you play; it connects by itself.
  - **Display:** the layout scales to big screens; View → Zoom In / Zoom Out
    (⌘+, ⌘−, ⌘0) sizes the whole interface; windows reopen where you left them.
  - **Output:** a master level, and a limiter that keeps the output under
    -0.3 dBFS.
  - **Projects:** undo and redo cover every edit; save as `.tunek`, collect
    the samples into a folder beside the project, and autosave recovers
    unsaved work after a crash. Export the pattern or the whole song as a WAV
    or as stems (one WAV per pad), or bounce it onto a pad to resample it.
- **Timeline** (⌘2): tracks of audio clips on the Sampler's beat clock, with
  the Sampler as the first track.
  - **Tracks:** name, colour, volume, pan, mute, solo and a meter; drag a
    track's foot to change its height. An arm button waits for recording.
  - **Clips:** drag samples from the Library or files from Finder onto a
    track (they play one after another), or below the tracks for a track
    each, lined up; the Analyzer sends a stem, or all four, to new tracks at
    bar 1. Move them (⌥ copies), trim either edge, loop one by
    dragging the top of its right edge, fade in and out from its top corners,
    set its gain, split at the playhead (⌘E), duplicate (⌘D) and delete (⌫).
    Drags snap to the grid (bar, beat, 1/8, 1/16 or off; hold ⌘ to ignore it).
  - **Ruler:** click to move the playhead; drag in its top strip for a loop
    range, drag the brace to move it, click it to turn the loop on or off.
    Zoom with ⌘-scroll, pinch, or = and -; the playhead is followed while
    playing.
  - Clips aren't warped yet: they play at their own speed, so a clip's length
    in beats follows the tempo. Its files are decoded into memory.
  - Undo and redo cover the Timeline's edits; tracks, clips, the loop and the
    view are saved in the project, and collecting samples copies the clips'
    files too.
- **Analyzer:** open a song, split it into vocals, drums, bass and other,
  then play, solo, mute and export the stems, or send one to the pads, whole
  or chopped at its hits from the playhead, or to the Timeline, one stem or
  all four on tracks of their own. It finds the song's tempo and key
  (with its Camelot code) and can set the Sampler to that tempo. Separation runs on this Mac in a
  separate worker process, with Demucs. Its weights are research-only, so a
  model we may ship replaces it before release (see `docs/PLAN.md` section 6).

Keys: <kbd>Space</kbd> plays and stops, <kbd>⌘1</kbd> and <kbd>⌘2</kbd>
show the Sampler and the Timeline, <kbd>1</kbd>–<kbd>4</kbd>,
<kbd>Q</kbd>–<kbd>R</kbd>, <kbd>A</kbd>–<kbd>F</kbd> and <kbd>Z</kbd>–<kbd>V</kbd>
play the pads, <kbd>/</kbd> searches the Library, <kbd>⌘S</kbd>,
<kbd>⌘O</kbd>, <kbd>⌘N</kbd> and <kbd>⌘E</kbd> save, open, start a new
project and export; <kbd>⌘Z</kbd> and <kbd>⇧⌘Z</kbd> undo and redo.

## Build and run

You need Rust, Node 24, Xcode's command line tools (macOS) and, for the
Analyzer's stem worker, [uv](https://docs.astral.sh/uv/).

```bash
npm install
(cd stems && uv sync)
npm run tauri dev
```

The main window is the Sampler; open the Analyzer from its top bar. Demucs's weights (about 160 MB)
download to this Mac on first use. See [`stems/README.md`](stems/README.md).

`npm run tauri build -- --bundles app` makes `tuneKore.app` in
`target/release/bundle/macos/`. The repo is one Cargo workspace: the audio
engine is the `tunekore-engine` crate in `engine/`, the Analyzer's decoding,
worker and export code is `tunekore-analyzer` in `analyzer/`, and the Tauri
shell in `src-tauri/` depends on both. To try the UI in a browser without
Rust, run `npm run dev` and open `http://localhost:1420/?mock` (the Sampler
and the Timeline) or `/analyzer.html?mock` (the Analyzer). The stem worker is Python, in
`stems/`. To test the engine alone (it plays a short tone on the default
output unless `CI` is set): `cargo test -p tunekore-engine`.

- [`docs/PLAN.md`](docs/PLAN.md): the stack, architecture, roadmap, plugin
  support, stems and cross-platform path
- [`CHANGELOG.md`](CHANGELOG.md): what each version added. Every pull request
  that adds to the app bumps the patch number with `npm run version:bump`
  (CI checks the copies agree); the version shows in the app's status bar.
- [`THIRD_PARTY.md`](THIRD_PARTY.md): every dependency and its licence
- [`stems/`](stems/README.md): the stem worker and the model comparison tools
- [`brand/`](brand/README.md): the logo, the K› icon and the macOS app icon
