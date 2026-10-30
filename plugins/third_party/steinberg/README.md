# Steinberg VST 3 API

The host talks to VST3 plugins through the `vst3` crate (MIT OR Apache-2.0),
whose Rust bindings are generated from Steinberg's VST 3 API headers
(`pluginterfaces`, part of the VST 3 SDK). Those headers are MIT-licensed since
SDK 3.8.0 (October 2025). `LICENSE.txt` here is Steinberg's licence text as
published in https://github.com/steinbergmedia/vst3_pluginterfaces, and must
ship with the app alongside the other third-party notices.

No Steinberg file is copied into this repository. "VST" is a Steinberg
trademark: tuneKore never uses it in a product or feature name (`CLAUDE.md`).
