<picture>
  <source media="(prefers-color-scheme: dark)" srcset="brand/tunekore-logo-color.svg">
  <img alt="tuneKore" src="brand/tunekore-logo-color-on-light.svg" width="420">
</picture>

tuneKore is a lightweight DAW in the style of Ableton Live, built by devKore to
host our own instruments, effects and tools alongside the plugins we already
use (Serum, Arturia, Native Instruments). The first tool is the Analyzer, which
splits a song into stems.

[![CI](https://github.com/idevkore/dk-tunekore/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/idevkore/dk-tunekore/actions/workflows/ci.yml?query=branch%3Amain)

CI: builds and tests the engine and the app on macOS, and compiles and links
them on Windows and Linux, for every push to `main` and every pull request.

Status: Phase 0, the skeleton. The app opens, lists audio outputs and plays a
test tone with a level meter. Nothing else yet.

## Build and run

You need Rust, Node 24 and Xcode's command line tools (macOS).

```bash
npm install
npm run tauri dev
```

`npm run tauri build -- --bundles app` makes `tuneKore.app` in
`target/release/bundle/macos/`. The repo is one Cargo workspace: the audio
engine is the `tunekore-engine` crate in `engine/`, and the Tauri shell in
`src-tauri/` depends on it. To test the engine alone (it plays a short tone on
the default output unless `CI` is set): `cargo test -p tunekore-engine`.

- [`docs/PLAN.md`](docs/PLAN.md): the stack, architecture, roadmap, plugin
  support, stems and cross-platform path
- [`THIRD_PARTY.md`](THIRD_PARTY.md): every dependency and its licence
- [`brand/`](brand/README.md): the logo, the K› icon and the macOS app icon
