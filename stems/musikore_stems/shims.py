"""Stand-ins for modules SAM Audio's code imports but never calls on our path.

We install SAM Audio, dacvae and perception_models without their declared
dependencies (see `pyproject.toml`). Their code still imports four heavy
packages at module load:

- `torchaudio`: used only to load or resample a file path. We pass tensors
  already at 48 kHz.
- `torchcodec`: FFmpeg-based decoders, used only for file paths and video.
- `audiotools` (descript-audiotools): dacvae's base class and training losses.
  It would bring libsndfile (LGPL) and much else.
- `xformers`: CUDA attention kernels that perception_models imports for a
  type hint and an attention mode SAM Audio doesn't select.

`install()` puts a small module in `sys.modules` for each one that isn't
installed. Anything that is really called raises, so a code path we didn't
expect fails loudly instead of silently.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import sys
import types


def _missing(name: str):
    def fail(*_args, **_kwargs):
        raise RuntimeError(f"{name} is not installed in musiKore's stem worker")

    return fail


def _module(name: str, **attrs) -> types.ModuleType:
    mod = types.ModuleType(name)
    # Some libraries call importlib.util.find_spec on it, which needs a spec.
    mod.__spec__ = importlib.machinery.ModuleSpec(name, loader=None)
    mod.__dict__.update(attrs)
    mod.__musikore_shim__ = True
    sys.modules[name] = mod
    return mod


def _have(name: str) -> bool:
    return name in sys.modules or importlib.util.find_spec(name) is not None


def install() -> None:
    import torch
    # transformers caches which optional packages exist the first time it
    # asks; let it ask before the stand-ins exist, so it sees them as absent
    # rather than half-installed.
    import transformers.utils.import_utils as tu

    tu.is_torchcodec_available()
    tu.is_torchaudio_available()

    if not _have("torchaudio"):
        functional = _module("torchaudio.functional", resample=_missing("torchaudio.functional.resample"))
        _module("torchaudio", load=_missing("torchaudio.load"), save=_missing("torchaudio.save"), functional=functional)

    if not _have("torchcodec"):
        class _Codec:
            def __init__(self, *_a, **_k):
                raise RuntimeError("torchcodec is not installed in musiKore's stem worker")

        decoders = _module("torchcodec.decoders", AudioDecoder=_Codec, VideoDecoder=_Codec)
        encoders = _module("torchcodec.encoders", AudioEncoder=_Codec)
        _module("torchcodec", decoders=decoders, encoders=encoders)

    if not _have("audiotools"):
        class BaseModel(torch.nn.Module):
            """audiotools.ml.BaseModel without its save/load helpers."""

            INTERN: list[str] = []
            EXTERN: list[str] = []

        class _Unavailable:
            def __init__(self, *_a, **_k):
                raise RuntimeError("audiotools is not installed in musiKore's stem worker")

        ml = _module("audiotools.ml", BaseModel=BaseModel)
        _module("audiotools", ml=ml, AudioSignal=_Unavailable, STFTParams=_Unavailable)

    if not _have("xformers"):
        class AttentionBias:
            pass

        fmha = _module("xformers.ops.fmha", memory_efficient_attention=_missing("xformers fmha"))
        ops = _module("xformers.ops", AttentionBias=AttentionBias, fmha=fmha)
        _module("xformers", ops=ops)
