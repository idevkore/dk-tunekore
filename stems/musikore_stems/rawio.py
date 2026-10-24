"""Audio the app and the worker hand each other.

The app decodes the song and writes it as raw float32, little-endian,
planar: all of channel 0, then all of channel 1. The worker writes each stem
the same way, with the same channels, frame count and sample rate. No codec
library in the worker, and nothing to get wrong about alignment.

The WAV helpers are for the comparison tool only.
"""

from __future__ import annotations

import os
import struct
import wave

import numpy as np


def read_planar(path: str, channels: int, frames: int) -> np.ndarray:
    data = np.fromfile(path, dtype="<f4")
    if data.size != channels * frames:
        raise ValueError(f"{path}: expected {channels * frames} samples, found {data.size}")
    return data.reshape(channels, frames)


def write_planar(path: str, audio: np.ndarray) -> None:
    tmp = path + ".part"
    np.ascontiguousarray(audio, dtype="<f4").tofile(tmp)
    os.replace(tmp, path)  # the app never sees a half-written stem


def read_wav(path: str) -> tuple[np.ndarray, int]:
    """Reads a 16/24-bit PCM or 32-bit float WAV as (channels, frames) float32."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise ValueError(f"{path} is not a WAV file")
    pos, fmt, raw = 12, None, None
    while pos + 8 <= len(data):
        cid, size = data[pos:pos + 4], struct.unpack("<I", data[pos + 4:pos + 8])[0]
        body = data[pos + 8:pos + 8 + size]
        if cid == b"fmt ":
            tag, ch, rate, _, _, bits = struct.unpack("<HHIIHH", body[:16])
            if tag == 0xFFFE:  # WAVE_FORMAT_EXTENSIBLE: the real tag is in the sub-format GUID
                tag = struct.unpack("<H", body[24:26])[0]
            fmt = (tag, ch, rate, bits)
        elif cid == b"data":
            raw = body
        pos += 8 + size + (size & 1)
    if fmt is None or raw is None:
        raise ValueError(f"{path}: no fmt or data chunk")
    tag, ch, rate, bits = fmt
    if tag == 3 and bits == 32:
        x = np.frombuffer(raw, "<f4")
    elif tag == 1 and bits == 16:
        x = np.frombuffer(raw, "<i2") / 32768.0
    elif tag == 1 and bits == 24:
        b = np.frombuffer(raw, np.uint8).reshape(-1, 3).astype(np.int32)
        v = b[:, 0] | (b[:, 1] << 8) | (b[:, 2] << 16)
        x = np.where(v >= 1 << 23, v - (1 << 24), v) / float(1 << 23)
    elif tag == 1 and bits == 32:
        x = np.frombuffer(raw, "<i4") / float(1 << 31)
    else:
        raise ValueError(f"{path}: unsupported WAV format {tag}/{bits}-bit")
    x = x[: len(x) // ch * ch]
    return np.ascontiguousarray(x.reshape(-1, ch).T, dtype=np.float32), rate


def write_wav16(path: str, audio: np.ndarray, rate: int) -> None:
    """Writes (channels, frames) as 16-bit PCM, for listening."""
    pcm = (np.clip(audio, -1, 1) * 32767).round().astype("<i2").T.tobytes()
    with wave.open(path, "wb") as w:
        w.setnchannels(audio.shape[0])
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(pcm)
