# Third-party software

Every dependency tuneKore builds on, with its licence. The rule (`CLAUDE.md`):
MIT, BSD, Apache-2.0, zlib, ISC or public domain. MPL-2.0 only for unmodified
files. Nothing GPL, AGPL or LGPL goes into the app.

`npm run licenses` reads the root `Cargo.lock` and `package-lock.json` and fails CI when
any package has no option on that list. It takes the permissive branch of a
dual licence (for example `MIT OR Apache-2.0 OR LGPL-2.1-or-later` is used as
MIT). Update this file in the same PR that adds a dependency.

## Engine and app shell (Rust), direct dependencies

One Cargo workspace: `engine/` (`tunekore-engine`) and `src-tauri/` (`tunekore`).

| Crate | Version | Licence | Use |
| --- | --- | --- | --- |
| [cpal](https://github.com/RustAudio/cpal) | 0.18.2 | Apache-2.0 | Engine: audio device I/O through CoreAudio on macOS, WASAPI on Windows and ALSA on Linux |
| [serde](https://github.com/serde-rs/serde) | 1.0.229 | MIT OR Apache-2.0 | Engine and shell: the device list sent to the UI |
| [tauri](https://github.com/tauri-apps/tauri) | 2.12.5 | Apache-2.0 OR MIT | Shell: window, menus, IPC |
| tauri-build | 2.7.4 | Apache-2.0 OR MIT | Shell: build step |

The lockfile has 420 crates in all, counting every platform's. Nearly all are
MIT or Apache-2.0 (or both). The rest are Unicode-3.0, Zlib, BSD-3-Clause, ISC,
Unlicense, CC0-1.0 and 0BSD. cpal brings `coreaudio-rs` and `objc2-core-audio`
on macOS, `windows` on Windows and `alsa` on Linux, all MIT or Apache-2.0.
Five crates are MPL-2.0, all used unmodified and all from Tauri: `cssparser`,
`cssparser-macros`, `dtoa-short`, `option-ext` and `selectors`. `r-efi` offers
LGPL-2.1-or-later as one of three choices; we use it as MIT.

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
| Linux | ALSA (`libasound`, through cpal), WebKitGTK, GTK | LGPL. Linked dynamically from the system, never bundled. Phase 0 only compiles on Linux; settle how to ship before Phase 9. |

## Our own

The tuneKore name, logo and app icon (`brand/`) and all source in this repo
belong to devKore.
