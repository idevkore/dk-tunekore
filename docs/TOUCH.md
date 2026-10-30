# Touch on the Mac: discovery

Written 2026-10-29, the day after Apple announced the first touchscreen Mac.
The facts were checked on the web that day; sources are at the end. Items
marked *secondary* come from developer write-ups, not Apple.

## The short version

- **Touch Macs ship on 2026-11-06.** The MacBook Pro with PROLED XDR Display
  (14" and 16", M5 Pro or M5 Max, from $2,899) has a touch sensor built into
  the panel. Apple says macOS "automatically translates touches into clicks",
  and that updated gesture APIs let apps build for touch.
- **Apple is aiming at our MVP.** Logic Pro on the touch Mac gets a multi-touch
  keyboard and tappable chord and drum pads. Pads and steps are exactly what
  the Sampler does.
- **Most DAWs are unready.** On the Mac only Studio One does multi-touch today,
  and only through a third-party protocol (TUIO). Live, Cubase, Reaper and
  Reason treat touch as a mouse. Bitwig's touch work is Windows-only. A
  touch-first Sampler on the Mac is open ground.
- **Our stack mostly helps.** The UI is web content and already uses Pointer
  Events, the API browsers use for mouse, pen and touch alike. Taps and
  one-finger drags will work on day one, even under click emulation.
- **The open question is multi-touch in the webview.** Nobody, Apple included,
  has said whether WKWebView hands a page several touches at once with
  `pointerType: "touch"`. Apple's documented path for real touch is AppKit
  gesture recognizers, which web content can't see. If the webview only gets
  emulated clicks, finger drumming on two pads at once needs a small native
  layer in the Tauri shell (see "Two routes").

## What Apple shipped

| | Status |
| --- | --- |
| Hardware | Announced 2026-10-28, ships 2026-11-06. 14.2" and 16.3" tandem OLED, 10–120 Hz, new hinge "for stability while using Touch". No Pencil. Only these two models; the new M6 14" and the MacBook Neo have no touch. |
| Default behavior | Touches become clicks, so unmodified apps work. *Secondary:* touch-and-pan becomes scroll, pinch and rotate become magnify and rotate, hold-then-move becomes drag. |
| Touch-native apps | WWDC26 tech talk "Bring Touch to your Mac app": an `NSIsTouchNative` Info.plist key; touch-only gesture recognizers (`allowedTouchTypes = .direct`); 36-point minimum hit targets for touch; `exclusiveGestureBehavior = .notExclusive` for simultaneous controls (the talk's example is two sliders at once); `disablesDirectTouchSystemGestures` on a window. |
| Web content | Nothing from Apple or WebKit yet. Today WebKit on macOS reports `navigator.maxTouchPoints` as 0 and ignores `touch-action`. |
| Sidecar | macOS 27 with an iPad on iPadOS 27 takes direct finger taps and swipes on the iPad screen. A way to test before the hardware arrives, though not identical to a built-in panel. |

## How other DAWs do touch

- **Logic Pro for iPad**, the closest model. Pad velocity has three modes:
  Fixed, Dynamic (tap strength, iPad only) and Y-position (where on the pad).
  Double-tap resets a knob or fader; several fingers move several controls.
  Quick Sampler chops by tapping the waveform to add a marker and dragging its
  handle. Live Loops has a separate Cell Edit mode so editing never launches
  clips. Plug-in tiles show a simplified, reflowing view of each plug-in.
- **Ableton Note.** A Single layout turns one pad into a big velocity surface
  (higher is harder). Hold a pad and tap a division for note repeat. In the
  velocity lane one finger draws values and two fingers draw a ramp.
- **Bitwig (Windows).** A Tablet display profile with bigger headers, view tabs
  and one-tap shortcut buttons, and a press-and-hold radial menu that replaces
  right-click. Plug-ins get "Remote Controls": pages of 8 parameters.
- **Koala Sampler.** Users complain that playing a pad jumps the editor to it;
  they want play and select kept apart.
- **Ableton Live** is the warning: no multi-touch, and many controls act on
  release instead of touch-down, so clip launching feels late.

## Where tuneKore stands today

An audit of `src/` on 2026-10-29:

| Works for touch now | Blocks touch |
| --- | --- |
| Pads, trim handles and velocity cells use Pointer Events and set `touch-action: none` | The step editor (velocity, chance, ratchets) opens only on right-click (`Steps.tsx`, `onContextMenu`) |
| Steps paint by dragging; the velocity cells and inspector knobs capture the pointer | Resets and loading need double-click (`Inspector.tsx`, `Library.tsx`, `AudioDialog.tsx`, `Analyzer.tsx`) |
| Pads trigger on pointer-down, not release | Step cells, mute buttons and row labels are 26px tall, and trim handles are 10px wide, against 36pt for touch |
| | 48 `title=` tooltips and 18 `:hover` rules carry hints touch users never see |
| | Pads play at one fixed velocity; there is no Y-position or Single-pad mode |
| | Pressing a pad also selects it, so playing moves the Inspector (Koala's complaint) |
| | Pad hits go through React and a Tauri command before they reach the engine |

## Two routes for multi-touch

**A. Web only.** If WKWebView on a touch Mac delivers several pointers with
`pointerType: "touch"`, everything stays in React. Pads, knobs and steps
already work per pointer, so two fingers on two pads just work. This is the
cheapest route and also the one Windows (WebView2 delivers real touch pointers)
will take.

**B. Native touch layer.** If the webview only gets emulated clicks, the shell
adds an AppKit gesture recognizer over the webview (Rust through `objc2`, in
our own Tauri plugin), set to `allowedTouchTypes = .direct` and
`.notExclusive`. It forwards touches two ways:

- **Pads go straight to the engine.** The UI sends the shell the pads'
  rectangles whenever the layout changes; the native layer hit-tests each
  touch and pushes note-on with velocity onto the engine's lock-free queue.
  This skips React and the IPC hop, which helps latency either way.
- **Everything else goes to JS** as synthetic multi-pointer events, so knobs,
  pinch-zoom on waveforms and two-finger velocity ramps keep their web code.

Route B may be worth doing for pads even if A works, for latency. The
measurement below decides.

## Plugins

VST3 and CLAP editors (Serum, Arturia, NI) are mouse UIs: hover, right-click,
small knobs. Under click emulation they get one finger at a time. tuneKore
should draw its own touch page for every plugin:

- When a CLAP plugin offers remote-control pages (8 parameters each, a draft
  extension in the CLAP spec), use them.
- Otherwise build 8-parameter pages from the plugin's parameter list, saved
  per plugin, like Bitwig's Remote Controls.
- The vendor's own editor stays one tap away, for mouse work.

This fits Phase 3 (plugin hosting) and helps hardware controllers too.

## Latency

Players notice more than about 10 ms from hit to sound, and jitter (uneven
delay) bothers them as much as delay does. Touch panels are usually the slow
part: tens of milliseconds is common. Apple says its in-panel sensor "adds no
latency" but gives no number. We measure it ourselves on the new MacBook Pro:
a contact mic on the glass and the audio output into one stereo recording,
then the gap between the two.

## Design rules

1. Pads sound on touch-down, never on release.
2. Every control has a hit area of at least 36pt on the Mac (44pt elsewhere),
   even when it draws smaller. Pads are far bigger.
3. Nothing needs hover, right-click or double-click alone. Press-and-hold opens
   whatever right-click opens; double-tap resets as double-click does; every
   tooltip hint also lives somewhere visible.
4. Three pad velocity modes: Fixed, Y-position (with a range) and Single (one
   big pad). Tap strength isn't available on the Mac.
5. Playing never changes the selection. A select/edit toggle (like Logic's
   Cell Edit) picks a pad for the Inspector.
6. Knobs: vertical drag, double-tap reset, tap for a large value page with
   typed entry, and several knobs at once.
7. Precision without pixel-exact drags: snap on by default, nudge buttons by
   grid step, typed values for chop points and envelope times, pinch-zoom on
   waveforms.
8. Step sequencer: tap toggles, drag paints, drag in the velocity lane draws,
   two fingers draw a ramp. Hold a pad and tap a division for note repeat.
9. No three-finger gestures on playing surfaces; undo and redo are buttons and
   shortcuts.
10. Keyboard and trackpad stay first-class. A touch layout adds space and
    bigger controls; it never takes features away.

## Plan

| Step | What | When |
| --- | --- | --- |
| 1. Touch readiness | Fix the audit's blockers, which also help mouse users: press-and-hold for the step editor, double-tap resets, 36pt hit areas, visible hints, pad velocity modes, play/select split | Now; no touch hardware needed |
| 2. Touch probe | A dev-only page that logs `pointerType`, pointer IDs, simultaneous pointers, `maxTouchPoints` and gesture events in our real Tauri webview | Now |
| 3. Sidecar test | Run the probe on this Mac (macOS 27) with an iPad on iPadOS 27 | As soon as an iPad is at hand |
| 4. Hardware test | Run the probe on a touch MacBook Pro, measure touch-to-sound latency, and pick route A or B | From 2026-11-06 |
| 5. Touch layout | A layout for touch: the Sampler first (pads, steps, chop), then the Library | After step 4 |
| 6. Plugin touch pages | Remote-control pages per plugin | With Phase 3 |

**Out of scope for now: iPad.** Tauri 2 builds for iOS, so the same UI could
run on an iPad, but iPadOS hosts only AUv3 plugins, so our VST3/CLAP rule would
leave it with no third-party plugins, and cpal's iOS build has reported linker
trouble under Tauri. A Sampler-only iPad app with no plugins is possible later;
it would be an owner decision.

## Sources

- Apple Newsroom, "Apple unveils groundbreaking MacBook Pro with PROLED XDR Display", 2026-10-28: https://www.apple.com/newsroom/2026/10/apple-unveils-groundbreaking-macbook-pro-with-proled-xdr-display/
- Apple tech talk, "Bring Touch to your Mac app" (WWDC26): https://developer.apple.com/videos/play/tech-talks/111468/
- MacRumors, macOS 27 Sidecar direct touch, 2026-06-09: https://www.macrumors.com/2026/06/09/macos-27-brings-direct-touch-control-to-sidecar/
- MacRumors, touchscreen MacBook Pro features, 2026-10-28: https://www.macrumors.com/2026/10/28/touchscreen-macbook-pro-debut-these-5-features/
- *Secondary:* Blake Crosley, "Touch on Mac for Developers", 2026-10-28: https://blakecrosley.com/blog/touch-on-mac-for-developers
- *Secondary:* byteiota, "Apple Touchscreen MacBook Pro: What Developers Must Test Now": https://byteiota.com/apple-touchscreen-macbook-pro-developer-guide/
- WebKit commit on `touch-action` and pointer events on macOS: https://webkit.googlesource.com/WebKit/+/19a4b7274c591e88d2ce1fec24df38fd8335bfc5
- Logic Pro for iPad, drum pad velocity: https://support.apple.com/guide/logicpro-ipad/lpip465fc5e1/ipados
- Logic Pro for iPad, Quick Sampler slicing: https://support.apple.com/guide/logicpro-ipad/lpipda25caca/ipados
- Logic Pro for iPad, Live Loops cell editing: https://support.apple.com/guide/logicpro-ipad/lpip7ff7ae9f/ipados
- Ableton Note manual: https://www.ableton.com/en/note/manual/note-manual/
- Bitwig, working on a tablet computer: https://bitwig.com/userguide/latest/working_on_a_tablet_computer
- Bitwig, radial gesture menu: https://www.bitwig.com/userguide/latest/the_radial_gesture_menu
- PreSonus, touchscreens with Studio One: https://support.presonus.com/hc/en-us/articles/360003650272-Is-it-possible-to-use-a-touchscreen-monitor-with-Studio-One
- Steinberg forum, Cubase multi-touch: https://forums.steinberg.net/t/multi-touch-support-when/901580
- Ableton forum, Live and touchscreens: https://forum.ableton.com/viewtopic.php?p=1753266
- Loopy Pro forum, Koala Sampler pad selection: https://forum.loopypro.com/discussion/comment/1426556/
- CLAP remote-controls extension (via nice-plug docs): https://docs.rs/nice-plug/0.1.6/nice_plug/context/remote_controls/
- McPherson et al., action-to-sound latency, NIME 2016: https://nime.org/proc/nime2016_mcpherson/
- TriboTouch, touchscreen latency, CHI 2022: https://www.figlab.com/research/2022/tribotouch
- Tauri 2.0 (iOS and Android): https://v2.tauri.app/blog/tauri-20/
- cpal iOS backend: https://docs.rs/crate/cpal/latest/source/src/host/coreaudio/ios/mod.rs
- Rust forum, cpal linking in a Tauri iOS build: https://users.rust-lang.org/t/tauri-rodio-and-ios-cross-compiling-and-linking-c-dependencies-for-ios/124563
