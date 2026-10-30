# Code from soundCraft

Source: https://github.com/storytold/soundcraft, commit `7dc56a4`
(`ui: Tracks and Groups and Clips in full-height dock columns (#1088)`),
crate `crates/vst3-host`. Licence: MIT OR Apache-2.0, copyright 2026 ArtCraft
Team and the SoundCraft contributors. `LICENSE-MIT`, `LICENSE-APACHE` and
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
