"""The `Separator` interface every stem model sits behind.

A separator takes a song as float32 samples, shape (channels, frames), and
returns four stems with exactly the same shape and sample rate: vocals,
drums, bass and other. Anything model-specific (sample rate, mono or stereo,
chunking, normalisation) stays inside the backend, so the Analyzer never sees
it and a backend can be swapped without touching the app.
"""

from __future__ import annotations

import os
from abc import ABC, abstractmethod
from typing import Callable

import numpy as np

STEMS = ("vocals", "drums", "bass", "other")
MODES = ("fast", "best")

# progress(stage, fraction): stage is a short name ("load", "separate",
# "vocals", ...); fraction is 0..1 across the whole job.
Progress = Callable[[str, float], None]


class Separator(ABC):
    id: str
    name: str
    #: Shown in the UI next to each mode.
    mode_labels: dict[str, str]

    @classmethod
    def unavailable(cls) -> str | None:
        """Why this backend can't run on this machine, or None if it can."""
        return None

    @abstractmethod
    def separate(self, mix: np.ndarray, sample_rate: int, mode: str, progress: Progress) -> dict[str, np.ndarray]:
        """Splits `mix` (channels, frames) into STEMS, each (channels, frames)."""


def torch_device():
    """MPS on Apple silicon, else CPU. TUNEKORE_STEMS_DEVICE overrides it."""
    import torch

    want = os.environ.get("TUNEKORE_STEMS_DEVICE")
    if want:
        return torch.device(want)
    if torch.backends.mps.is_available():
        return torch.device("mps")
    if torch.cuda.is_available():
        return torch.device("cuda")
    return torch.device("cpu")


def conform(stems: dict[str, np.ndarray], channels: int, frames: int) -> dict[str, np.ndarray]:
    """Checks a backend's output and makes every stem exactly (channels, frames)."""
    missing = [s for s in STEMS if s not in stems]
    if missing:
        raise RuntimeError(f"separator returned no {', '.join(missing)}")
    out = {}
    for name in STEMS:
        s = np.asarray(stems[name], dtype=np.float32)
        if s.ndim == 1:
            s = s[None]
        if s.shape[0] != channels:
            if s.shape[0] == 1:
                s = np.repeat(s, channels, 0)
            elif channels == 1:
                s = s.mean(0, keepdims=True)
            else:
                raise RuntimeError(f"separator returned {s.shape[0]} channels for {name}, wanted {channels}")
        if s.shape[1] < frames:
            s = np.pad(s, ((0, 0), (0, frames - s.shape[1])))
        s = np.ascontiguousarray(s[:, :frames])
        if not np.isfinite(s).all():
            raise RuntimeError(f"separator returned non-finite samples in {name}")
        out[name] = s
    return out


def backends() -> dict[str, type[Separator]]:
    from .demucs_backend import DemucsSeparator
    from .sam_audio_backend import SamAudioSeparator

    classes: list[type[Separator]] = [DemucsSeparator, SamAudioSeparator]
    if os.environ.get("TUNEKORE_STEMS_TEST") == "1":
        from .test_backend import TestSeparator

        classes.append(TestSeparator)
    return {cls.id: cls for cls in classes}
