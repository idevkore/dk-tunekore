"""Synthesises a short test song with known stems, for SDR scores we own.

Everything is generated here from oscillators and noise: drums, a bass line,
chords and arpeggio ("other"), and a formant-synthesised sung vowel line
("vocals"). No samples or recordings, so the stems are ours to use however we
like. It's a sanity benchmark, not a substitute for real multitracks: the
sounds are simpler than real instruments and voices.

    python tools/make_test_song.py OUT_DIR [--bpm 120] [--seconds 60] [--seed 1] [--key 57]

Writes OUT_DIR/mix.wav and OUT_DIR/stems/{vocals,drums,bass,other}.wav
(44.1 kHz, 16-bit stereo; the stems sum to the mix).
"""

from __future__ import annotations

import argparse
import os
import sys

import numpy as np
from scipy.signal import butter, lfilter

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from tunekore_stems.rawio import write_wav16  # noqa: E402

SR = 44_100
MINOR = [0, 2, 3, 5, 7, 8, 10]


def midi_hz(n: float) -> float:
    return 440.0 * 2 ** ((n - 69) / 12)


def env(n: int, attack: float, decay: float) -> np.ndarray:
    t = np.arange(n) / SR
    a = np.clip(t / max(attack, 1e-4), 0, 1)
    return a * np.exp(-t / decay)


def saw(freq: np.ndarray | float, n: int, phase0: float = 0.0) -> np.ndarray:
    f = np.broadcast_to(np.asarray(freq, dtype=np.float64), (n,))
    ph = phase0 + np.cumsum(f) / SR
    return 2 * (ph % 1.0) - 1


def lowpass(x, hz, order=2):
    b, a = butter(order, hz / (SR / 2), "low")
    return lfilter(b, a, x)


def highpass(x, hz, order=2):
    b, a = butter(order, hz / (SR / 2), "high")
    return lfilter(b, a, x)


def bandpass(x, lo, hi, order=2):
    b, a = butter(order, [lo / (SR / 2), hi / (SR / 2)], "band")
    return lfilter(b, a, x)


def place(track: np.ndarray, sound: np.ndarray, at: int, pan: float = 0.0) -> None:
    """Adds a mono sound to a stereo track at sample `at`, panned -1..1."""
    end = min(track.shape[1], at + len(sound))
    if end <= at:
        return
    g = np.array([np.cos((pan + 1) * np.pi / 4), np.sin((pan + 1) * np.pi / 4)]) * np.sqrt(2)
    track[:, at:end] += g[:, None] * sound[: end - at]


def make(bpm: float, seconds: float, seed: int, key: int):
    rng = np.random.default_rng(seed)
    n = int(seconds * SR)
    beat = 60 / bpm
    step = int(beat / 2 * SR)  # eighth notes
    stems = {k: np.zeros((2, n)) for k in ("vocals", "drums", "bass", "other")}
    progression = [0, 5, 2, 6] if seed % 2 else [0, 3, 5, 4]  # scale degrees per bar
    bar = int(4 * beat * SR)

    # Drums
    kick_n = int(0.35 * SR)
    t = np.arange(kick_n) / SR
    kick = np.sin(2 * np.pi * np.cumsum(50 + 110 * np.exp(-t * 30)) / SR) * env(kick_n, 0.001, 0.12)
    snare_n = int(0.25 * SR)
    snare = (bandpass(rng.standard_normal(snare_n), 1500, 8000) * 0.8 + np.sin(2 * np.pi * 190 * np.arange(snare_n) / SR) * 0.4) * env(snare_n, 0.001, 0.07)
    hat_n = int(0.06 * SR)
    for i, at in enumerate(range(0, n, step)):
        pos = i % 8
        if pos in (0, 4) or (pos == 6 and rng.random() < 0.3):
            place(stems["drums"], kick * 0.9, at)
        if pos in (2, 6):
            place(stems["drums"], snare * 0.6, at, 0.05)
        hat = highpass(rng.standard_normal(hat_n), 7000) * env(hat_n, 0.0005, 0.015 if pos % 2 else 0.03)
        place(stems["drums"], hat * 0.25, at, 0.35)

    # Bass: chord roots in eighths, with an octave jump now and then
    for i, at in enumerate(range(0, n, step)):
        degree = progression[(at // bar) % 4]
        root = key - 24 + MINOR[degree % 7]
        note = root + (12 if i % 8 == 7 else 0)
        m = int(step * 0.9)
        tone = lowpass(saw(midi_hz(note), m), 900) * env(m, 0.004, 0.25)
        place(stems["bass"], tone * 0.55, at)

    # Other: a pad holding each bar's chord, plus a plucked arpeggio
    for b0 in range(0, n, bar):
        degree = progression[(b0 // bar) % 4]
        chord = [key + MINOR[(degree + k) % 7] + 12 * ((degree + k) // 7) for k in (0, 2, 4)]
        m = min(bar, n - b0)
        pad = sum(saw(midi_hz(c) * d, m, rng.random()) for c in chord for d in (0.997, 1.003))
        pad = lowpass(pad, 2200) * env(m, 0.15, 3.0) * 0.06
        place(stems["other"], pad, b0, -0.3)
        for k in range(8):
            at = b0 + k * step
            if at >= n:
                break
            c = chord[k % 3] + 12
            p = int(step * 0.8)
            pluck = lowpass(saw(midi_hz(c), p), 3500) * env(p, 0.002, 0.09) * 0.12
            place(stems["other"], pluck, at, 0.45)

    # Vocals: a sung vowel line through formant filters, with vibrato; phrases with rests
    formants = {"a": [(800, 80), (1150, 90), (2900, 120)], "o": [(450, 70), (800, 80), (2830, 100)],
                "e": [(400, 60), (1600, 80), (2700, 120)]}
    beat_n = int(beat * SR)
    melody_steps = [0, 2, 4, 2, 0, -1, 0, 4, 5, 4, 2, 0]
    i = 0
    for phrase_start in range(bar, n, bar * 2):  # sing every other bar, starting at bar 2
        for k in range(6):
            at = phrase_start + k * beat_n
            if at >= n:
                break
            m = int(beat_n * (1.6 if k == 5 else 0.95))
            deg = melody_steps[i % len(melody_steps)]
            i += 1
            note = key + 12 + MINOR[deg % 7] + 12 * (deg // 7)
            tt = np.arange(m) / SR
            f0 = midi_hz(note) * (1 + 0.012 * np.sin(2 * np.pi * 5.5 * tt) * np.clip(tt / 0.3, 0, 1))
            src = saw(f0, m) + 0.02 * rng.standard_normal(m)  # breath
            vowel = formants["aoe"[k % 3]]
            sung = sum(bandpass(src, max(60, f - bw), f + bw, 2) * g for (f, bw), g in zip(vowel, (1.0, 0.6, 0.3)))
            sung *= env(m, 0.04, 0.9) * 0.9
            place(stems["vocals"], sung, at, 0.0)

    peak = np.abs(sum(stems.values())).max()
    scale = 0.89 / peak
    return {k: (v * scale).astype(np.float32) for k, v in stems.items()}


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--bpm", type=float, default=120)
    ap.add_argument("--seconds", type=float, default=60)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--key", type=int, default=57, help="MIDI note of the tonic (57 = A3)")
    a = ap.parse_args()
    stems = make(a.bpm, a.seconds, a.seed, a.key)
    os.makedirs(os.path.join(a.out, "stems"), exist_ok=True)
    # Quantise the stems first, then sum the quantised stems: the mix is then
    # exactly the sum of the files.
    q = {k: np.round(np.clip(v, -1, 1) * 32767) / 32767 for k, v in stems.items()}
    for k, v in q.items():
        write_wav16(os.path.join(a.out, "stems", f"{k}.wav"), v, SR)
    write_wav16(os.path.join(a.out, "mix.wav"), sum(q.values()), SR)
    print(f"wrote {a.out}: {a.seconds:.0f} s at {a.bpm:.0f} BPM")


if __name__ == "__main__":
    main()
