<picture>
  <source media="(prefers-color-scheme: dark)" srcset="brand/tunekore-logo-color.svg">
  <img alt="tuneKore" src="brand/tunekore-logo-color-on-light.svg" width="420">
</picture>

[![CI](https://github.com/idevkore/dk-tunekore/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/idevkore/dk-tunekore/actions/workflows/ci.yml?query=branch%3Amain)

CI: whether `main` builds and passes its tests on macOS, Windows and Linux.

tuneKore is a lightweight DAW in the style of Ableton Live, built by devKore to
host our own instruments, effects and tools alongside the plugins we already
use (Serum, Arturia, Native Instruments). It is sampling first, and new tech
only: VST3 and CLAP plugins, no legacy formats. macOS first, then Windows and
Linux.

## What's in it

The main window has two views on one beat clock, the Sampler and the
Timeline (<kbd>⌘1</kbd> and <kbd>⌘2</kbd>). The Analyzer is a second window
that splits a song into stems.

**Status:** the Sampler, the Timeline with audio clips, and VST3 plugins on
tracks and pads are in. MIDI clips, CLAP, recording, automation, the Mixer and
export come next (`docs/PLAN.md` section 4 has the roadmap).

### Sampler

- **Library and pads:** pick a sample folder, click or arrow through samples to
  hear them, and drag them onto 16 pads (or select a pad and press
  <kbd>Enter</kbd>). Each pad has start and end, level, pan, pitch, gate,
  reverse, mute, a choke group, an amp envelope and a low, high or band-pass
  filter. Chop a sample at its hits onto the empty pads. **Clear pad** (in the
  Inspector) empties one.
- **Steps:** four patterns (A–D) of 8, 16, 32 or 64 steps, with tempo and
  swing. Right-click a step for its velocity, chance and repeats (ratchets), or
  drag the velocity lane. Arm Record and play pads while it runs; a metronome
  clicks on the beat.
- **Song:** chain patterns into a song and play it through, looped or once.
- **Sampling:** press Sample from input to record from a microphone or audio
  interface (pick it in Audio settings). Takes are trimmed and saved in
  `Music/tuneKore/Samples/Recordings`.
- **Output:** a master level and a limiter that keeps the output under
  -0.3 dBFS.

### Timeline

- **Tracks:** audio tracks and instrument tracks, with name, colour, volume,
  pan, mute, solo and a meter. The Sampler is the first track.
- **Clips:** drag samples from the Library or files from Finder onto a track.
  Move, copy (<kbd>⌥</kbd>), trim, loop, fade, set gain, split at the playhead
  (<kbd>⌘E</kbd>), duplicate (<kbd>⌘D</kbd>) and delete. Drags snap to the
  grid; hold <kbd>⌘</kbd> to ignore it.
- **Ruler:** click to move the playhead, drag its top strip for a loop range,
  zoom with <kbd>⌘</kbd>-scroll or pinch.
- Clips aren't warped yet: they play at their own speed.

### Plugins

- **Instrument tracks:** "+ Instrument" picks a VST3 instrument for a new
  track. A MIDI controller plays the selected instrument track while the
  Timeline shows; otherwise it plays the pads.
- **Effects:** select any track, including the Sampler's, and add effects in
  the device strip under the tracks.
- **Pads:** "Plugin…" in a pad's settings makes the pad play a plugin
  instrument instead of its sample.
- **Windows and state:** each plugin opens in its own window, and its sound is
  saved with the project. Plugin parameters can be MIDI learn targets.
- Exports and bounces leave plugin pads out for now.

### Analyzer

Open a song and split it into vocals, drums, bass and other. Play, solo, mute
and export the stems, or send them to the pads (whole or chopped) or to
Timeline tracks. It also finds the tempo and key (with its Camelot code) and
can set the Sampler to that tempo. Separation runs on this Mac in a separate
worker, with Demucs. Its weights are research-only, so a model we may ship
replaces it before release (`docs/PLAN.md` section 6).

### MIDI

Plug in a controller and it connects by itself. Notes C1 to D♯2 (36 to 51)
play pads 1 to 16. MIDI learn maps knobs and faders (7-bit, or 14-bit pairs)
to pad settings, tempo, swing, the master level, track volume and pan, and
plugin parameters. Click the MIDI light in the top bar, click a control and
move a knob, or right-click a control. The mappings are saved with the project.

### Projects and display

- Undo and redo cover every edit. Save as `.tunek`, double-click a `.tunek` in
  Finder to open it, and collect the samples into a folder beside the
  project. Autosave recovers unsaved work after a crash.
- Export the pattern or the whole song as a WAV or as stems (one WAV per pad),
  or bounce it onto a pad.
- Eight colour themes (tuneKore, Dracula, Nord, Gruvbox Dark, Tokyo Night, and
  the light Solarized Light, GitHub Light and Gruvbox Light), picked from the
  top bar.
- The layout scales to big screens; View → Zoom In / Zoom Out
  (<kbd>⌘+</kbd>, <kbd>⌘−</kbd>, <kbd>⌘0</kbd>) sizes the whole interface.

### Keys

| Key | Does |
| --- | --- |
| <kbd>Space</kbd> | play and stop |
| <kbd>⌘1</kbd>, <kbd>⌘2</kbd> | show the Sampler, the Timeline |
| <kbd>1</kbd>–<kbd>4</kbd>, <kbd>Q</kbd>–<kbd>R</kbd>, <kbd>A</kbd>–<kbd>F</kbd>, <kbd>Z</kbd>–<kbd>V</kbd> | play the pads |
| <kbd>/</kbd> | search the Library |
| <kbd>⌘S</kbd>, <kbd>⌘O</kbd>, <kbd>⌘N</kbd>, <kbd>⌘E</kbd> | save, open, new project, export |
| <kbd>⌘Z</kbd>, <kbd>⇧⌘Z</kbd> | undo, redo |

## Build and run

You need Rust, Node 24, Xcode's command line tools (macOS) and, for the
Analyzer's stem worker, [uv](https://docs.astral.sh/uv/).

```bash
npm install
(cd stems && uv sync)
npm run tauri dev
```

Demucs's weights (about 160 MB) download to this Mac on first use. See
[`stems/README.md`](stems/README.md).

`npm run tauri build -- --bundles app` makes `tuneKore.app` in
`target/release/bundle/macos/`.

To try the UI in a browser without Rust, run `npm run dev` and open
`http://localhost:1420/?mock` (the Sampler and the Timeline) or
`/analyzer.html?mock` (the Analyzer).

To test the engine alone (it plays a short tone on the default output unless
`CI` is set): `cargo test -p tunekore-engine`.

## How the repo is laid out

One Cargo workspace:

- `engine/`: the audio engine (`tunekore-engine`)
- `analyzer/`: the Analyzer's decoding, worker and export code
- `plugins/`: the VST3 host and the plugin scanner
- `src-tauri/`: the Tauri shell, which depends on all of the above
- `src/`: the React UI
- `stems/`: the stem worker, in Python

## Contributing

CI (`.github/workflows/ci.yml`) builds and tests the engine, the Analyzer, the
stem worker and the app on macOS, and compiles and links them on Windows and
Linux, for every push to `main` and every pull request. A pull request into
`main` needs CI to pass and a code owner's approval (`.github/CODEOWNERS`).
Report security problems privately, as [`SECURITY.md`](SECURITY.md) says.

- [`docs/PLAN.md`](docs/PLAN.md): the stack, architecture, roadmap, plugin
  support, stems and cross-platform path
- [`CHANGELOG.md`](CHANGELOG.md): what each version added. A pull request that
  adds to the app bumps the patch number with `npm run version:bump` (CI
  checks the copies agree); the version shows in the app's status bar.
- [`THIRD_PARTY.md`](THIRD_PARTY.md): every dependency and its licence
- [`stems/`](stems/README.md): the stem worker and the model comparison tools
- [`brand/`](brand/README.md): the logo, the K› icon and the macOS app icon
