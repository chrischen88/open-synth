"""Render a .SerumPreset through the installed Serum 2 and analyse the audio.

    python -m serum_host.render <preset> --out <dir> [--notes "48:0:1.5,55:0:1.5"]
        [--velocity 100] [--duration 2.5] [--macros "0:50,1:20"]

stdout carries exactly one JSON object (the result, or ``{"error": "..."}``); all logs go to stderr.
Exit codes: 0 ok, 1 bad input, 2 runtime failure / timeout.
"""
import argparse
import json
import math
import os
import sys
import threading
import time

import numpy as np

SR = 44100
TIMEOUT_S = 90
DEFAULT_NOTES = "48:0:1.5"
MIN_DURATION = 2.5
FLOOR_DB = -120.0
SILENT_DBFS = -60.0


class InputError(ValueError):
    pass


# --------------------------------------------------------------------------- parsing
def parse_notes(spec):
    """'48:0:1.5,55:0:1.5' -> [(48, 0.0, 1.5), (55, 0.0, 1.5)]"""
    notes = []
    for part in (spec or "").split(","):
        part = part.strip()
        if not part:
            continue
        f = part.split(":")
        if len(f) != 3:
            raise InputError(f"bad note '{part}': expected midi:start_s:length_s")
        try:
            midi, start, length = int(f[0]), float(f[1]), float(f[2])
        except ValueError:
            raise InputError(f"bad note '{part}': non-numeric field")
        if not 0 <= midi <= 127:
            raise InputError(f"bad note '{part}': midi must be 0..127")
        if not (math.isfinite(start) and math.isfinite(length)) or start < 0 or length <= 0:
            raise InputError(f"bad note '{part}': need start>=0 and length>0")
        notes.append((midi, start, length))
    if not notes:
        raise InputError("no notes given")
    return notes


def parse_macros(spec):
    """'0:50,1:20' -> [(0, 0.5), (1, 0.2)]  (index 0..7 -> host macro_{index+1}, raw = pct/100)"""
    out = []
    for part in (spec or "").split(","):
        part = part.strip()
        if not part:
            continue
        f = part.split(":")
        if len(f) != 2:
            raise InputError(f"bad macro '{part}': expected index:percent")
        try:
            idx, pct = int(f[0]), float(f[1])
        except ValueError:
            raise InputError(f"bad macro '{part}': non-numeric field")
        if not 0 <= idx <= 7:
            raise InputError(f"bad macro '{part}': index must be 0..7")
        if not (math.isfinite(pct) and 0 <= pct <= 100):
            raise InputError(f"bad macro '{part}': percent must be 0..100")
        out.append((idx, pct / 100.0))
    return out


# --------------------------------------------------------------------------- analysis
def _db(x):
    return float(max(FLOOR_DB, 20.0 * math.log10(x))) if x > 0 else FLOOR_DB


def _mono(audio):
    return np.mean(audio, axis=0)


def peak_dbfs(audio):
    return _db(float(np.max(np.abs(audio)))) if audio.size else FLOOR_DB


def rms_dbfs(audio):
    return _db(float(np.sqrt(np.mean(audio.astype(np.float64) ** 2)))) if audio.size else FLOOR_DB


def _shelf_hp_coeffs(sr):
    """BS.1770 K-weighting biquads (high shelf + RLB high-pass) for any sample rate."""
    f0, G, Q = 1681.974450955533, 3.999843853973347, 0.7071752369554196
    K = math.tan(math.pi * f0 / sr)
    Vh = 10 ** (G / 20.0)
    Vb = Vh ** 0.4996667741545416
    a0 = 1.0 + K / Q + K * K
    b1 = [(Vh + Vb * K / Q + K * K) / a0, 2.0 * (K * K - Vh) / a0, (Vh - Vb * K / Q + K * K) / a0]
    a1 = [1.0, 2.0 * (K * K - 1.0) / a0, (1.0 - K / Q + K * K) / a0]
    f0, Q = 38.13547087602444, 0.5003270373238773
    K = math.tan(math.pi * f0 / sr)
    a0 = 1.0 + K / Q + K * K
    b2 = [1.0, -2.0, 1.0]
    a2 = [1.0, 2.0 * (K * K - 1.0) / a0, (1.0 - K / Q + K * K) / a0]
    return (b1, a1), (b2, a2)


def lufs_integrated(audio, sr=SR):
    """ITU-R BS.1770-4 integrated loudness: K-weighting, 400 ms blocks / 75% overlap,
    absolute gate -70 LUFS, relative gate -10 LU. L/R weighted 1.0. Returns None if no block passes
    the gate (silence or audio shorter than 400 ms; short audio is zero-padded to one block)."""
    from scipy.signal import lfilter
    x = np.atleast_2d(audio).astype(np.float64)
    (b1, a1), (b2, a2) = _shelf_hp_coeffs(sr)
    y = np.stack([lfilter(b2, a2, lfilter(b1, a1, ch)) for ch in x])
    blk, hop = int(round(0.4 * sr)), int(round(0.1 * sr))
    if y.shape[1] < blk:
        y = np.pad(y, ((0, 0), (0, blk - y.shape[1])))
    n = (y.shape[1] - blk) // hop + 1
    z = np.array([np.sum(np.mean(y[:, i * hop:i * hop + blk] ** 2, axis=1)) for i in range(n)])
    with np.errstate(divide="ignore"):
        l = -0.691 + 10 * np.log10(z)
    z1 = z[l > -70.0]
    if z1.size == 0:
        return None
    gate = -0.691 + 10 * math.log10(float(np.mean(z1))) - 10.0
    z2 = z[l > gate]
    if z2.size == 0:
        return None
    return float(-0.691 + 10 * math.log10(float(np.mean(z2))))


def spectral_centroid_hz(audio, sr=SR, n_fft=2048, hop=512):
    """Mean power-weighted centroid over STFT frames whose RMS exceeds -60 dBFS.

    Power (not magnitude) weighting keeps a flat noise floor (16-bit files, recordings) from
    dragging the centroid of quiet or bass-heavy sounds upward."""
    x = _mono(audio).astype(np.float64)
    if x.size < n_fft:
        x = np.pad(x, (0, n_fft - x.size))
    win = np.hanning(n_fft)
    freqs = np.fft.rfftfreq(n_fft, 1.0 / sr)
    vals = []
    for s in range(0, x.size - n_fft + 1, hop):
        fr = x[s:s + n_fft]
        if math.sqrt(float(np.mean(fr ** 2))) < 10 ** (SILENT_DBFS / 20):
            continue
        power = np.abs(np.fft.rfft(fr * win)) ** 2
        t = power.sum()
        if t > 0:
            vals.append(float((freqs * power).sum() / t))
    return float(np.mean(vals)) if vals else None


def fundamental_hz(audio, start_s=0.0, sr=SR, fmin=20.0, fmax=2000.0, clarity=0.5):
    """Autocorrelation pitch estimate on a window just after note start (skips the attack).
    Picks the shortest lag whose normalised ACF is within 90% of the global max (avoids octave-down
    errors) and refines with parabolic interpolation. None if the periodicity is weak (<0.5)."""
    x = _mono(audio).astype(np.float64)
    s0 = int((start_s + 0.05) * sr)
    seg = x[s0:s0 + 16384]
    if seg.size < 4096:
        seg = x[int(start_s * sr):int(start_s * sr) + 16384]
    if seg.size < 2048 or math.sqrt(float(np.mean(seg ** 2))) < 10 ** (SILENT_DBFS / 20):
        return None
    seg = (seg - seg.mean()) * np.hanning(seg.size)
    n = 1 << (2 * seg.size - 1).bit_length()
    f = np.fft.rfft(seg, n)
    acf = np.fft.irfft(f * np.conj(f), n)[:seg.size]
    if acf[0] <= 0:
        return None
    acf = acf / acf[0]
    lo, hi = int(sr / fmax), min(int(sr / fmin), seg.size - 2)
    if hi <= lo + 2:
        return None
    r = acf[lo:hi]
    # local maxima only
    pk = np.where((r[1:-1] > r[:-2]) & (r[1:-1] >= r[2:]))[0] + 1
    if pk.size == 0:
        return None
    best = r[pk].max()
    if best < clarity:
        return None
    i = pk[np.argmax(r[pk] >= 0.9 * best)] + lo
    a, b, c = acf[i - 1], acf[i], acf[i + 1]
    d = a - 2 * b + c
    off = 0.5 * (a - c) / d if d != 0 else 0.0
    return float(sr / (i + off))


def stereo_width(audio):
    """RMS(side)/RMS(mid) with mid=(L+R)/2, side=(L-R)/2. 0 for mono/silence."""
    if audio.shape[0] < 2:
        return 0.0
    m = (audio[0] + audio[1]) / 2.0
    s = (audio[0] - audio[1]) / 2.0
    mr = math.sqrt(float(np.mean(m ** 2)))
    sr_ = math.sqrt(float(np.mean(s ** 2)))
    return float(sr_ / mr) if mr > 1e-9 else 0.0


BANDS = (("sub", 0, 60), ("low", 60, 250), ("lowmid", 250, 2000), ("highmid", 2000, 6000), ("high", 6000, None))


def band_levels(audio, sr=SR):
    """RMS (dBFS) of the mono mix per band, from the whole-signal spectrum (Parseval)."""
    x = _mono(audio).astype(np.float64)
    n = x.size
    X = np.fft.rfft(x)
    p = np.abs(X) ** 2
    p[1:] *= 2
    if n % 2 == 0:
        p[-1] /= 2
    freqs = np.fft.rfftfreq(n, 1.0 / sr)
    out = {}
    for name, lo, hi in BANDS:
        sel = (freqs >= lo) & ((freqs < hi) if hi else True)
        out[name] = round(_db(math.sqrt(float(p[sel].sum())) / n), 1)
    return out


def envelope_db(audio, sr=SR, win_s=0.05):
    x = _mono(audio).astype(np.float64)
    w = int(round(win_s * sr))
    return [round(_db(math.sqrt(float(np.mean(x[i:i + w] ** 2)))), 1) for i in range(0, x.size, w) if x[i:i + w].size]


def analyze(audio, sr=SR, first_note_start=0.0):
    pk = peak_dbfs(audio)
    lufs = lufs_integrated(audio, sr)
    cen = spectral_centroid_hz(audio, sr)
    f0 = fundamental_hz(audio, first_note_start, sr)
    return {
        "durationS": round(audio.shape[1] / sr, 3),
        "peakDbfs": round(pk, 2),
        "rmsDbfs": round(rms_dbfs(audio), 2),
        "lufsIntegrated": None if lufs is None else round(lufs, 2),
        "spectralCentroidHz": None if cen is None else round(cen, 1),
        "fundamentalHz": None if f0 is None else round(f0, 2),
        "stereoWidth": round(stereo_width(audio), 4),
        "silent": bool(pk < SILENT_DBFS),
        "clipping": bool(float(np.max(np.abs(audio))) >= 0.999) if audio.size else False,
        "bands": band_levels(audio, sr),
        "envelope": envelope_db(audio, sr),
    }


def save_spectrogram(audio, path, sr=SR):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.ticker import FuncFormatter, FixedLocator
    from scipy.signal import stft
    x = _mono(audio).astype(np.float64)
    f, t, Z = stft(x, fs=sr, window="hann", nperseg=4096, noverlap=4096 - 512, boundary=None, padded=False)
    mag = 2.0 * np.abs(Z)  # scipy already divides by sum(window): sine of amplitude A -> A
    db = 20 * np.log10(np.maximum(mag, 1e-10))
    vmax = max(float(db.max()), -20.0)
    keep = f >= 20
    fig, ax = plt.subplots(figsize=(8, 4), dpi=100)
    im = ax.pcolormesh(t, f[keep], db[keep], shading="auto", cmap="magma", vmin=vmax - 90, vmax=vmax)
    ax.set_yscale("log")
    ax.set_ylim(20, min(20000, sr / 2))
    ticks = [t_ for t_ in (20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000) if t_ <= sr / 2]
    ax.yaxis.set_major_locator(FixedLocator(ticks))
    ax.yaxis.set_minor_locator(FixedLocator([]))
    ax.yaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v / 1000:g}k" if v >= 1000 else f"{v:g}"))
    ax.set_xlim(0, x.size / sr)
    ax.set_xlabel("Time (s)")
    ax.set_ylabel("Frequency (Hz)")
    ax.set_title("Spectrogram")
    fig.colorbar(im, ax=ax, label="dBFS")
    fig.tight_layout()
    fig.savefig(path)
    plt.close(fig)


# --------------------------------------------------------------------------- serum
SERUM_DISPLAYS = ("filter_1_freq_hz", "filter_1_type", "a_level", "env_1_attack")


def render_with_serum(preset, notes, velocity, duration, macros, serum=None):
    """Render with a fresh Serum, or reuse `serum` (reset to its baseline first; renders are
    bit-identical to a fresh instance, verified on reverb/delay-heavy presets)."""
    from . import state
    from .host import Serum
    t0 = time.time()
    if serum is None:
        s = Serum()
    else:
        s = serum
        s.reset()
    t_load = time.time() - t0
    s.inject_preset(preset)
    for idx, raw in macros:
        s.set(f"macro_{idx + 1}", raw)
    if macros:
        s.settle(0.3)
    t1 = time.time()
    audio = np.asarray(s.render_notes(notes, velocity, duration), dtype=np.float32)
    t_render = time.time() - t1
    info = {"presetName": (state.decode_raw_state(s.plugin.raw_state)[3] or {}).get("presetName")}
    for name in SERUM_DISPLAYS:
        try:
            info[name] = s.display(name).strip()
        except Exception as e:  # noqa: BLE001
            info[name] = None
            log(f"display {name} failed: {e}")
    log(f"timing: load={t_load:.1f}s inject+render={t_render:.1f}s")
    return audio, info, {"loadS": round(t_load, 2), "renderS": round(t_render, 2)}


def log(*a):
    print(*a, file=sys.stderr, flush=True)


class _Parser(argparse.ArgumentParser):
    def error(self, message):
        raise InputError(message)


def render_job(preset, out, notes_spec=DEFAULT_NOTES, velocity=100, duration=None, macros_spec="", serum=None):
    """Validate inputs, render `preset`, write render.wav + spectrogram.png into `out`, analyse."""
    if not os.path.isfile(preset):
        raise InputError(f"preset not found: {preset}")
    if not 1 <= int(velocity) <= 127:
        raise InputError("velocity must be 1..127")
    notes = parse_notes(notes_spec)
    macros = parse_macros(macros_spec)
    end = max(s + l for _, s, l in notes)
    duration = duration if duration is not None else max(MIN_DURATION, end + 0.5)
    if not (math.isfinite(duration) and 0 < duration <= 60):
        raise InputError("duration must be in (0, 60]")
    out = os.path.abspath(out)
    try:
        os.makedirs(out, exist_ok=True)
    except OSError as e:
        raise InputError(f"cannot create out dir: {e}")

    audio, info, timing = render_with_serum(os.path.abspath(preset), notes, int(velocity), duration, macros, serum)
    from .host import write_wav
    wav, png = os.path.join(out, "render.wav"), os.path.join(out, "spectrogram.png")
    write_wav(wav, audio)
    save_spectrogram(audio, png)
    res = {"wav": wav, "png": png}
    res.update(analyze(audio, SR, notes[0][1]))
    res["serum"] = info
    res["timing"] = timing
    return res


def run(argv):
    ap = _Parser(prog="serum_host.render", description=__doc__)
    ap.add_argument("preset")
    ap.add_argument("--out", required=True)
    ap.add_argument("--notes", default=DEFAULT_NOTES)
    ap.add_argument("--velocity", type=int, default=100)
    ap.add_argument("--duration", type=float, default=None)
    ap.add_argument("--macros", default="")
    a = ap.parse_args(argv)
    return render_job(a.preset, a.out, a.notes, a.velocity, a.duration, a.macros)


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    real_out = os.dup(1)
    os.dup2(2, 1)  # anything native code prints to stdout goes to stderr

    def emit(obj):
        os.write(real_out, (json.dumps(obj, allow_nan=False) + "\n").encode())

    def watchdog():
        emit({"error": f"timeout: Serum load+render exceeded {TIMEOUT_S}s"})
        os._exit(2)

    timer = threading.Timer(TIMEOUT_S, watchdog)
    timer.daemon = True
    timer.start()
    try:
        emit(run(argv))
        code = 0
    except InputError as e:
        emit({"error": str(e)})
        code = 1
    except SystemExit as e:  # argparse --help
        code = e.code if isinstance(e.code, int) else 0
    except Exception as e:  # noqa: BLE001
        emit({"error": f"{type(e).__name__}: {e}"})
        code = 2
    timer.cancel()
    sys.stderr.flush()
    os._exit(code)  # skip slow plugin teardown / hangs


if __name__ == "__main__":
    main()
