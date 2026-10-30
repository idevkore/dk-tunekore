# soundCraft `vst3-host`, as copied

Source: https://github.com/storytold/soundcraft, commit `7dc56a4`
(`ui: Tracks and Groups and Clips in full-height dock columns (#1088)`),
directory `crates/vst3-host`. Licence: MIT OR Apache-2.0, copyright
2026 ArtCraft Team and the SoundCraft contributors. `LICENSE-MIT`,
`LICENSE-APACHE` and `NOTICE` sit next to this file exactly as upstream has
them. The ArtCraft name and logos are not covered by that licence and we use
neither.

This folder is the upstream crate **unmodified**. It is not part of the
Cargo workspace and is not built. The next commits move the files we use
into `plugins/src/` and change them there, so `git log --follow` shows what
is ours and what is theirs. Whatever has not moved by the end of the PR is
deleted from here, and this README then says what we kept.

Not copied: soundCraft's `dsp` crate, whose `Plugin` trait we read but replaced
with our own (multiple buses, preallocated buffers, a separate main-thread
half).
