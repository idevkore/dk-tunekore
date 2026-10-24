"""Meta's SAM Audio (github.com/facebookresearch/sam-audio), under the SAM License.

SAM Audio isolates whatever a text prompt names. We ask it for vocals, drums
and bass, then cut all four stems from the mix with soft masks (see
`mixing.py`), so "other" is what remains.

- Fast: `facebook/sam-audio-small` (5.1 GB). Best: `facebook/sam-audio-base`
  (7.7 GB). `sam-audio-large` (14.9 GB in float32) doesn't fit in this Mac's
  16 GB with everything else running. TUNEKORE_SAM_FAST_MODEL and
  TUNEKORE_SAM_BEST_MODEL pick other checkpoints (e.g. the `-tv` ones).
- The checkpoints are gated. Nate logs in with `hf auth login`; the worker
  only reads the cached login through huggingface_hub and never sees it.
- Weights live in this Mac's Hugging Face cache, never in the repo.

The model works on mono audio at 48 kHz, so the song is mixed to mono and
split into chunks (20 s, with a 2 s crossfade), one stem at a time, to fit
in a 16 GB Mac's GPU memory.
The decoder adds an inaudible watermark to what it generates; the masked
stems are cut from the original mix, so they carry little of it.
"""

from __future__ import annotations

import json
import math
import os
import time

import numpy as np

from .mixing import mask_from_mix
from .separator import Progress, Separator, torch_device

MODELS = {"fast": "facebook/sam-audio-small", "best": "facebook/sam-audio-base"}
PROMPTS = {"vocals": "vocals", "drums": "drums", "bass": "bass"}
SAMPLE_RATE = 48_000
CHUNK_SECONDS = 20.0
OVERLAP_SECONDS = 2.0
# The model's own default: midpoint steps of 1/16, two evaluations each.
STEP_SIZE = 2 / 32


def model_id(mode: str) -> str:
    return os.environ.get(f"TUNEKORE_SAM_{mode.upper()}_MODEL", MODELS[mode])


class SamAudioSeparator(Separator):
    id = "sam-audio"
    name = "SAM Audio (Meta)"
    mode_labels = {"fast": "sam-audio-small", "best": "sam-audio-base"}

    @classmethod
    def unavailable(cls) -> str | None:
        try:
            from huggingface_hub import get_token

            from . import shims

            shims.install()
            import sam_audio  # noqa: F401
        except ImportError as e:
            return f"SAM Audio is not installed in the stem worker ({e.name})"
        if get_token() is None and not os.environ.get("TUNEKORE_SAM_OFFLINE"):
            return "log in to Hugging Face first: stems/.venv/bin/hf auth login"
        return None

    def separate(self, mix: np.ndarray, sample_rate: int, mode: str, progress: Progress) -> dict[str, np.ndarray]:
        import julius
        import torch

        progress("load", 0.0)
        model, processor = load(model_id(mode), progress)
        device = torch_device()
        model = model.to(device)

        channels, frames = mix.shape
        mono = torch.from_numpy(mix.mean(0))
        mono48 = julius.resample_frac(mono, sample_rate, SAMPLE_RATE) if sample_rate != SAMPLE_RATE else mono
        n = mono48.shape[-1]

        chunk = int(CHUNK_SECONDS * SAMPLE_RATE)
        ramp = int(OVERLAP_SECONDS * SAMPLE_RATE)
        starts = chunk_starts(n, chunk, ramp)
        names = list(PROMPTS)
        acc = torch.zeros(len(names), n)
        weight = torch.zeros(n)
        for i, start in enumerate(starts):
            weight[start:start + chunk] += fade_weights(min(chunk, n - start), ramp, i == 0, i == len(starts) - 1)

        # One stem and one chunk at a time: generating several at once ran
        # out of GPU memory in the decoder on a 16 GB Mac.
        evals_per_chunk = 2 * round(1 / STEP_SIZE)
        total = len(names) * len(starts) * evals_per_chunk
        calls = [0]
        stage = [names[0]]
        inner_forward = model.forward

        def counted_forward(*args, **kwargs):
            out = inner_forward(*args, **kwargs)
            calls[0] += 1
            progress(stage[0], 0.1 + 0.8 * min(calls[0] / total, 1.0))
            return out

        model.forward = counted_forward  # the ODE solver calls self.forward
        torch.manual_seed(0)
        for j, name in enumerate(names):
            stage[0] = name
            progress(name, 0.1 + 0.8 * calls[0] / total)
            for i, start in enumerate(starts):
                seg = mono48[start:start + chunk]
                batch = processor(descriptions=[PROMPTS[name]], audios=[seg[None]]).to(device)
                est = separate_target(model, batch)[: seg.shape[-1]]
                w = fade_weights(seg.shape[-1], ramp, first=i == 0, last=i == len(starts) - 1)
                acc[j, start:start + est.shape[-1]] += est * w[: est.shape[-1]]
                del batch
                if device.type == "mps":
                    torch.mps.empty_cache()
        model.forward = inner_forward
        acc /= weight.clamp(min=1e-8)

        if sample_rate != SAMPLE_RATE:
            acc = julius.resample_frac(acc, SAMPLE_RATE, sample_rate)
        acc = acc[..., :frames]
        if acc.shape[-1] < frames:
            acc = torch.nn.functional.pad(acc, (0, frames - acc.shape[-1]))
        estimates = {k: acc[j].numpy() for j, k in enumerate(names)}

        progress("other", 0.92)
        if os.environ.get("TUNEKORE_SAM_STEREO", "mask") == "raw":
            # The model's own output: mono, and "other" is the mix minus the rest.
            stems = {k: np.repeat(v[None], channels, 0) for k, v in estimates.items()}
            stems["other"] = mix - sum(stems.values())
            return stems
        return mask_from_mix(mix, estimates)


def separate_target(model, batch):
    """SAMAudio.separate for one prompt, decoding only the target.

    The model generates the target and the residual together; `separate`
    then decodes both. We only use the target, and decoding is a fifth of
    the time, so this skips the residual's decode. Same ODE, same steps.
    """
    import torch
    from torchdiffeq import odeint

    with torch.inference_mode():
        args = model._get_forward_args(batch, candidates=1)
        features = args["audio_features"]
        half = features.shape[-1] // 2  # target and residual are stacked on the channels

        def field(t, x):
            return model.forward(noisy_audio=x, time=t.expand(x.size(0)), **args)

        noise = torch.randn_like(features)
        states = odeint(field, noise, torch.tensor([0.0, 1.0], device=noise.device),
                        method="midpoint", options={"step_size": STEP_SIZE})
        target = states[-1].transpose(1, 2)[:, :half]
        wav = model.audio_codec.decode(target)
        size = int(model.audio_codec.feature_idx_to_wav_idx(batch.sizes)[0])
        return wav.reshape(-1)[:size].float().cpu()


def chunk_starts(n: int, chunk: int, overlap: int) -> list[int]:
    """Chunk start positions covering n samples; the last chunk ends at n."""
    if n <= chunk:
        return [0]
    count = math.ceil((n - overlap) / (chunk - overlap))
    starts = [i * (chunk - overlap) for i in range(count - 1)]
    starts.append(n - chunk)
    return starts


def fade_weights(length: int, ramp: int, first: bool, last: bool):
    """Crossfade weights: linear ramps at the inner edges of a chunk."""
    import torch

    w = torch.ones(length)
    r = min(ramp, length // 2)
    if r > 0 and not first:
        w[:r] = torch.linspace(0, 1, r + 2)[1:-1]
    if r > 0 and not last:
        w[-r:] = torch.linspace(1, 0, r + 2)[1:-1]
    return w


class _NoVision(object):
    """Takes the vision encoder's place: we never prompt with video."""

    def __init__(self, dim: int):
        self.dim = dim


def load(repo_id: str, progress: Progress | None = None):
    """Loads a SAM Audio checkpoint without the parts text prompts don't use.

    The span predictor and the re-rankers are separate downloads (one is
    ImageBind, under a non-commercial license), so we leave them out, and we
    drop the video encoder after loading to save memory.
    """
    import torch
    from huggingface_hub import snapshot_download

    from . import shims

    shims.install()
    from sam_audio import SAMAudio, SAMAudioProcessor
    from sam_audio.model.config import SAMAudioConfig

    t0 = time.monotonic()
    path = snapshot_download(repo_id, allow_patterns=["config.json", "checkpoint.pt", "LICENSE*"])
    with open(os.path.join(path, "config.json")) as f:
        cfg = json.load(f)
    cfg["span_predictor"] = None
    cfg["visual_ranker"] = None
    cfg["text_ranker"] = None
    config = SAMAudioConfig(**cfg)
    model = SAMAudio(config)
    state = torch.load(os.path.join(path, "checkpoint.pt"), map_location="cpu", weights_only=True, mmap=True)
    skip = ("span_predictor.", "visual_ranker.", "text_ranker.")
    state = {k: v for k, v in state.items() if not k.startswith(skip)}
    model.load_state_dict(state)
    del state
    del model.vision_encoder  # a registered submodule; delete before replacing
    model.vision_encoder = _NoVision(config.vision_encoder.dim)
    model.eval()
    processor = SAMAudioProcessor(config.audio_codec.hop_length, config.audio_codec.sample_rate)
    if progress:
        progress("load", 0.1)
    model.load_seconds = time.monotonic() - t0
    return model, processor
