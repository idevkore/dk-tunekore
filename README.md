<picture>
  <source media="(prefers-color-scheme: dark)" srcset="brand/musikore-logo-color.svg">
  <img alt="musiKore" src="brand/musikore-logo-color-on-light.svg" width="420">
</picture>

musiKore is a lightweight DAW in the style of Ableton Live, built by devKore to
host our own instruments, effects and tools alongside the plugins we already
use (Serum, Arturia, Native Instruments). The first tool is the Analyzer, which
splits a song into stems.

[![CI](https://github.com/idevkore/dk-musikore/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/idevkore/dk-musikore/actions/workflows/ci.yml?query=branch%3Amain)

CI: builds and tests the engine, the Analyzer, the stem worker and the app on
macOS, and compiles and links them on Windows and Linux, for every push to
`main` and every pull request.

Status: Phase 1, the Analyzer. Open a song, split it into vocals, drums, bass
and other, then play, solo, mute and export the stems. Separation runs on this
Mac in a separate worker process, with Demucs or Meta's SAM Audio; neither
model ships yet (see `docs/PLAN.md` section 6).

## Build and run

You need Rust, Node 24, Xcode's command line tools (macOS) and, for the
Analyzer's stem worker, [uv](https://docs.astral.sh/uv/).

```bash
npm install
(cd stems && uv sync --extra sam)
npm run tauri dev
```

Open the Analyzer from the main window. Model weights download to this Mac on
first use; SAM Audio needs a Hugging Face login first
(`stems/.venv/bin/hf auth login`). See [`stems/README.md`](stems/README.md).

`npm run tauri build -- --bundles app` makes `musiKore.app` in
`target/release/bundle/macos/`. The repo is one Cargo workspace: the audio
engine is the `musikore-engine` crate in `engine/`, the Analyzer's decoding,
worker and export code is `musikore-analyzer` in `analyzer/`, and the Tauri
shell in `src-tauri/` depends on both. The stem worker is Python, in
`stems/`. To test the engine alone (it plays a short tone on the default
output unless `CI` is set): `cargo test -p musikore-engine`.

- [`docs/PLAN.md`](docs/PLAN.md): the stack, architecture, roadmap, plugin
  support, stems and cross-platform path
- [`THIRD_PARTY.md`](THIRD_PARTY.md): every dependency and its licence
- [`stems/`](stems/README.md): the stem worker and the model comparison tools
- [`brand/`](brand/README.md): the logo, the K› icon and the macOS app icon
