# DK.musiKore

musiKore is devKore's lightweight Ableton-style DAW: a platform for our own
instruments, effects and tools, owned outright. macOS first, then Windows and
Linux. Owner: Nate Soria. Tracker: `idevkore/kore-issues` (sidebar group KORE).

- `docs/PLAN.md`: the stack, architecture, roadmap and licensing decisions. Read
  it before structural work, and update it in the same PR when a decision
  changes.
- `brand/`: the logo, the K› icon and the app icon, with the scripts that
  draw them (`brand/README.md`).

## Rules

- **Own the IP.** A dependency must be MIT, BSD, Apache-2.0, zlib, ISC or public
  domain. MPL-2.0 is fine for unmodified files. Nothing GPL, AGPL or LGPL goes
  into the app, and no model weights or datasets that are non-commercial or
  have no license. Record each dependency and its license in
  `THIRD_PARTY.md` in the PR that adds it.
- **Ableton as reference.**
  - Live, its manual (the Ableton Knowledge connector), its behavior and the
    app itself are fair game for ideas, including study with reverse
    engineering tools. Nate decided this on 2026-10-23.
  - What stays out of musiKore:
    - Live's code: nothing pasted or translated from a decompiler.
    - Its graphics, presets, samples, Packs and device names.
    - Anything that touches Live's license check.
  - Keep reverse engineering notes outside this repo, and write musiKore's
    code from our own descriptions of what Live does.
- **Names.** No "Ableton", "Live", "Push", "Session View" or "VST" in product,
  feature or UI names. Ours are Clip Grid, Timeline, Library, Analyzer.
- **Audio thread.** No allocation, locks, logging or file I/O on the audio
  thread. Hand data across with lock-free queues.
- **Stems.** Demucs weights are for prototyping on Nate's machine only. Never
  bundle or ship them (see `docs/PLAN.md` section 6).
- **Plugins to test with** are installed on Nate's Mac as AU and VST3: Serum,
  Serum 2 (Splice Rent-to-Own), Arturia V Collection 11 and Native
  Instruments. The test list is in `docs/PLAN.md` section 5.
