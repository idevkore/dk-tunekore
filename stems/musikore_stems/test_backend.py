"""A separator with no model, for tests: fixed shares of the mix.

Listed only when MUSIKORE_STEMS_TEST=1, so the app never offers it.
"""

from __future__ import annotations

import numpy as np

from .separator import STEMS, Progress, Separator

SHARES = {"vocals": 0.4, "drums": 0.3, "bass": 0.2, "other": 0.1}


class TestSeparator(Separator):
    id = "test-split"
    name = "Test split (no model)"
    mode_labels = {"fast": "fixed shares", "best": "fixed shares"}

    def separate(self, mix: np.ndarray, sample_rate: int, mode: str, progress: Progress) -> dict[str, np.ndarray]:
        out = {}
        for i, name in enumerate(STEMS):
            progress(name, (i + 1) / len(STEMS))
            out[name] = mix * SHARES[name]
        return out
