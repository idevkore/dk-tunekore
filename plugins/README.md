# tunekore-plugins

tuneKore's plugin host: it scans for plugins, loads them, runs them, saves and
restores their state, and opens their editors. VST3 today; CLAP (#39) goes
behind the same traits. Issue: idevkore/dk-tunekore#35. The engine and app use
it from the plugins-in-the-app work (#37).

This crate is its own Cargo workspace until #37 joins it to the root one, so
CI does not build it yet.

## Use

```rust
use tunekore_plugins::{scan, vst3, ProcessSetup};

// Find plugins in a helper process (cached; `scan::rescan` forces a fresh probe).
let cache = scan::scan(&scan::ScanOptions::default())?;
let serum = cache.find("Serum 2").unwrap();

// Load one. The controller stays on the main thread...
let mut controller = vst3::load(&serum.info)?;
// ...and activation hands back the half that goes to the audio thread.
let mut plugin = controller.activate(&ProcessSetup::default())?;
// plugin.process(&mut block) on the audio thread; controller.save_state(),
// set_param(), open_editor() on the main thread.

// When the app quits, after dropping every plugin:
vst3::shutdown();
```

## Layout

| Path | What |
| --- | --- |
| `src/plugin.rs` | The format-free interface: `PluginController` (main thread), `Plugin` (audio thread), events, transport, buses, parameters, errors |
| `src/vst3/` | The VST3 host: module loading, host objects, event and parameter lists, the instance and its two halves, editors |
| `shim/tk_shim.cpp`, `src/vst3/shim.rs` | The C++ shim every call into a plugin goes through |
| `src/scan/` | Bundle discovery, the probe, the scanner's orchestration and the JSON cache |
| `src/bin/tunekore-scan.rs` | The scanner helper program |
| `src/offline.rs`, `src/wav.rs` | Offline rendering and a small WAV writer, for tests, examples and later bounces |
| `src/editor_window.rs` | A small floating macOS window for an editor (the app's own windows come in #37) |
| `examples/` | `render` (play notes through a plugin to a WAV) and `editor` (open a plugin's editor) |
| `tests/` | Tests against the fixture plugin, a misbehaving C++ plugin, the allocation counter, and the installed plugins |
| `third_party/soundcraft/` | Licence, NOTICE and a record of the soundCraft code this started from |
| `third_party/steinberg/` | Steinberg's MIT notice for the VST 3 API the `vst3` crate's bindings come from |

## How it works

**Two halves.** A loaded plugin is a `PluginController` and, once activated, a
`Plugin`. That is VST3's and CLAP's own threading split, and it means the
engine never locks on the audio thread: the controller handles parameters,
state and the editor on the main thread while the `Plugin` processes. Edits
made in the plugin's editor reach its processor through a lock-free queue and
are also reported by `poll`, for the UI and automation recording. Activating
again (new sample rate or block size) makes the old `Plugin` stale; it returns
`ProcessError::Stale`. Drop the `Plugin` first (off the audio thread), then the
controller.

**`process` allocates nothing, locks nothing and logs nothing.** Bus pointer
tables, event lists (1,024 events), parameter queues (256 parameters × 32
points in, 64 out) and the process context are allocated at activation. Notes
become VST3 events; control change, pitch bend and channel pressure go through
the plugin's `IMidiMapping` as parameter changes. The transport (tempo, time
signature, position in samples and beats, bar start, playing, recording, loop)
goes to the plugin every block. `tests/no_alloc.rs` counts allocations inside
`process` with a global allocator and expects none.

**The exception shim.** Rust cannot catch a C++ exception; one that reaches a
Rust frame aborts the process (Analog Lab V did this in the hosting spike). So
no Rust code calls a plugin directly. Each call takes the function pointer from
the plugin object's vtable, with its exact Rust type, and hands it to a C++
function in `shim/tk_shim.cpp` that calls it inside `try`/`catch` and returns
the exception's type and message as an error. That covers every interface
method we use, `addRef`/`release`/`queryInterface`, and the module's
`bundleEntry`, `GetPluginFactory` and `bundleExit`. The success path costs one
indirect call. A plugin that throws while setting up or processing is switched
off: it is never called again, and its objects are leaked rather than released.
One that throws while answering a question (a parameter's text) only records
the error. A crash (a segfault, an abort) still takes the process down; the
scanner is what protects the app from those.

**The scanner.** `tunekore-scan` finds `.vst3` bundles in
`/Library/Audio/Plug-Ins/VST3`, `~/Library/Audio/Plug-Ins/VST3` and
`VST3_PATH`. It then starts one copy of itself per bundle (`--probe`), four at
a time, each in its own process group, and kills a probe after the timeout
(60 s by default). A probe loads each plugin in the bundle and writes what it
found to a file. A probe that crashed, aborted, hung or threw becomes an entry
with the reason, and the scan goes on. The cache (`plugin-cache.json` in
`~/Library/Application Support/tuneKore/` by default) lists name, vendor,
version, category, class id, buses, parameter count, load time, path, status
(`ok`, `failed`, `crashed`, `timed_out`), error and a stamp of the binary.
`ScanCache::plugins()` gives the ones that loaded and `failures()` the
blocklist. Unchanged bundles are not probed again, failed ones included;
`scan::retry_failed` gives the failures another try and `scan::rescan` probes
everything.

**Parameters** (`PluginController::params`) carry id, name, short name,
units, range (`min`, `max` in the plugin's own units), default, step count, and
whether each is automatable, hidden, read-only or the bypass. Values at the
interface are normalized; `param_text` gives the plugin's own text. Plugins
without a mapping (many JUCE ones) report a 0 to 1 range.

**State** is the component's state and the controller's state in one blob
(`TKV3`, two length-prefixed parts). A bare component state also loads.

**Editors** attach to an `NSView` the caller owns (`open_editor`), on the main
thread. The plugin's resize requests come to a callback.

**Quitting.** Call `vst3::shutdown()` after dropping every plugin. It runs each
module's `bundleExit` while the process is still whole. Left to the process exit
handler, Serum 2 crashes there (see the function's notes).

## Run

```bash
cargo test --locked                  # tests that need installed plugins skip when CI is set
cargo clippy --all-targets
cargo run --release --bin tunekore-scan -- --cache /tmp/plugins.json
cargo run --release --example render -- /Library/Audio/Plug-Ins/VST3/Serum2.vst3 --out /tmp/serum2.wav --state-check
cargo run --release --example render -- "/Library/Audio/Plug-Ins/VST3/Guitar Rig 7.vst3" --input sine
cargo run --release --example render -- "/Library/Audio/Plug-Ins/VST3/Kontakt 7.vst3" --all-buses
cargo run --release --example editor -- /Library/Audio/Plug-Ins/VST3/Serum2.vst3 --seconds 10
```

The tests build two test plugins: soundCraft's fixture (Rust, `tests/fixture`,
its own workspace) and a misbehaving C++ plugin (`tests/bad_plugin`, built with
the system `c++`; its tests skip if there is none).

## Not done yet

- CLAP (#39), Windows and Linux editors, Linux `IRunLoop`.
- Sidechain inputs are activated on request (`BusSelection::All`) but not tried
  with a real sidechain signal; Solid Bus Comp only loaded and processed.
- 64-bit processing, `IUnitInfo` program lists, `IComponentHandler2`, note
  expression, plugin MIDI output other than notes.
- Running a plugin out of process. Today a plugin that crashes after it has
  loaded takes the app with it.
