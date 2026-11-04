# Code from soundCraft

Source: https://github.com/storytold/soundcraft, commit `7dc56a4`
(`ui: Tracks and Groups and Clips in full-height dock columns (#1088)`),
crates `crates/vst3-host` and (since #39) `crates/clap-host`. Licence: MIT OR
Apache-2.0, copyright 2026 ArtCraft Team and the SoundCraft contributors. `LICENSE-MIT`, `LICENSE-APACHE` and
`NOTICE` sit next to this file exactly as upstream has them. The ArtCraft name
and logos are not covered by that licence and we use neither.

The first commit of `plugins/` added the crate here unmodified. The next moved
`ffi.rs`, `scan.rs` and the test fixture into place as renames, so
`git log --follow` shows upstream's text and then ours; `ffi.rs` was then split
into the modules below (`git log -- plugins/src/vst3/ffi.rs` shows it up to the
split). Each file derived from soundCraft says so at the top.

| Upstream file | Here | What we changed |
| --- | --- | --- |
| `src/ffi.rs` | `src/vst3/module.rs`, `host.rs`, `events.rs`, `instance.rs`, `editor.rs`, `util.rs`, and `src/editor_window.rs` | Split into modules. Every call into a plugin goes through our C++ exception shim. `IPluginFactory3::setHostContext`. The instance is split into a main-thread controller and an audio-thread processor behind our `Plugin` traits. `process` takes several buses, events, ramped parameter changes and a full transport, and neither allocates nor locks (upstream's event list used a mutex). Editor edits reach the processor through a lock-free queue. A plugin that throws is switched off and leaked, not called again. Our own state container (`TKV3`). The editor attaches to a view the caller owns. |
| `src/scan.rs` | `src/scan/bundles.rs` | Categories are instrument, effect or other. |
| `tests/fixture/` | `tests/fixture/` | Added "Fixture Thru", an allocation-free pass-through class. |
| `tests/common/mod.rs` | `tests/common/mod.rs` | Rewritten around our API; adds the misbehaving C++ test plugin. |

Not taken (deleted from here): `src/lib.rs`, `src/plugin.rs` and
`src/registry.rs` (soundCraft's adapter to its own `Plugin` trait and its
in-memory registry; ours are `src/plugin.rs` and the scanner's JSON cache), and
upstream's integration tests (the module-exit tests were not ported; the
fixture still checks the module lifecycle in every test process). soundCraft's
`dsp` crate, whose `Plugin` trait we read, was not copied.

## `clap-host` (#39)

The CLAP host started from soundCraft's `crates/clap-host` at the same commit,
read from upstream rather than copied in first: its design (one instance used
from one thread, no exception guard, a global host state) didn't fit ours, so
the files here were written new around the pieces taken. Each one derived from
it says so at the top.

| Upstream file | Here | What we took, and what changed |
| --- | --- | --- |
| `src/ffi.rs` (`Bundle`) | `src/clap/module.rs` | Loading the binary, reading `clap_entry`, the version check, `init` with the bundle path, the factory and its descriptors. Every call goes through the C++ shim; modules are never unloaded, and `deinit` runs once at quit for modules with no live or leaked plugin. |
| `src/ffi.rs` (`HostState`, `host_*`) | `src/clap/host.rs` | The `clap_host` and the log, params, state, latency and GUI host extensions. One host per instance; added thread-check, timer support, tail, audio-ports and note-ports; log lines from the audio thread are counted, not written. |
| `src/ffi.rs` (`RawEvent`, `transport_event`, the list and stream callbacks) | `src/clap/events.rs`, `src/clap/instance.rs` | Preallocated input and output lists (parameters sorted in with notes, 32 points per parameter, note-off room, drops counted), more MIDI, the loop in the transport, state streams with a size cap. |
| `src/ffi.rs` (`Instance`) | `src/clap/instance.rs` | Split into a main-thread controller and an audio-thread processor behind our `Plugin` traits, with the VST3 host's handshake, failure handling and leak rules; every port gets buffers (silent ones for ports that are off); `start_processing` on the audio thread. |
| `src/ffi.rs` (`GuiLink::open_embedded`) | `src/clap/editor.rs` | The embedded editor sequence, attached to a view the caller owns; resize requests reach the caller from `poll`. |
| `src/scan.rs` | `src/scan/bundles.rs` | The CLAP folders (also in CLAP's own `entry.h`). |

Not taken: `src/lib.rs`, `src/plugin.rs`, `src/registry.rs` (its adapter and
registry), its Windows host window, and its test plugin and tests: ours
(`tests/clap_fixture/`, `tests/bad_clap/`) are new.
