"""Demucs (htdemucs). MIT code; the weights are for research only.

Weights download to this Mac's Hugging Face cache on first use. They are never
committed, bundled or shipped (docs/PLAN.md section 6).

- Fast: `htdemucs`, one pass that produces all four stems.
- Best: `htdemucs_ft`, four fine-tuned models, one per stem (like the
  per-stem passes in other DAWs' high-quality modes), with one random shift.
"""

from __future__ import annotations

import math

import numpy as np

from .separator import Progress, Separator, torch_device

MODELS = {"fast": ("htdemucs", 0), "best": ("htdemucs_ft", 1)}  # (model, shifts)
OVERLAP = 0.25


class DemucsSeparator(Separator):
    id = "demucs"
    name = "Demucs (htdemucs)"
    mode_labels = {"fast": "htdemucs, one pass", "best": "htdemucs_ft, one model per stem"}

    @classmethod
    def unavailable(cls) -> str | None:
        try:
            import demucs  # noqa: F401
        except ImportError:
            return "demucs is not installed in the stem worker"
        return None

    def separate(self, mix: np.ndarray, sample_rate: int, mode: str, progress: Progress) -> dict[str, np.ndarray]:
        import julius
        import torch
        from demucs.apply import BagOfModels, apply_model
        from demucs.pretrained import get_model

        name, shifts = MODELS[mode]
        progress("load", 0.0)
        model = get_model(name)
        model.eval()
        device = torch_device()

        channels, frames = mix.shape
        if channels not in (1, 2):
            raise ValueError(f"Demucs takes mono or stereo, not {channels} channels")
        x = torch.from_numpy(mix)
        if channels == 1:
            x = x.repeat(2, 1)
        if sample_rate != model.samplerate:
            x = julius.resample_frac(x, sample_rate, model.samplerate)

        # Demucs was trained on normalised mixes (as its own CLI does).
        ref = x.mean(0)
        mean, std = ref.mean(), ref.std() + 1e-8
        x = (x - mean) / std

        subs = model.models if isinstance(model, BagOfModels) else [model]
        stages = [_stage_for(model, i) for i in range(len(subs))]
        length = x.shape[-1] + (int(0.5 * model.samplerate) * 2 if shifts else 0)
        per_pass = [math.ceil(length / int((1 - OVERLAP) * int(m.samplerate * m.segment))) for m in subs]
        total = sum(per_pass) * max(shifts, 1)
        done = [0]

        def callback(d: dict) -> None:
            if d.get("state") != "end":
                return
            done[0] += 1
            progress(stages[d.get("model_idx_in_bag", 0)], 0.05 + 0.9 * min(done[0] / total, 1.0))

        progress(stages[0], 0.05)
        with torch.inference_mode():
            out = apply_model(model, x[None], device=device, shifts=shifts, split=True, overlap=OVERLAP,
                              progress=False, callback=callback)[0]
        out = out.cpu() * std + mean
        if sample_rate != model.samplerate:
            out = julius.resample_frac(out, model.samplerate, sample_rate)
        out = out[..., :frames]
        if channels == 1:
            out = out.mean(1, keepdim=True)
        return {src: out[i].numpy() for i, src in enumerate(model.sources)}


def _stage_for(model, index: int) -> str:
    """The stem a bag's sub-model is in charge of, or "separate" for one model."""
    from demucs.apply import BagOfModels

    if not isinstance(model, BagOfModels) or len(model.models) == 1:
        return "separate"
    weights = model.weights[index]
    best = max(range(len(weights)), key=lambda i: weights[i])
    if sum(1 for w in weights if w > 0) == 1:
        return model.sources[best]
    return "separate"
