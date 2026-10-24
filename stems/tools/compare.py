"""Compares separators on the same songs, on this Mac: speed, memory, the
Phase 1 checks and, where we own reference stems, SDR.

    python tools/compare.py RESULTS_DIR SONG [SONG ...] \
        [--runs demucs:fast demucs:best sam-audio:fast sam-audio:best]

A SONG is an audio file, or a folder with mix.wav and stems/{vocals,drums,bass,other}.wav
(as tools/make_test_song.py writes). Each run goes through the Rust
`analyze` example, the same decode → worker → checks path as the app. The
stems land in RESULTS_DIR/<song>/<backend>-<mode>/ for listening, and the
scores in RESULTS_DIR/results.json and RESULTS_DIR/results.md.

SDR is the plain signal-to-distortion ratio over the whole track (the
"new SDR" of the MDX challenges): 10·log10(Σ ref² / Σ (ref − estimate)²).
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys

import numpy as np

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from tunekore_stems.rawio import read_wav  # noqa: E402
from tunekore_stems.separator import STEMS  # noqa: E402

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
ANALYZE = os.path.join(REPO, "target", "release", "examples", "analyze")


def sdr(ref: np.ndarray, est: np.ndarray) -> float:
    n = min(ref.shape[-1], est.shape[-1])
    ref, est = ref[..., :n].astype(np.float64), est[..., :n].astype(np.float64)
    num = (ref ** 2).sum() + 1e-9
    den = ((ref - est) ** 2).sum() + 1e-9
    return float(10 * np.log10(num / den))


def song_input(song: str) -> tuple[str, str, str | None]:
    """(name, audio file, reference stems folder or None)."""
    if os.path.isdir(song):
        return os.path.basename(song.rstrip("/")), os.path.join(song, "mix.wav"), os.path.join(song, "stems")
    return os.path.splitext(os.path.basename(song))[0], song, None


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("results")
    ap.add_argument("songs", nargs="+")
    ap.add_argument("--runs", nargs="+", default=["demucs:fast", "demucs:best", "sam-audio:fast", "sam-audio:best"])
    a = ap.parse_args()
    if not os.path.exists(ANALYZE):
        sys.exit("build it first: cargo build --release -p tunekore-analyzer --example analyze")
    os.makedirs(a.results, exist_ok=True)
    path = os.path.join(a.results, "results.json")
    results = json.load(open(path)) if os.path.exists(path) else []

    for song in a.songs:
        name, audio, refs = song_input(song)
        for run in a.runs:
            backend, mode = run.split(":")
            out = os.path.join(a.results, name, f"{backend}-{mode}")
            print(f"{name}: {backend} {mode} ...", flush=True)
            p = subprocess.run([ANALYZE, audio, "--backend", backend, "--mode", mode, "--out", out],
                               capture_output=True, text=True, cwd=REPO, env=os.environ)
            if p.returncode != 0:
                print(p.stderr.strip()[-2000:], flush=True)
                results = [r for r in results if not (r["song"] == name and r["run"] == run)]
                results.append({"song": name, "run": run, "error": p.stderr.strip()[-500:]})
                continue
            rep = json.loads(p.stdout.strip().splitlines()[-1])
            row = {**rep, "song": name, "run": run}
            if refs and os.path.isdir(refs):
                scores = {}
                for stem in STEMS:
                    ref, _ = read_wav(os.path.join(refs, f"{stem}.wav"))
                    est, _ = read_wav(os.path.join(out, f"{stem}.wav"))
                    scores[stem] = round(sdr(ref, est), 2)
                scores["mean"] = round(float(np.mean([scores[s] for s in STEMS])), 2)
                row["sdr"] = scores
            print(f"  {rep['separateSeconds']:.1f} s, sum {rep['checks']['sumSnrDb']:.1f} dB, lag {rep['checks']['lagFrames']}"
                  + (f", SDR {row['sdr']}" if "sdr" in row else ""), flush=True)
            results = [r for r in results if not (r["song"] == name and r["run"] == run)]
            results.append(row)
            json.dump(results, open(path, "w"), indent=1)

    json.dump(results, open(path, "w"), indent=1)
    lines = ["| Song | Model, mode | Time (s) | x real time | Worker RSS / GPU (MB) | Sum SNR (dB) | Lag | SDR vocals | drums | bass | other | mean |",
             "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
    for r in results:
        if "error" in r:
            lines.append(f"| {r['song']} | {r['run']} | failed: {r['error'][-120:]} | | | | | | | | | |")
            continue
        s = r.get("sdr", {})
        cells = [s.get(k, "") for k in (*STEMS, "mean")]
        lines.append(
            f"| {r['song']} | {r['run']} | {r['separateSeconds']:.1f} | {r['seconds'] / r['separateSeconds']:.1f} | "
            f"{r['peakRssMb']} / {r['peakGpuMb']} | {r['checks']['sumSnrDb']:.1f} | {r['checks']['lagFrames']} | "
            + " | ".join(str(c) for c in cells) + " |")
    open(os.path.join(a.results, "results.md"), "w").write("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
