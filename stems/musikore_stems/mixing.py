"""Turns rough stem estimates into stems cut from the mix itself.

SAM Audio generates each stem as new mono audio at 48 kHz. Its output is close
to the source but not sample-identical to it, it has no stereo, and its stems
don't add up to the song. `mask_from_mix` uses the estimates only to decide,
for each time-frequency cell, how much of the mix belongs to each stem (a
Wiener-style soft mask), then cuts the stems out of the original stereo mix.
The stems keep the mix's stereo image and phase, line up with it to the
sample, and sum back to it exactly (up to float rounding).
"""

from __future__ import annotations

import numpy as np

N_FFT = 4096
HOP = 1024
EPS = 1e-10


def mask_from_mix(mix: np.ndarray, estimates: dict[str, np.ndarray], other: str | None = "other",
                  power: float = 2.0) -> dict[str, np.ndarray]:
    """Splits `mix` (channels, frames) by soft masks built from `estimates`.

    Each estimate is (frames,) or (1 or channels, frames) at the mix's rate.
    If `other` names a stem missing from `estimates`, its magnitude is what is
    left of the mix's magnitude after the named stems, floored at zero.
    """
    import torch

    channels, frames = mix.shape
    window = torch.hann_window(N_FFT)

    def stft(x: np.ndarray) -> torch.Tensor:
        t = torch.from_numpy(np.ascontiguousarray(x, dtype=np.float32))
        if t.ndim == 1:
            t = t[None]
        return torch.stft(t, N_FFT, HOP, window=window, return_complex=True)

    spec = stft(mix)  # (channels, bins, frames)
    mags = {name: stft(est).abs() for name, est in estimates.items()}
    if other is not None and other not in mags:
        mix_mag = spec.abs().mean(0, keepdim=True)
        named = sum(m.mean(0, keepdim=True) for m in mags.values())
        mags[other] = (mix_mag - named).clamp(min=0)

    weights = {name: (m + EPS) ** power for name, m in mags.items()}
    total = sum(weights.values())
    out = {}
    for name, w in weights.items():
        stem = torch.istft(spec * (w / total), N_FFT, HOP, window=window, length=frames)
        out[name] = stem.numpy().astype(np.float32)
    return out
