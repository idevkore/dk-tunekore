# Third-party software

Every dependency tuneKore builds on, with its licence. The rule (`CLAUDE.md`):
MIT, BSD, Apache-2.0, zlib, ISC or public domain. MPL-2.0 only for unmodified
files. Nothing GPL, AGPL or LGPL goes into the app.

`npm run licenses` reads the root `Cargo.lock` and `package-lock.json` and fails CI when
any package has no option on that list. It takes the permissive branch of a
dual licence (for example `MIT OR Apache-2.0 OR LGPL-2.1-or-later` is used as
MIT). Update this file in the same PR that adds a dependency.

## Engine, Analyzer and app shell (Rust), direct dependencies

One Cargo workspace: `engine/` (`tunekore-engine`), `analyzer/`
(`tunekore-analyzer`) and `src-tauri/` (`tunekore`).

| Crate | Version | Licence | Use |
| --- | --- | --- | --- |
| [cpal](https://github.com/RustAudio/cpal) | 0.18.2 | Apache-2.0 | Engine: audio device I/O through CoreAudio on macOS, WASAPI on Windows and ALSA on Linux |
| [rtrb](https://github.com/mgeier/rtrb) | 0.4.0 | MIT OR Apache-2.0 | Engine: lock-free queues that hand stems to the audio thread and back |
| [rubato](https://github.com/HEnquist/rubato) | 5.0.1 | MIT OR Apache-2.0 | Engine: resamples stems to the output device's rate (off the audio thread) |
| [symphonia](https://github.com/pdeljanov/Symphonia) | 0.6.1 | MPL-2.0 (unmodified) | Analyzer: decodes WAV, AIFF, FLAC, MP3 and M4A (AAC, ALAC) |
| [hound](https://github.com/ruuda/hound) | 3.5.1 | Apache-2.0 | Analyzer: writes the exported stems as WAV |
| [rustfft](https://github.com/ejmahler/RustFFT) | 6.4.1 | MIT OR Apache-2.0 | Analyzer: the FFTs behind tempo and key detection (also pulled in by rubato) |
| [serde](https://github.com/serde-rs/serde) | 1.0.229 | MIT OR Apache-2.0 | Engine, Analyzer and shell: data sent to the UI and the stem worker |
| [serde_json](https://github.com/serde-rs/json) | 1.0.154 | MIT OR Apache-2.0 | Analyzer and shell: the stem worker's JSON-lines protocol |
| [tauri](https://github.com/tauri-apps/tauri) | 2.12.5 | Apache-2.0 OR MIT | Shell: windows, menus, IPC |
| [tauri-plugin-dialog](https://github.com/tauri-apps/plugins-workspace) | 2.8.3 | Apache-2.0 OR MIT | Shell: the Open and Export folder dialogs |
| tauri-build | 2.7.4 | Apache-2.0 OR MIT | Shell: build step |
| [tempfile](https://github.com/Stebalien/tempfile) | 3.27.0 | MIT OR Apache-2.0 | Tests only |

The lockfile has 471 crates in all, counting every platform's. Nearly all are
MIT or Apache-2.0 (or both). The rest are Unicode-3.0, Zlib, BSD-3-Clause, ISC,
Unlicense, CC0-1.0 and 0BSD. cpal brings `coreaudio-rs` and `objc2-core-audio`
on macOS, `windows` on Windows and `alsa` on Linux, all MIT or Apache-2.0.
rubato brings `audioadapter`, `audioadapter-buffers`, `realfft` and `rustfft`
(MIT or Apache-2.0). The dialog plugin brings `tauri-plugin-fs` (Apache-2.0 OR
MIT) and `rfd` (MIT). Sixteen crates are MPL-2.0, all used unmodified: the
eleven Symphonia crates (`symphonia`, `-core`, `-common`, `-metadata`,
`-bundle-flac`, `-bundle-mp3`, `-codec-aac`, `-codec-alac`, `-codec-pcm`,
`-format-isomp4`, `-format-riff`) and five from Tauri (`cssparser`,
`cssparser-macros`, `dtoa-short`, `option-ext` and `selectors`). `r-efi`
offers LGPL-2.1-or-later as one of three choices; we use it as MIT.

## UI (TypeScript)

| Package | Version | Licence | Use |
| --- | --- | --- | --- |
| [react](https://github.com/facebook/react), react-dom | 19.3.1 | MIT | UI |
| [@tauri-apps/api](https://github.com/tauri-apps/tauri) | 2.12.4 | Apache-2.0 OR MIT | Calls from the UI into the Rust side |
| [@tauri-apps/plugin-dialog](https://github.com/tauri-apps/plugins-workspace) | 2.8.3 | MIT OR Apache-2.0 | The Analyzer's Open and Export folder dialogs |

## Build tools (not shipped)

| Package | Version | Licence |
| --- | --- | --- |
| [@tauri-apps/cli](https://github.com/tauri-apps/tauri) | 2.12.4 | Apache-2.0 OR MIT |
| [vite](https://github.com/vitejs/vite) | 8.3.5 | MIT |
| @vitejs/plugin-react | 6.1.2 | MIT |
| [typescript](https://github.com/microsoft/typescript-go) | 7.0.2 | Apache-2.0 |
| @types/react, @types/react-dom | 19.3.0 | MIT |

82 npm packages in all; 77 are build-time only. Twelve of them are
`lightningcss` packages (MPL-2.0, unmodified), which Vite uses to build the CSS
and which never reach the app.

## Stem worker (Python, on Nate's Mac only, not shipped)

`stems/` is the Analyzer's worker process for Phase 1. It runs from this
checkout's virtualenv (`uv sync`) and is not part of the app
bundle. Before tuneKore ships, the separator it ships with replaces it (see
`docs/PLAN.md` section 6), so these are recorded for the record and for that
decision, not as shipped code. Versions are pinned in `stems/pyproject.toml`
and locked in `stems/uv.lock` (Python 3.13).

| Package | Version | Licence | Use |
| --- | --- | --- | --- |
| [torch](https://github.com/pytorch/pytorch) | 2.14.1 | Apache-2.0 AND BSD (per its metadata) | Runs Demucs on Metal (MPS) |
| [numpy](https://numpy.org) | 2.5.4 | BSD-3-Clause | Audio buffers |
| [demucs](https://github.com/facebookresearch/demucs) | 4.1.0 | MIT (code) | The Demucs separator |
| [julius](https://github.com/adefossez/julius) | 0.2.8 | MIT | Resampling in the worker |
| [einops](https://github.com/arogozhnikov/einops) | 0.8.2 | MIT | Used by Demucs |
| [huggingface-hub](https://github.com/huggingface/huggingface_hub) | 2.3.0 | Apache-2.0 | Downloads Demucs's weights to this Mac's cache |
| [safetensors](https://github.com/huggingface/safetensors) | 0.8.0 | Apache-2.0 | Demucs weights format |
| [pyyaml](https://github.com/yaml/pyyaml) | 6.0.3 | MIT | Demucs model configs |
| [tqdm](https://github.com/tqdm/tqdm) | 4.70.1 | MPL-2.0 AND MIT (unmodified) | Imported by Demucs |
| [scipy](https://scipy.org) | 1.18.1 | BSD-3-Clause | Dev only: the test-song generator's filters |
| [pytest](https://pytest.org) | 9.1.1 | MIT | Dev only: tests |

32 packages in the environment in all; the rest are their dependencies, all
MIT, BSD, Apache-2.0 or PSF-2.0, plus tqdm (MPL-2.0 AND MIT, unmodified).

**Left out on purpose.** `stems/pyproject.toml` overrides Demucs's declared
dependencies so `lameenc` (LAME, LGPL; Demucs uses it only to write MP3) and
its training tools are never installed. The worker reads and writes raw
float32, so it needs no audio codec library at all.

SAM Audio (Meta) was tried in Phase 1 and removed on 2026-10-24; see
`docs/PLAN.md` section 6. Its packages are gone from the environment.

**Model weights**, downloaded to this Mac's Hugging Face cache
(`~/.cache/huggingface`), never committed, bundled or shipped:

| Weights | Licence | Status |
| --- | --- | --- |
| Demucs `htdemucs`, `htdemucs_ft` (Hugging Face `adefossez/HTDemucs`, `adefossez/HTDemucs-ft`) | The author: "provided only for scientific purposes" | Prototyping on Nate's Mac only. Can't ship. |

## Code adapted from other projects

Code we adapted rather than depend on. Each file keeps a header naming its
source and licence.

| Project | Licence | Where | What |
| --- | --- | --- | --- |
| [soundCraft](https://github.com/storytold/soundcraft) (ArtCraft Team and the SoundCraft contributors) | MIT OR Apache-2.0 | `engine/src/limiter.rs` | The master limiter, from soundCraft's `Maximizer` (`crates/dsp/src/plugins/dynamics.rs`, commit `7dc56a4`) |

## System libraries, not bundled

The app links the operating system's own libraries. They are not part of what
we ship.

| OS | Libraries | Licence note |
| --- | --- | --- |
| macOS | CoreAudio, CoreFoundation, WKWebView | Apple system frameworks |
| Windows | WASAPI, Media Foundation, WebView2 | Microsoft system components |
| Linux | ALSA (`libasound`, through cpal), WebKitGTK, GTK | LGPL. Linked dynamically from the system, never bundled. Phase 0 only compiles on Linux; settle how to ship before Phase 9. |

## Our own

The tuneKore name, logo and app icon (`brand/`) and all source in this repo
belong to devKore.
