# Rust plugin-hosting spike: findings

Tracker: idevkore/kore-issues#24. Run on 2026-10-24 on Nate's Mac (Apple
silicon, macOS 27, 48 kHz, 512-frame blocks). This is throwaway code on
`nate/chore/rust-hosting-spike`, not for merging.

**The question.** `docs/PLAN.md` picked C++ for the engine mainly because
hosting VST3 and AU plugins from Rust was "the least proven path". Can a
small Rust host load Serum 2 as a VST3 and as an AU, open its editor, and play
notes with tempo?

**The answer: yes, both formats, on the first try.** The one Rust-specific
problem it found has a known fix (below).

## What was built

`rust-host/` is a single binary, about 1,500 lines:

| File | Lines | What it does |
| --- | --- | --- |
| `vst3host.rs` | 750 | VST3 host on the raw `vst3` bindings. It loads the module (`bundleEntry`, `GetPluginFactory`) and implements the host side by hand: `IHostApplication`, `IMessage`, `IAttributeList`, `IBStream`, `IEventList`, an empty `IParameterChanges`, `IComponentHandler` and `IPlugFrame`. Then it sets up the component, controller and connection points, transfers state, sets bus arrangements, calls `process()` with a `ProcessContext`, and attaches the editor to an `NSView`. |
| `auhost.rs` | 270 | AUv2 host on `objc2-audio-toolbox`. It finds the unit from the bundle's `Info.plist`, sets the stream format, max frames and host callbacks (tempo, beat, transport), and calls `MusicDeviceMIDIEvent` and `AudioUnitRender`. The editor comes from `kAudioUnitProperty_CocoaUI`. |
| `transport.rs` | 77 | Test pattern: 120 BPM, 4/4, C4 E4 G4 C5, one note per beat, each half a beat long |
| `audio.rs`, `meter.rs` | 160 | Output through `cpal`, or offline rendering, plus a level and load meter |
| `main.rs` | 220 | AppKit window and event loop, command line |

```bash
cargo run --release -- vst3 /Library/Audio/Plug-Ins/VST3/Serum2.vst3 --seconds 8
cargo run --release -- au /Library/Audio/Plug-Ins/Components/Serum2.component --seconds 8
```

Add `--offline` to render without a device and `--no-editor` to skip the
window. With `MK_NO_NOTES=1` no notes are sent, which checks that the sound
comes from them.

## Results

| Plugin | Format | Loads | Editor | Notes play | Tempo read | State save + restore |
| --- | --- | --- | --- | --- | --- | --- |
| Serum 2 | VST3 | yes, 70–150 ms | yes, 1190×740 NSView, one resize request handled | yes, peak −10.9 dBFS; silent with no notes | sent every block (VST3 gives no way to see if it is read) | yes, 1,258 bytes |
| Serum 2 | AU | yes, ~100 ms | yes, 1190×740 Cocoa view | yes, identical levels to the VST3 | **yes**: 750 host-callback queries in 4 s | yes (`ClassInfo`) |
| Serum 1 | VST3 | yes | not tried | yes, peak −9.6 dBFS | sent | yes, 6,506 bytes |
| Serum 1 | AU | yes | not tried | yes, identical to the VST3 | yes, 750 queries | yes |
| Analog Lab V | VST3 | **no**: the process aborted on a C++ exception thrown into Rust | | | | |
| Analog Lab V | AU | no: `AudioComponentInstanceNew` −1 | | | | |
| Jup-8 V4 | VST3 | no: `initialize` returned false | | | | |
| Jup-8 V4 | AU | no: stream format −10877 | | | | |

- **Real-time playback.** 8 s through the speakers, with the editor open, in
  both formats. The worst device callback used 6–10% of its budget in a debug
  build, and the release build was about the same. The debug VST3 run had one
  underrun at start-up; the release run had none.
- **Arturia is not a Rust problem.** Apple's own `auval` fails both Arturia
  AUs the same way (`OpenAComponent: -1`, `Initialize: -1`), printing the same
  "Unload failed … launchctl bootout" line our host printed. Their launch
  agent or licence is not working on this Mac. Check that in Arturia Software
  Center before Phase 2.
- **The Rust-specific finding.** When Analog Lab V's VST3 threw a C++
  exception through `initialize`, Rust aborted the whole process ("Rust cannot
  catch foreign exceptions"). A C++ host can wrap plugin calls in
  `try { } catch (...)` and report the error instead. The fix is to put every
  call into a plugin behind a few lines of C++ that catch the exception, or to
  run untrusted plugins out of process. The plan already scans out of process.

## What it cost

- **The VST3 host-side helpers were the bulk of the work, and they were not
  hard.** About 400 lines replace what the SDK's C++ hosting helpers provide:
  host application, message, attribute list, memory stream, event list and
  parameter changes. Each is a small COM class, and `vst3`'s `Class` /
  `ComWrapper` make them easy to write.
- **The AU side is thin.** `objc2-audio-toolbox` covers AudioComponent,
  AudioUnit, MusicDevice and the property structs. One call needed raw
  `objc_msgSend`: `uiViewForAudioUnit:withSize:` has no binding, and
  `AudioUnit` has no `Encode` impl.
- **Nearly all of it is `unsafe`.** Rust's safety does not reach the plugin
  boundary. It does help everywhere else: `Send` forced the audio-thread
  renderer to be explicit, and the engine's own graph and document would be
  safe code.
- **Licences are all permissive.** The 101 crates are MIT, Apache-2.0, Zlib,
  ISC, BSD and Unlicense. There is no MPL or GPL, and no `vst3-sys` (GPLv3).

## Not tested yet

These are the next risks, in the order they would bite:

1. Parameter automation: a real `IParameterChanges` / `IParamValueQueue`, and
   AU parameter scheduling.
2. Kontakt 7: several output buses. Solid Bus Comp: sidechain input.
3. Editors: editor-driven resizing past the first request, content scaling,
   and keyboard focus. Serum's editor sent no `performEdit` because nobody
   clicked it.
4. AUv3 out of process (`requestViewController`).
5. Windows (HWND editors) and Linux, where X11 editors need an `IRunLoop` that
   we implement.
6. Plugins crashing or throwing: the exception shim or out-of-process hosting
   above.

None of these looks harder in Rust than in C++; each is the same API work in
either language.

## Recommendation

**Rust is viable for the engine.** The reason C++ won in `docs/PLAN.md`,
"hosting VST3 and AU editors from Rust is the least proven path", did not hold
up. Two formats of both Serum versions loaded, played with tempo, showed their
editors and restored their state, from about 1,000 lines of Rust in one
session.

If Nate agrees, the switch is cheapest now, before Phase 1:

- Replace the Phase 0 C++ engine (PR idevkore/dk-musikore#1) with an
  `engine` Rust crate. It would use `cpal` (Apache-2.0) in place of RtAudio,
  and there would be no CMake or `build.rs` bridge.
- Keep a small C++ file for the try/catch around plugin calls, or decide to
  host plugins out of process from the start.
- Update section 1 of `docs/PLAN.md` (engine, why not Rust, other options) and
  `THIRD_PARTY.md` in the same PR.
- The C++ libraries in the plan need Rust bindings or alternatives:
  Signalsmith Stretch (bindings), r8brain (`rubato`, MIT), dr_libs and
  stb_vorbis (`symphonia` is MPL-2.0, fine unmodified, or bindings).

Staying with C++ is still reasonable if a single toolchain matters less to us
than matching the reference code line for line. The spike only shows that
choosing Rust no longer adds hosting risk.
