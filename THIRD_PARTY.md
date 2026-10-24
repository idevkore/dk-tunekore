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
checkout's virtualenv (`uv sync --extra sam`) and is not part of the app
bundle. Before tuneKore ships, the separator it ships with replaces it (see
`docs/PLAN.md` section 6), so these are recorded for the record and for that
decision, not as shipped code. Versions are pinned in `stems/pyproject.toml`
and locked in `stems/uv.lock` (Python 3.13).

| Package | Version | Licence | Use |
| --- | --- | --- | --- |
| [torch](https://github.com/pytorch/pytorch) | 2.14.1 | Apache-2.0 AND BSD (per its metadata) | Runs both models, on Metal (MPS) |
| [numpy](https://numpy.org) | 2.5.4 | BSD-3-Clause | Audio buffers |
| [demucs](https://github.com/facebookresearch/demucs) | 4.1.0 | MIT (code) | The Demucs separator |
| [julius](https://github.com/adefossez/julius) | 0.2.8 | MIT | Resampling in the worker |
| [einops](https://github.com/arogozhnikov/einops) | 0.8.2 | MIT | Used by both models |
| [huggingface-hub](https://github.com/huggingface/huggingface_hub) | 2.3.0 | Apache-2.0 | Downloads weights to this Mac's cache; reads Nate's login |
| [safetensors](https://github.com/huggingface/safetensors) | 0.8.0 | Apache-2.0 | Demucs weights format |
| [pyyaml](https://github.com/yaml/pyyaml) | 6.0.3 | MIT | Demucs model configs |
| [tqdm](https://github.com/tqdm/tqdm) | 4.70.1 | MPL-2.0 AND MIT (unmodified) | Imported by Demucs |
| [sam-audio](https://github.com/facebookresearch/sam-audio) | git `bb4c699` | SAM License (Meta) | The SAM Audio separator |
| [dacvae](https://github.com/facebookresearch/dacvae) | git `414c207` | Apache-2.0 | SAM Audio's audio codec |
| [perception-models](https://github.com/facebookresearch/perception_models) | git `3e352cc` | Apache-2.0 per its README; its `setup.py` says "FAIR Noncommercial Research License". **Confirm with Meta before shipping SAM Audio.** | Encoder modules SAM Audio imports |
| [transformers](https://github.com/huggingface/transformers) | 5.20.2 | Apache-2.0 | SAM Audio's T5 text encoder |
| [torchvision](https://github.com/pytorch/vision) | 0.29.1 | BSD-3-Clause | Imported by SAM Audio |
| [torchdiffeq](https://github.com/rtqichen/torchdiffeq) | 0.2.5 | MIT | SAM Audio's ODE solver |
| [timm](https://github.com/huggingface/pytorch-image-models) | 1.0.30 | Apache-2.0 | Imported by perception-models |
| [scipy](https://scipy.org) | 1.18.1 | BSD-3-Clause | Pulled in by the above; the test-song generator's filters |
| [hf](https://github.com/huggingface/huggingface_hub) | 2.3.0 | Apache-2.0 | Dev only: `hf auth login` |
| [pytest](https://pytest.org) | 9.1.1 | MIT | Dev only: tests |

49 packages in the environment in all; the rest are their dependencies, all
MIT, BSD, Apache-2.0, ISC, PSF-2.0, MIT-CMU (pillow) or MPL-2.0 (tqdm).

**Left out on purpose.** `stems/pyproject.toml` overrides the declared
dependencies of demucs, sam-audio, dacvae and perception-models so these are
never installed: `lameenc` (LAME, LGPL), `descript-audiotools` (brings
`soundfile`, i.e. libsndfile, LGPL), `torchcodec` (FFmpeg), `torchaudio`,
`imagebind` (CC BY-NC-SA 4.0, a non-commercial model used only for video
re-ranking), `laion-clap`, `audiobox_aesthetics` and `xformers`.
`stems/tunekore_stems/shims.py` stands in for the four of them the code imports
without calling on our path; anything that really calls one fails loudly.

**Model weights**, downloaded to this Mac's Hugging Face cache
(`~/.cache/huggingface`), never committed, bundled or shipped:

| Weights | Licence | Status |
| --- | --- | --- |
| Demucs `htdemucs`, `htdemucs_ft` (Hugging Face `adefossez/HTDemucs`, `adefossez/HTDemucs-ft`) | The author: "provided only for scientific purposes" | Prototyping on Nate's Mac only. Can't ship. |
| SAM Audio `facebook/sam-audio-small`, `-base` (gated) | SAM License, 2025-11-19 | Shippable with the licence text, if Nate makes the exception `CLAUDE.md` needs |
| `t5-base` (SAM Audio's text encoder, from Hugging Face) | Apache-2.0 | Fine |

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
