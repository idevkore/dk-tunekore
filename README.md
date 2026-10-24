<picture>
  <source media="(prefers-color-scheme: dark)" srcset="brand/musikore-logo-color.svg">
  <img alt="musiKore" src="brand/musikore-logo-color-on-light.svg" width="420">
</picture>

musiKore is a lightweight DAW in the style of Ableton Live, built by devKore to
host our own instruments, effects and tools alongside the plugins we already
use (Serum, Arturia, Native Instruments). The first tool is the Analyzer, which
splits a song into stems.

[![CI](https://github.com/idevkore/dk-musikore/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/idevkore/dk-musikore/actions/workflows/ci.yml?query=branch%3Amain)

CI: builds and tests the engine and the app on macOS, and compiles and links
them on Windows and Linux, for every push to `main` and every pull request.

Status: Phase 0, the skeleton. The app opens, lists audio outputs and plays a
test tone with a level meter. Nothing else yet.

## Build and run

You need Rust, CMake, Node 24 and Xcode's command line tools (macOS).

```bash
npm install
npm run tauri dev
```

`npm run tauri build -- --bundles app` makes `musiKore.app` in
`src-tauri/target/release/bundle/macos/`. The C++ engine in `engine/` is built
by `src-tauri/build.rs` with CMake. Its first build downloads RtAudio, pinned by
SHA256. `ctest` for the engine alone: `cmake -S engine -B build && cmake --build build && ctest --test-dir build`.

- [`docs/PLAN.md`](docs/PLAN.md): the stack, architecture, roadmap, plugin
  support, stems and cross-platform path
- [`THIRD_PARTY.md`](THIRD_PARTY.md): every dependency and its licence
- [`brand/`](brand/README.md): the logo, the K› icon and the macOS app icon
