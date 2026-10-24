# Stem worker

The Analyzer's separation runs here, in its own Python process, so a long or
failed job never touches the app or the audio thread. The app starts one
worker per job and kills it to cancel. Phase 1 runs it from this folder on
Nate's Mac; it is not bundled into the app.

- `musikore_stems/separator.py`: the `Separator` interface. Every model sits
  behind it and returns vocals, drums, bass and other with the song's exact
  channels, length and sample rate.
- `demucs_backend.py`: Demucs. Fast is `htdemucs`, Best is `htdemucs_ft`.
- `sam_audio_backend.py`: Meta's SAM Audio. Fast is `sam-audio-small`, Best
  is `sam-audio-base`. It is asked for vocals, drums and bass; `mixing.py`
  then cuts all four stems out of the original stereo mix with soft masks.
- `worker.py`: the JSON-lines protocol the app speaks (see its docstring).
- `shims.py`: stand-ins for four packages SAM Audio imports but doesn't use on
  our path, so their LGPL, FFmpeg and non-commercial dependencies stay out.
- `tools/make_test_song.py`: synthesises a test song with known stems, for
  SDR scores we own. `tools/compare.py`: runs every model on the same songs.

## Set up

```bash
cd stems
uv sync --extra sam
```

`uv sync` without `--extra sam` removes SAM Audio again (that is what CI
runs). To use SAM Audio, Nate requests access to the `facebook/sam-audio-*`
checkpoints on Hugging Face, then logs in once:

```bash
stems/.venv/bin/hf auth login
```

Weights download to `~/.cache/huggingface` on first use: about 160 MB for
Demucs, 5.1 GB for `sam-audio-small`, 7.7 GB for `sam-audio-base` and 0.9 GB
for T5. Never commit, bundle or ship them.

## Test

```bash
cd stems && uv run pytest -q tests    # no weights needed
cargo test -p musikore-analyzer       # app → worker path, with a test backend
```

## Compare models

```bash
cargo build --release -p musikore-analyzer --example analyze
cd stems
.venv/bin/python tools/make_test_song.py /tmp/songs/synth-a --bpm 120 --seed 1
.venv/bin/python tools/compare.py /tmp/results /tmp/songs/synth-a ~/Music/song.mp3
```

Results so far are in `docs/PLAN.md` section 6.
