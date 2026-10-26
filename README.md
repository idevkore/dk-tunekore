<picture>
  <source media="(prefers-color-scheme: dark)" srcset="brand/tunekore-logo-color.svg">
  <img alt="tuneKore" src="brand/tunekore-logo-color-on-light.svg" width="420">
</picture>

tuneKore is a lightweight DAW in the style of Ableton Live, built by devKore to
host our own instruments, effects and tools alongside the plugins we already
use (Serum, Arturia, Native Instruments). It is sampling first, and new tech
only: VST3 and CLAP plugins, no legacy formats. The main window is the
Sampler; the Analyzer splits a song into stems and sends them to its pads.

[![CI](https://github.com/idevkore/dk-tunekore/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/idevkore/dk-tunekore/actions/workflows/ci.yml?query=branch%3Amain)

CI: builds and tests the engine, the Analyzer, the stem worker and the app on
macOS, and compiles and links them on Windows and Linux, for every push to
`main` and every pull request.

Status: Phase 2, the Sampler (the MVP).

- **Sampler** (main window): pick a sample folder in the Library, click or
  arrow through samples to hear them, and drag them onto 16 pads (or select
  a pad and press Enter). Each pad has start and end, level, pan, pitch,
  gate, reverse and mute. Program a 16-step pattern with tempo and swing,
  save it as a `.tunek` project, and export it as a WAV.
- **Analyzer:** open a song, split it into vocals, drums, bass and other,
  then play, solo, mute and export the stems, or send one to the pads, whole
  or chopped at its hits from the playhead. Separation runs on this Mac in a
  separate worker process, with Demucs. Its weights are research-only, so a
  model we may ship replaces it before release (see `docs/PLAN.md` section 6).

Keys: <kbd>Space</kbd> plays and stops, <kbd>1</kbd>–<kbd>4</kbd>,
<kbd>Q</kbd>–<kbd>R</kbd>, <kbd>A</kbd>–<kbd>F</kbd> and <kbd>Z</kbd>–<kbd>V</kbd>
play the pads, <kbd>/</kbd> searches the Library, <kbd>⌘S</kbd>,
<kbd>⌘O</kbd>, <kbd>⌘N</kbd> and <kbd>⌘E</kbd> save, open, start a new
project and export.

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
Rust, run `npm run dev` and open `http://localhost:1420/?mock` (the Sampler)
or `/analyzer.html?mock` (the Analyzer). The stem worker is Python, in
`stems/`. To test the engine alone (it plays a short tone on the default
output unless `CI` is set): `cargo test -p tunekore-engine`.

- [`docs/PLAN.md`](docs/PLAN.md): the stack, architecture, roadmap, plugin
  support, stems and cross-platform path
- [`THIRD_PARTY.md`](THIRD_PARTY.md): every dependency and its licence
- [`stems/`](stems/README.md): the stem worker and the model comparison tools
- [`brand/`](brand/README.md): the logo, the K› icon and the macOS app icon
