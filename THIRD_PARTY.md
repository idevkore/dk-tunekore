# Third-party software

Every dependency musiKore builds on, with its licence. The rule (`CLAUDE.md`):
MIT, BSD, Apache-2.0, zlib, ISC or public domain. MPL-2.0 only for unmodified
files. Nothing GPL, AGPL or LGPL goes into the app.

`npm run licenses` reads `Cargo.lock` and `package-lock.json` and fails CI when
any package has no option on that list. It takes the permissive branch of a
dual licence (for example `MIT OR Apache-2.0 OR LGPL-2.1-or-later` is used as
MIT). Update this file in the same PR that adds a dependency.

## Engine (C++)

| Dependency | Version | Licence | Use |
| --- | --- | --- | --- |
| [RtAudio](https://github.com/thestk/rtaudio) | 6.0.1 | MIT-style (its `LICENSE`, with a non-binding request to send modifications upstream) | Audio device I/O: CoreAudio on macOS, WASAPI on Windows, ALSA on Linux. Fetched by `engine/CMakeLists.txt`, pinned by URL and SHA256, linked statically. |

## App shell (Rust), direct dependencies

| Crate | Version | Licence | Use |
| --- | --- | --- | --- |
| [tauri](https://github.com/tauri-apps/tauri) | 2.12.5 | Apache-2.0 OR MIT | App shell: window, menus, IPC |
| tauri-build | 2.7.4 | Apache-2.0 OR MIT | Build step for Tauri |
| [cmake](https://github.com/rust-lang/cmake-rs) | 0.1.58 | MIT OR Apache-2.0 | Builds the C++ engine from `build.rs` |
| [serde](https://github.com/serde-rs/serde) | 1.0.229 | MIT OR Apache-2.0 | Data conversion |
| [serde_json](https://github.com/serde-rs/json) | 1.0.154 | MIT OR Apache-2.0 | Reads the engine's device list |

The lockfile has 407 crates in all, counting every platform's. Their licences:
MIT or Apache-2.0 (or both) for nearly all, plus Unicode-3.0, Zlib, BSD-3-Clause,
ISC, Unlicense, CC0-1.0 and 0BSD. Five are MPL-2.0 (`cssparser`,
`cssparser-macros`, `dtoa-short`, `option-ext`, `selectors`), used unmodified.
`r-efi` offers LGPL-2.1-or-later as one of three choices; we use it as MIT.

## UI (TypeScript)

| Package | Version | Licence | Use |
| --- | --- | --- | --- |
| [react](https://github.com/facebook/react), react-dom | 19.3.1 | MIT | UI |
| [@tauri-apps/api](https://github.com/tauri-apps/tauri) | 2.12.4 | Apache-2.0 OR MIT | Calls from the UI into the Rust side |

## Build tools (not shipped)

| Package | Version | Licence |
| --- | --- | --- |
| [@tauri-apps/cli](https://github.com/tauri-apps/tauri) | 2.12.4 | Apache-2.0 OR MIT |
| [vite](https://github.com/vitejs/vite) | 8.3.5 | MIT |
| @vitejs/plugin-react | 6.1.2 | MIT |
| [typescript](https://github.com/microsoft/typescript-go) | 7.0.2 | Apache-2.0 |
| @types/react, @types/react-dom | 19.3.0 | MIT |

81 npm packages in all; 77 are build-time only. Twelve of them are
`lightningcss` packages (MPL-2.0, unmodified), which Vite uses to build the CSS
and which never reach the app.

## System libraries, not bundled

The app links the operating system's own libraries. They are not part of what
we ship.

| OS | Libraries | Licence note |
| --- | --- | --- |
| macOS | CoreAudio, CoreFoundation, WKWebView | Apple system frameworks |
| Windows | WASAPI, Media Foundation, WebView2 | Microsoft system components |
| Linux | ALSA (`libasound`), WebKitGTK, GTK | LGPL. Linked dynamically from the system, never bundled. Phase 0 only compiles on Linux; settle how to ship before Phase 9. |

## Our own

The musiKore name, logo and app icon (`brand/`) and all source in this repo
belong to devKore.
