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

from tunekore_stems.rawio import read_planar, read_wav, write_planar, write_wav16  # noqa: E402
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
    assert {"demucs", "test-split"} <= set(ids)
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




def test_wav_round_trip(tmp_path):
    x = np.linspace(-0.5, 0.5, 200, dtype=np.float32).reshape(2, 100)
    write_wav16(str(tmp_path / "a.wav"), x, 44_100)
    y, rate = read_wav(str(tmp_path / "a.wav"))
    assert rate == 44_100 and y.shape == (2, 100) and np.abs(x - y).max() < 1e-4
