"""Analyse an audio file, or compare a candidate render against a reference sound.

    python -m serum_host.analyze <audio> --out <dir> [--start S] [--end S]
    python -m serum_host.analyze <reference> --compare <candidate> --out <dir> [--start S] [--end S]

Reads WAV/FLAC/AIFF/MP3 (via pedalboard), resampled to 44.1 kHz stereo. --start/--end trim
the reference (and, in compare mode, the candidate is trimmed to the same length).

stdout carries exactly one JSON object; logs go to stderr. Exit codes: 0 ok, 1 bad input,
2 runtime failure. Single mode returns the same analysis as render.py plus `png`. Compare mode
returns `reference`, `candidate` (analyses), `diff` (candidate minus reference) and `png`
(both spectrograms plus overlaid average spectra).
"""
import argparse
import json
import math
import os
import sys

import numpy as np

from .render import SR, InputError, _Parser, _mono, analyze, save_spectrogram

MAX_SECONDS = 60.0
FLOOR = 1e-10


def load_audio(path, start=0.0, end=None):
    """-> float32 array (2, n) at 44.1 kHz."""
    if not os.path.isfile(path):
        raise InputError(f"audio file not found: {path}")
    from pedalboard.io import AudioFile
    try:
        with AudioFile(path).resampled_to(SR) as f:
            x = f.read(f.frames)
    except Exception as e:  # noqa: BLE001 - surface decoder errors as bad input
        raise InputError(f"cannot read {path}: {e}")
    x = np.asarray(x, dtype=np.float32)
    if x.ndim == 1:
        x = x[None, :]
    x = np.vstack([x, x]) if x.shape[0] == 1 else x[:2]
    a = int(max(0.0, start) * SR)
    b = x.shape[1] if end is None else int(end * SR)
    x = x[:, a:b]
    if x.shape[1] < SR // 20:
        raise InputError("selected audio is shorter than 50 ms")
    return x[:, : int(MAX_SECONDS * SR)]


# --------------------------------------------------------------------------- comparison
def third_octave_levels(audio, sr=SR):
    """Average spectrum in 1/3-octave bands (25 Hz .. 16 kHz), dB relative to the total."""
    x = _mono(audio).astype(np.float64)
    n = 8192
    if x.size < n:
        x = np.pad(x, (0, n - x.size))
    frames = np.lib.stride_tricks.sliding_window_view(x, n)[:: n // 2] * np.hanning(n)
    power = np.mean(np.abs(np.fft.rfft(frames, axis=1)) ** 2, axis=0)
    freqs = np.fft.rfftfreq(n, 1 / sr)
    centers = [1000 * 2 ** (k / 3) for k in range(-16, 13)]  # 25 Hz .. 16 kHz
    total = float(power[(freqs >= 20) & (freqs <= 20000)].sum()) + FLOOR
    out = []
    for c in centers:
        lo, hi = c / 2 ** (1 / 6), c * 2 ** (1 / 6)
        p = float(power[(freqs >= lo) & (freqs < hi)].sum())
        out.append((round(c, 1), 10 * math.log10(p / total + FLOOR)))
    return out


def envelope_shape(env_db):
    """Attack (time to within 3 dB of peak) and decay (peak to -20 dB) from 50 ms RMS frames."""
    e = np.asarray(env_db, dtype=np.float64)
    if e.size == 0:
        return None, None
    peak_i = int(np.argmax(e))
    peak = e[peak_i]
    attack_i = int(np.argmax(e >= peak - 3))
    after = np.nonzero(e[peak_i:] <= peak - 20)[0]
    decay = None if after.size == 0 else round(after[0] * 0.05, 2)
    return round(attack_i * 0.05, 2), decay


def compare(ref, cand, ref_audio, cand_audio):
    d = {}
    if ref["lufsIntegrated"] is not None and cand["lufsIntegrated"] is not None:
        d["loudnessLu"] = round(cand["lufsIntegrated"] - ref["lufsIntegrated"], 2)
    if ref["spectralCentroidHz"] and cand["spectralCentroidHz"]:
        d["brightnessOctaves"] = round(math.log2(cand["spectralCentroidHz"] / ref["spectralCentroidHz"]), 2)
    if ref["fundamentalHz"] and cand["fundamentalHz"]:
        d["pitchSemitones"] = round(12 * math.log2(cand["fundamentalHz"] / ref["fundamentalHz"]), 2)
    d["stereoWidth"] = round(cand["stereoWidth"] - ref["stereoWidth"], 3)

    # Tonal balance independent of overall level: each band relative to the file's RMS.
    d["bandBalanceDb"] = {
        k: round((cand["bands"][k] - cand["rmsDbfs"]) - (ref["bands"][k] - ref["rmsDbfs"]), 1) for k in ref["bands"]
    }
    r3, c3 = third_octave_levels(ref_audio), third_octave_levels(cand_audio)
    diffs = [(f, c - r) for (f, r), (_, c) in zip(r3, c3) if r > -60 or c > -60]
    d["spectralDistanceDb"] = round(math.sqrt(sum(x * x for _, x in diffs) / max(len(diffs), 1)), 1)
    # Perceptual-ish distance: bands weighted by compressed energy (so near-silent bands barely
    # count) and differences capped at 24 dB.
    rp = np.array([10 ** (v / 10) for _, v in r3])
    cp = np.array([10 ** (v / 10) for _, v in c3])
    w = np.maximum(rp, cp) ** 0.3
    w /= w.sum()
    delta = np.clip(np.array([c - r for (_, r), (_, c) in zip(r3, c3)]), -24, 24)
    d["weightedSpectralDistanceDb"] = round(math.sqrt(float((w * delta**2).sum())), 1)
    d["largestSpectralDifferences"] = [
        {"hz": f, "db": round(x, 1)} for f, x in sorted(diffs, key=lambda t: -abs(t[1]))[:5]
    ]

    re, ce = ref["envelope"], cand["envelope"]
    m = min(len(re), len(ce))
    if m >= 3:
        a, b = np.asarray(re[:m]) - max(re[:m]), np.asarray(ce[:m]) - max(ce[:m])
        a, b = np.maximum(a, -60), np.maximum(b, -60)
        if a.std() > 0 and b.std() > 0:
            d["envelopeCorrelation"] = round(float(np.corrcoef(a, b)[0, 1]), 3)
    ra, rd = envelope_shape(re)
    ca, cd = envelope_shape(ce)
    d["attackS"] = {"reference": ra, "candidate": ca}
    d["decayTo-20dBS"] = {"reference": rd, "candidate": cd}

    hints = []
    if abs(d.get("pitchSemitones", 0)) > 0.5:
        hints.append(f"pitch off by {d['pitchSemitones']:+.1f} semitones (check note / octave / coarse)")
    if abs(d.get("brightnessOctaves", 0)) > 0.3:
        hints.append("candidate is " + ("brighter" if d["brightnessOctaves"] > 0 else "darker") + " (filter cutoff / wavetable / drive)")
    for k, v in d["bandBalanceDb"].items():
        if abs(v) > 6:
            hints.append(f"{k} band {'+' if v > 0 else ''}{v} dB vs reference")
    if abs(d["stereoWidth"]) > 0.2:
        hints.append("candidate is " + ("wider" if d["stereoWidth"] > 0 else "narrower") + " (unison width / chorus / reverb)")
    if d.get("envelopeCorrelation") is not None and d["envelopeCorrelation"] < 0.7:
        hints.append("loudness shape over time differs (envelopes / LFO movement)")
    d["hints"] = hints
    # 0..100: weighted spectral distance dominates (12 dB -> ~37%); envelope shape scales it.
    corr = d.get("envelopeCorrelation", 1.0)
    score = 100 * math.exp(-d["weightedSpectralDistanceDb"] / 12) * (0.7 + 0.3 * max(corr, 0.0))
    d["similarity"] = int(max(0, min(100, round(score))))
    return d, r3, c3


def save_comparison(ref_audio, cand_audio, r3, c3, path, sr=SR):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from scipy.signal import stft

    fig, axes = plt.subplots(3, 1, figsize=(8, 9), dpi=100)
    for ax, audio, title in ((axes[0], ref_audio, "Reference"), (axes[1], cand_audio, "Candidate")):
        x = _mono(audio).astype(np.float64)
        f, t, z = stft(x, fs=sr, window="hann", nperseg=4096, noverlap=4096 - 512, boundary=None, padded=False)
        db = 20 * np.log10(np.maximum(2 * np.abs(z), FLOOR))
        vmax = max(float(db.max()), -20.0)
        keep = f >= 20
        ax.pcolormesh(t, f[keep], db[keep], shading="auto", cmap="magma", vmin=vmax - 90, vmax=vmax)
        ax.set_yscale("log")
        ax.set_ylim(20, 20000)
        ax.set_title(title)
        ax.set_ylabel("Hz")
    axes[2].semilogx([f for f, _ in r3], [v for _, v in r3], label="reference", lw=2)
    axes[2].semilogx([f for f, _ in c3], [v for _, v in c3], label="candidate", lw=2, ls="--")
    axes[2].set_xlim(20, 20000)
    axes[2].set_ylim(-70, 0)
    axes[2].set_xlabel("Frequency (Hz)")
    axes[2].set_ylabel("dB rel. total")
    axes[2].set_title("Average spectrum (1/3 octave, level-normalized)")
    axes[2].grid(True, which="both", alpha=0.3)
    axes[2].legend()
    fig.tight_layout()
    fig.savefig(path)
    plt.close(fig)


# --------------------------------------------------------------------------- CLI
def run(argv):
    ap = _Parser(prog="serum_host.analyze", description=__doc__)
    ap.add_argument("audio")
    ap.add_argument("--out", required=True)
    ap.add_argument("--compare")
    ap.add_argument("--start", type=float, default=0.0)
    ap.add_argument("--end", type=float, default=None)
    a = ap.parse_args(argv)
    if a.end is not None and a.end <= a.start:
        raise InputError("--end must be after --start")
    out = os.path.abspath(a.out)
    os.makedirs(out, exist_ok=True)

    ref_audio = load_audio(a.audio, a.start, a.end)
    ref = analyze(ref_audio, SR)
    if not a.compare:
        png = os.path.join(out, "spectrogram.png")
        save_spectrogram(ref_audio, png)
        return {"file": os.path.abspath(a.audio), "png": png, **ref}

    cand_audio = load_audio(a.compare)[:, : ref_audio.shape[1]]
    cand = analyze(cand_audio, SR)
    diff, r3, c3 = compare(ref, cand, ref_audio, cand_audio)
    png = os.path.join(out, "comparison.png")
    save_comparison(ref_audio, cand_audio, r3, c3, png)
    strip = lambda r: {k: v for k, v in r.items() if k != "envelope"}  # noqa: E731
    return {"png": png, "reference": strip(ref), "candidate": strip(cand), "diff": diff}


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    real_out = os.dup(1)
    os.dup2(2, 1)

    def emit(obj):
        os.write(real_out, (json.dumps(obj, allow_nan=False) + "\n").encode())

    try:
        emit(run(argv))
        code = 0
    except InputError as e:
        emit({"error": str(e)})
        code = 1
    except SystemExit as e:
        code = e.code if isinstance(e.code, int) else 0
    except Exception as e:  # noqa: BLE001
        emit({"error": f"{type(e).__name__}: {e}"})
        code = 2
    sys.stderr.flush()
    os._exit(code)


if __name__ == "__main__":
    main()
