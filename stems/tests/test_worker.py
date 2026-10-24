"""Worker tests that need no model weights."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import numpy as np
import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from tunekore_stems.mixing import mask_from_mix  # noqa: E402
from tunekore_stems.rawio import read_planar, read_wav, write_planar, write_wav16  # noqa: E402
from tunekore_stems.sam_audio_backend import chunk_starts, fade_weights  # noqa: E402
from tunekore_stems.separator import STEMS, conform  # noqa: E402


def worker(*args, stdin: str = "", env: dict | None = None):
    e = {**os.environ, "TUNEKORE_STEMS_TEST": "1", **(env or {})}
    p = subprocess.run([sys.executable, "-m", "tunekore_stems.worker", *args], input=stdin, cwd=ROOT,
                       capture_output=True, text=True, env=e, timeout=120)
    return p.returncode, [json.loads(line) for line in p.stdout.splitlines()], p.stderr


def test_lists_backends():
    code, events, _ = worker("--list")
    assert code == 0 and len(events) == 1
    ids = {b["id"]: b for b in events[0]["backends"]}
    assert {"demucs", "sam-audio", "test-split"} <= set(ids)
    assert ids["test-split"]["available"] is True
    assert ids["demucs"]["modes"] == ["fast", "best"]


def test_runs_a_job_end_to_end(tmp_path):
    rng = np.random.default_rng(0)
    mix = (rng.standard_normal((2, 48_000)) * 0.1).astype(np.float32)
    write_planar(str(tmp_path / "mix.f32"), mix)
    req = {"input": str(tmp_path / "mix.f32"), "channels": 2, "frames": 48_000, "sample_rate": 48_000,
           "backend": "test-split", "mode": "fast", "out_dir": str(tmp_path)}
    code, events, err = worker(stdin=json.dumps(req) + "\n")
    assert code == 0, err
    assert events[-1]["event"] == "done"
    assert [e["stage"] for e in events if e["event"] == "progress"][:4] == list(STEMS)
    total = sum(read_planar(events[-1]["stems"][s], 2, 48_000) for s in STEMS)
    assert np.allclose(total, mix, atol=1e-6)


def test_reports_errors_as_events(tmp_path):
    code, events, _ = worker(stdin=json.dumps({"backend": "nope"}) + "\n")
    assert code == 1 and events[-1]["event"] == "error" and "nope" in events[-1]["message"]


def test_conform_fixes_shape_and_rejects_nan():
    stems = {s: np.zeros((1, 90), np.float32) for s in STEMS}
    out = conform(stems, 2, 100)
    assert all(v.shape == (2, 100) for v in out.values())
    stems["bass"] = np.full((2, 100), np.nan, np.float32)
    with pytest.raises(RuntimeError):
        conform(stems, 2, 100)


def test_masks_cut_stems_that_sum_to_the_mix():
    rng = np.random.default_rng(1)
    n = 48_000
    parts = {s: (rng.standard_normal((2, n)) * (i + 1) * 0.05).astype(np.float32) for i, s in enumerate(STEMS)}
    mix = sum(parts.values())
    estimates = {k: v.mean(0) for k, v in parts.items() if k != "other"}  # mono, like SAM Audio's
    out = mask_from_mix(mix, estimates)
    assert set(out) == set(STEMS)
    assert all(v.shape == (2, n) for v in out.values())
    err = mix - sum(out.values())
    assert 10 * np.log10((mix ** 2).sum() / (err ** 2).sum()) > 80


def test_chunks_cover_the_song():
    starts = chunk_starts(100, 30, 5)
    assert starts[0] == 0 and starts[-1] == 70
    covered = np.zeros(100)
    for i, s in enumerate(starts):
        covered[s:s + 30] += fade_weights(30, 5, i == 0, i == len(starts) - 1).numpy()
    assert covered.min() > 0
    assert chunk_starts(20, 30, 5) == [0]


def test_wav_round_trip(tmp_path):
    x = np.linspace(-0.5, 0.5, 200, dtype=np.float32).reshape(2, 100)
    write_wav16(str(tmp_path / "a.wav"), x, 44_100)
    y, rate = read_wav(str(tmp_path / "a.wav"))
    assert rate == 44_100 and y.shape == (2, 100) and np.abs(x - y).max() < 1e-4
