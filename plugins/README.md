# tunekore-plugins

tuneKore's plugin host: it scans for plugins, loads them, runs them, saves and
restores their state, and opens their editors. VST3 (#35) and CLAP (#39),
behind the same traits: the engine and the app never see the format.

It is part of the root Cargo workspace (since #37). The engine runs a
plugin's audio half as a track device (`engine/src/plugin_device.rs`), and the
app keeps its controller, editor window and state (`src-tauri/src/plugins/`).

## Use

```rust
use tunekore_plugins::{scan, ProcessSetup};

// Find plugins in a helper process (cached; `scan::rescan` forces a fresh probe).
let cache = scan::scan(&scan::ScanOptions::default())?;
let serum = cache.find("Serum 2").unwrap();

// Load one, with the host for its format (`vst3::load` and `clap::load` are
// the two behind it). The controller stays on the main thread...
let mut controller = tunekore_plugins::load(&serum.info)?;
// ...and activation hands back the half that goes to the audio thread.
let mut plugin = controller.activate(&ProcessSetup::default())?;
// plugin.process(&mut block) on the audio thread; controller.save_state(),
// set_param(), open_editor(), poll() on the main thread.

// When the app quits, after dropping every plugin (runs VST3's and CLAP's
// module exits):
tunekore_plugins::shutdown();
```

## Layout

| Path | What |
| --- | --- |
| `src/plugin.rs` | The format-free interface: `PluginController` (main thread), `Plugin` (audio thread), events, transport, buses, parameters, errors |
| `src/vst3/` | The VST3 host: module loading, host objects, event and parameter lists, the instance and its two halves, editors |
| `src/clap/` | The CLAP host: module loading, the `clap_host` and its extensions, event lists and the transport, the instance and its two halves, editors |
| `shim/tk_shim.cpp`, `src/vst3/shim.rs`, `src/clap/shim.rs` | The C++ shim every call into a plugin goes through, and its typed wrappers for each format |
| `src/rt.rs` | What both hosts share: the audio thread's failure message, the OS thread id |
| `src/scan/` | Bundle discovery, the probe, the scanner's orchestration and the JSON cache |
| `src/bin/tunekore-scan.rs` | The scanner helper program |
| `src/offline.rs`, `src/wav.rs` | Offline rendering and a small WAV writer, for tests, examples and later bounces |
| `src/editor_window.rs` | A small floating macOS window for an editor, for the examples and tests (the app has its own, `src-tauri/src/plugins/window.rs`) |
| `examples/` | `render` (play notes through a plugin to a WAV) and `editor` (open a plugin's editor) |
| `tests/` | Tests against the fixture plugins (`fixture/` for VST3, `clap_fixture/` for CLAP), misbehaving C++ plugins (`bad_plugin/`, `bad_clap/`), the allocation counter, and the installed plugins |
| `third_party/soundcraft/` | Licence, NOTICE and a record of the soundCraft code this started from |
| `third_party/steinberg/` | Steinberg's MIT notice for the VST 3 API the `vst3` crate's bindings come from |

## How it works

**Two halves.** A loaded plugin is a `PluginController` and, once activated, a
`Plugin`. That is VST3's and CLAP's own threading split, and it means the
engine never locks on the audio thread: the controller handles parameters,
state and the editor on the main thread while the `Plugin` processes.
Activating again (new sample rate, block size or buses) makes the old `Plugin`
stale; it returns `ProcessError::Stale`. Deactivating, reactivating and
dropping the controller wait for a block in progress. A block that runs for
more than 10 s is taken as hung: the plugin is switched off and leaked, never
stopped or terminated under a running `process`. The plugin's objects are
released when the controller is dropped, on its thread, so a plugin's
destructors never run on the audio thread, even if the `Plugin` is dropped
last. Activation picks the buses (`BusSelection::Main`, `All`, or `Only`
chosen indices) and records the latency the `Plugin` reports.

**Parameter values from outside the processor** (the UI's `set_param`, the
plugin's own editor) go into a mailbox with one slot per parameter: the
latest value waits, across deactivation, and the next block delivers it. A
zero-frame block is enough, as VST3 hosts flush parameters while stopped.
When a block has no parameter queue left, the rest wait for the next one.
`has_pending_params` says whether any are waiting (the state is what the
processor has seen); `load_state` discards them, so a knob moved before a
preset loads cannot play over it. Editor edits, gestures and restart requests reach the
main thread through a lock-free queue read by `poll`, so a plugin may report
them from any thread, the audio thread included.

**`process` allocates nothing, locks nothing and logs nothing**, even when the
plugin throws. The caller's channels come in as `AudioIn`/`AudioOut`, built
each block from a preallocated `ChannelTable` (or raw pointers, in place if it
likes). Bus pointer tables, event lists (1,024 events, the last 128 places kept
for note-offs so a full block cannot leave notes stuck), parameter queues (256
parameters × 32 points in, 64 out; points kept in time order, and a full queue
keeps the last value) and the process context are allocated at activation.
What had to be dropped is counted (`Plugin::take_dropped`). Notes become VST3
events; control change, pitch bend and channel pressure go through the
plugin's `IMidiMapping` as parameter changes (re-read when the plugin says the
mapping changed). The transport (tempo, time signature, position in samples
and quarter notes, bar start, playing, recording, loop) goes to the plugin
every block; `Transport`'s documentation maps it onto the engine's
`ProcessContext`. `tests/no_alloc.rs` counts every allocation from building the
block to the plugin returning, including when it throws, and expects none.

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
On the audio thread the message goes into a fixed buffer and the failure is
reported later: `poll` gives `ControllerEvent::Failed` once, `take_error` says
why. One that throws while answering a question (a parameter's text) only
records the error. A crash (a segfault, an abort) still takes the process down; the
scanner is what protects the app from those.

**The scanner.** `tunekore-scan` finds `.vst3` bundles in
`/Library/Audio/Plug-Ins/VST3`, `~/Library/Audio/Plug-Ins/VST3` and
`VST3_PATH`, and `.clap` plugins in `/Library/Audio/Plug-Ins/CLAP`,
`~/Library/Audio/Plug-Ins/CLAP` and `CLAP_PATH` (on Windows and Linux, the
folders CLAP's `entry.h` names). A plugin's format follows from its file's
extension. It then starts one copy of itself per bundle (`--probe`), four at
a time, each in its own process group, and kills a probe after the timeout
(60 s by default). A probe loads each plugin in the bundle and writes what it
found to a file. A probe that crashed, aborted, hung or threw becomes an entry
with the reason, and the scan goes on. The cache (`plugin-cache.json` in
`~/Library/Application Support/tuneKore/` by default) lists name, vendor,
version, category, class id, buses, parameter count, load time, path, status
(`ok`, `failed`, `crashed`, `timed_out`), error and a stamp of the binary.
`ScanCache::plugins()` gives the ones that loaded and `failures()` the
blocklist. Unchanged bundles are not probed again, failed ones included,
except a plugin that timed out, which is tried again for 3 scans in a row
before it stays on the blocklist; `scan::retry_failed` gives the failures
another try and `scan::rescan` probes everything. The scan has an overall
time limit (`--deadline`), after which it stops the probes still running and
writes what it found (every bundle found gets an entry: one the limit kept from
its turn is listed as timed out and probed next time); it rewrites the cache after every bundle and lists its
running probes, stamped with its own process id, so the app can kill them if
it has to kill the scanner.

**Parameters** (`PluginController::params`) carry id, name, short name,
units, range (`min`, `max` in the plugin's own units), default, step count, and
whether each is automatable, hidden, read-only or the bypass. Values at the
interface are normalized; `param_text` gives the plugin's own text. Plugins
without a mapping (many JUCE ones) report a 0 to 1 range.

**State** is the component's state and the controller's state in one blob
(`TKV3`, two length-prefixed parts). A bare component state also loads.

**Editors** attach to an `NSView` the caller owns (`open_editor`), on the main
thread. The plugin's resize requests come to a callback.

**CLAP** (`src/clap/`) keeps all of the above: the two halves and their
handshake, the shim around every call, the mailbox, no allocation, lock or log
in `process`, leaking a plugin that throws or hangs, destroying it on the
controller's thread. What is CLAP's own:

- *Parameters* are in the plugin's own units at CLAP's side and normalized at
  ours; the controller and the processor convert with the ranges the plugin
  lists (stepped ones land on a step). CLAP gives no units; the text has them.
- *Events.* Notes go as CLAP note events when the plugin's note port takes
  them, else as MIDI; control changes, pitch bend and channel pressure go as
  MIDI, poly pressure as a pressure expression (or MIDI). Parameter changes
  are sorted in with the notes, parameters first at any one time; a block
  takes 256 parameters, 32 points each (more replace the last, so the block
  ends on the latest value), and 1,024 notes (the last 128 places for
  note-offs). The plugin's output gives notes, MIDI, its own parameter
  changes (`ProcessBlock::param_out`) and the edits and gestures from its
  window, which `poll` also reports.
- *Ports.* Every port gets buffers, as CLAP asks: a port that is off gets
  silence of our own, marked constant, and its outputs go to scratch; the
  plugin is told through `audio-ports-activation` when it has it. An input
  that is also an output (in place) is copied first, since CLAP processes in
  place only on paired ports. A channel the plugin marks constant is filled
  from its first sample. Channel counts are the plugin's own (no
  `audio-ports-config` yet).
- *Threads.* `activate`, `deactivate` and every main-thread call run on the
  controller's thread; `start_processing` and `process` on the audio thread.
  `stop_processing` belongs to the audio thread too, but stopping runs on the
  controller's thread once no block can run, and the thread check says so for
  that call. The host's thread check calls the thread that made the instance
  the main thread and the one in `process` the audio thread.
- *The main thread's errands.* The plugin's `request_callback`, its timers
  (`timer-support`, at least 10 ms apart), its parameter flush requests and
  its editor's resize requests are served by `poll`; the app polls every
  100 ms. Values set while the plugin is off go to it at once through
  `params.flush` on the main thread; while it is on, the next block (a
  zero-frame one flushes) delivers them.
- *State* is the plugin's own `clap.state`, with no container around it. A
  plugin without the extension saves nothing and loads only nothing.
- *Editors* use `clap.gui`, embedded (cocoa on macOS) in the view the caller
  owns, as VST3's: `is_api_supported`, `create`, `set_parent`, `show`; `hide`
  and `destroy` to close.
- *Restarts.* `request_restart`, port rescans and a full parameter rescan
  become `ReloadRequested`; value, text and info rescans and `mark_dirty`
  become `ParamsChanged`; `latency.changed` outside activation becomes
  `LatencyChanged`.

**Quitting.** Call `tunekore_plugins::shutdown()` after dropping every plugin.
It runs each VST3 module's `bundleExit` while the process is still whole (left
to the process exit handler, Serum 2 crashes there; see `vst3::shutdown`), then
each CLAP module's `clap_entry.deinit`.

## Run

```bash
cargo test -p tunekore-plugins --locked   # tests that need installed plugins skip when CI is set
cargo clippy -p tunekore-plugins --all-targets
cargo run --release -p tunekore-plugins --bin tunekore-scan -- --cache /tmp/plugins.json
cargo run --release -p tunekore-plugins --example render -- /Library/Audio/Plug-Ins/VST3/Serum2.vst3 --out /tmp/serum2.wav --state-check
cargo run --release -p tunekore-plugins --example render -- "/Library/Audio/Plug-Ins/VST3/Guitar Rig 7.vst3" --input sine
cargo run --release -p tunekore-plugins --example render -- "/Library/Audio/Plug-Ins/VST3/Kontakt 7.vst3" --all-buses
cargo run --release -p tunekore-plugins --example editor -- /Library/Audio/Plug-Ins/VST3/Serum2.vst3 --seconds 10
cargo run --release -p tunekore-plugins --example render -- /Library/Audio/Plug-Ins/CLAP/<plugin>.clap --state-check
```

The tests build four test plugins: soundCraft's VST3 fixture (Rust,
`tests/fixture`, its own workspace), our CLAP fixture (Rust,
`tests/clap_fixture`, its own workspace), and misbehaving C++ plugins for each
format (`tests/bad_plugin`, `tests/bad_clap`, built with the system `c++`;
their tests skip if there is none).

## Not done yet

- Windows and Linux editors, Linux `IRunLoop`; for CLAP, `posix-fd-support`
  (Linux editors need it).
- CLAP: `audio-ports-config` (channel counts other than the plugin's own),
  parameter modulation and per-note automation, note expressions other than
  pressure, `thread-pool`, `preset-load`, `state-context`, `remote-controls`.
  Timers run at the app's poll rate (100 ms), not from the main run loop.
- No CLAP plugin has been tried yet: none is installed on the test Mac; the
  fixture is all the CLAP host has run.
- Sidechain inputs are activated on request (`BusSelection::All`) but not tried
  with a real sidechain signal; Solid Bus Comp only loaded and processed.
- 64-bit processing, `IUnitInfo` program lists, `IComponentHandler2`, note
  expression, plugin MIDI output other than notes.
- Running a plugin out of process. Today a plugin that crashes after it has
  loaded takes the app with it.
