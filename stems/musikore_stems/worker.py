"""The stem worker process. The app starts one per job and kills it to cancel.

    python -m musikore_stems.worker --list
        One line: {"event": "backends", "backends": [...]}

    python -m musikore_stems.worker
        Reads one JSON request from stdin:
          {"input": "/tmp/job/mix.f32", "channels": 2, "frames": 10584000,
           "sample_rate": 44100, "backend": "demucs", "mode": "fast",
           "out_dir": "/tmp/job"}
        and writes JSON lines to stdout:
          {"event": "progress", "stage": "separate", "fraction": 0.42}
          {"event": "done", "stems": {"vocals": "/tmp/job/vocals.f32", ...}, ...}
          {"event": "error", "message": "..."}

stdout carries only these lines. Anything a library prints goes to stderr,
which the app keeps as the job's log.
"""

from __future__ import annotations

import json
import os
import resource
import sys
import threading
import time
import traceback

from .rawio import read_planar, write_planar
from .separator import MODES, STEMS, backends, conform


class Events:
    """JSON lines on the real stdout; fd 1 itself is pointed at stderr."""

    def __init__(self) -> None:
        self._out = os.fdopen(os.dup(1), "w", buffering=1)
        os.dup2(2, 1)
        sys.stdout = sys.stderr
        self.peak_gpu = 0

    def send(self, event: str, **fields) -> None:
        self._out.write(json.dumps({"event": event, **fields}) + "\n")
        self._out.flush()

    def progress(self, stage: str, fraction: float) -> None:
        self._sample_gpu()
        self.send("progress", stage=stage, fraction=round(float(fraction), 4))

    def _sample_gpu(self) -> None:
        torch = sys.modules.get("torch")
        if torch is not None and torch.backends.mps.is_available():
            self.peak_gpu = max(self.peak_gpu, torch.mps.driver_allocated_memory())


def list_backends() -> list[dict]:
    out = []
    for cls in backends().values():
        reason = cls.unavailable()
        out.append({"id": cls.id, "name": cls.name, "modes": list(MODES), "modeLabels": cls.mode_labels,
                    "available": reason is None, "reason": reason})
    return out


def run(req: dict, events: Events) -> dict:
    classes = backends()
    backend, mode = req.get("backend", "demucs"), req.get("mode", "fast")
    if backend not in classes:
        raise ValueError(f"unknown backend {backend!r}")
    if mode not in MODES:
        raise ValueError(f"unknown mode {mode!r}")
    reason = classes[backend].unavailable()
    if reason:
        raise RuntimeError(reason)

    channels, frames, rate = int(req["channels"]), int(req["frames"]), int(req["sample_rate"])
    mix = read_planar(req["input"], channels, frames)
    t0 = time.monotonic()
    stems = classes[backend]().separate(mix, rate, mode, events.progress)
    stems = conform(stems, channels, frames)
    seconds = time.monotonic() - t0

    events.progress("write", 0.97)
    out_dir = req["out_dir"]
    paths = {}
    for name in STEMS:
        paths[name] = os.path.join(out_dir, f"{name}.f32")
        write_planar(paths[name], stems[name])
    events._sample_gpu()
    return {
        "stems": paths,
        "seconds": round(seconds, 2),
        "peakRssMb": round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20),
        "peakGpuMb": round(events.peak_gpu / 2**20),
    }


def exit_with_parent() -> None:
    """Stops the worker if the app goes away (quit or crash) mid-job."""
    parent = os.getppid()

    def watch() -> None:
        while True:
            time.sleep(1)
            if os.getppid() != parent:
                os._exit(1)

    threading.Thread(target=watch, daemon=True).start()


def main() -> int:
    exit_with_parent()
    events = Events()
    if "--list" in sys.argv:
        events.send("backends", backends=list_backends())
        return 0
    try:
        line = sys.stdin.readline()
        if not line:
            raise ValueError("no request on stdin")
        req = json.loads(line)
        events.send("done", **run(req, events))
        return 0
    except Exception as e:  # report it, then fail the job
        traceback.print_exc()
        events.send("error", message=f"{type(e).__name__}: {e}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
