# Stem worker

The Analyzer's separation runs here, in its own Python process, so a long or
failed job never touches the app or the audio thread. The app starts one
worker per job and kills it to cancel. Phase 1 runs it from this folder on
Nate's Mac; it is not bundled into the app.

- `tunekore_stems/separator.py`: the `Separator` interface. Every model sits
  behind it and returns vocals, drums, bass and other with the song's exact
  channels, length and sample rate. A shippable model replaces Demucs here
  without touching the app.
- `demucs_backend.py`: Demucs. Fast is `htdemucs`, Best is `htdemucs_ft`.
- `worker.py`: the JSON-lines protocol the app speaks (see its docstring).
- `test_backend.py`: a model-free separator for tests.
- `tools/make_test_song.py`: synthesises a test song with known stems, for
  SDR scores we own. `tools/compare.py`: runs separators on the same songs.

## Set up

```bash
cd stems
uv sync
```

Demucs's weights (about 160 MB) download to `~/.cache/huggingface` on first
use. They are for research only: never commit, bundle or ship them.

## Test

```bash
cd stems && uv run pytest -q tests    # no weights needed
cargo test -p tunekore-analyzer       # app → worker path, with a test backend
```

## Compare models

```bash
cargo build --release -p tunekore-analyzer --example analyze
cd stems
.venv/bin/python tools/make_test_song.py /tmp/songs/synth-a --bpm 120 --seed 1
.venv/bin/python tools/compare.py /tmp/results /tmp/songs/synth-a ~/Music/song.mp3
```

Results so far are in `docs/PLAN.md` section 6.
