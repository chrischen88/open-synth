"""Sound-search index over a Serum 2 preset library: render every preset, store audio features,
find the presets that sound most like a reference audio file.

    python -m serum_host.index build [--library DIR] [--index PATH] [--workers N] [--limit N]
    python -m serum_host.index query <audio> [--start S] [--end S] [--top 10] [--tags a,b] [--index PATH]
    (--tags matches browser tags, library folder names and name-prefix categories, e.g. "bass", "pad", "Factory")

Index path: --index, else $OSYN_INDEX, else ~/Library/Application Support/open-synth/preset_index.json.
Build is resumable (results are appended to ``<index>.cache.jsonl``), writes ``<index>.progress.json``
({done,total,failed,startedAt,updatedAt,etaS}) and replaces the index atomically.

Design
------
Every preset is rendered at MIDI 36, 48 and 60 (one 1.0 s note, 2.0 s render, velocity 100), each from a
reset Serum, so pitch-dependent behaviour (filter key-tracking, formant-like filters, wavetable
position vs. note) is captured by real renders rather than extrapolated.  Per render we keep:
level-normalised 1/3-octave spectrum (29 bands, dB re total), spectral centroid, 5 band levels relative to
RMS, measured fundamental (f0), spectral flatness, stereo width, RMS/LUFS, a 20-point envelope (100 ms RMS,
dB re peak, floor -60) and an 8-slice brightness curve (centroid per 250 ms).

Query: the reference is trimmed to its first onset, cropped to 2 s and analysed the same way.  Every preset is
scored at each of its three rendered notes and the best note is kept (reported as `note`).  Pitch independence
comes from two things: (1) the render nearest in pitch exists for every preset, so an octave-shifted copy finds the
render made at that pitch with its real filter/key-tracking behaviour; (2) the remaining gap (up to ~6 semitones,
which still moves harmonics across 1/3-octave bands) is absorbed by also trying the candidate spectrum shifted by
the residual pitch (f0 ratio, power-domain interpolation, shifts of 0/25/50/75/100 %), keeping the best per preset.
Choosing the note by "closest measured f0" alone was tried first; the autocorrelation f0 is often an octave or two
off for sub-heavy/detuned patches, and scoring all notes was clearly better on held-out renders (top-1 74% vs 63%).
Similarity (0..100) follows analyze.compare_analyses: 100 * exp(-D/8) * shape * penalties, with D the
energy-weighted 1/3-octave distance (weights loudest^0.15, bands >50 dB down ignored, diffs capped at 8 dB),
shape = 0.3 + 0.4*envelope correlation + 0.3*brightness-over-time agreement, and penalties for stereo-width and
noisiness differences.  Constants live in ``TUNE`` and were tuned on ~60 held-out renders at notes not in the index.

stdout carries exactly one JSON object; logs go to stderr.  Exit codes: 0 ok, 1 bad input, 2 failure.
"""
import json
import math
import multiprocessing as mp
import os
import queue
import sys
import time

import numpy as np

from .render import (SR, InputError, _Parser, _mono, band_levels, fundamental_hz, lufs_integrated, log,
                     peak_dbfs, rms_dbfs, spectral_centroid_hz, stereo_width, BANDS, SILENT_DBFS)

DEFAULT_LIBRARY = "/Library/Audio/Presets/Xfer Records/Serum 2 Presets/Presets"
DEFAULT_INDEX = "~/Library/Application Support/open-synth/preset_index.json"
NOTES = (36, 48, 60)
NOTE_LEN_S = 1.0
RENDER_S = 2.0
VELOCITY = 100
N_SAMPLES = int(RENDER_S * SR)
ENV_POINTS = 20
BRIGHT_SLICES = 8
N_BANDS = 29
BAND_NAMES = [b[0] for b in BANDS]
FEATURE_VERSION = 1
TASK_TIMEOUT_S = 120.0
FLOOR = 1e-10
ENV_FLOOR_DB = -60.0


def index_path(arg=None):
    return os.path.abspath(os.path.expanduser(arg or os.environ.get("OSYN_INDEX") or DEFAULT_INDEX))


# --------------------------------------------------------------------------- feature extraction
def prepare(audio):
    """Trim leading silence (like analyze compare mode), crop/pad to 2 s. -> (audio (2, N), valid samples)."""
    from .analyze import trim_to_onset
    a = trim_to_onset(np.asarray(audio, dtype=np.float32))[:, :N_SAMPLES]
    valid = a.shape[1]
    if valid < N_SAMPLES:
        a = np.pad(a, ((0, 0), (0, N_SAMPLES - valid)))
    return a, valid


def _third_octave_db(x):
    from .analyze import third_octave_levels
    return np.array([v for _, v in third_octave_levels(x)])


def _flatness(x):
    """Geometric/arithmetic mean of the average power spectrum (20 Hz..20 kHz): ~0 tonal, ~0.5 noise."""
    n = 8192
    if x.size < n:
        x = np.pad(x, (0, n - x.size))
    fr = np.lib.stride_tricks.sliding_window_view(x.astype(np.float64), n)[:: n // 2] * np.hanning(n)
    p = np.mean(np.abs(np.fft.rfft(fr, axis=1)) ** 2, axis=0)
    f = np.fft.rfftfreq(n, 1 / SR)
    p = p[(f >= 20) & (f <= 20000)] + 1e-20
    return float(np.exp(np.mean(np.log(p))) / np.mean(p))


def _envelope(x, valid):
    """20 points (100 ms RMS, dB re peak, floor -60); NaN where the audio doesn't reach (query only)."""
    w = N_SAMPLES // ENV_POINTS
    rms = np.sqrt(np.mean(x.astype(np.float64).reshape(ENV_POINTS, w) ** 2, axis=1))
    db = 20 * np.log10(np.maximum(rms, 1e-9))
    db = np.maximum(db - db.max(), ENV_FLOOR_DB)
    db[np.arange(ENV_POINTS) * w >= valid] = np.nan
    return db


def _brightness(x, valid):
    """Power-weighted centroid (Hz) per 250 ms slice; NaN for slices >45 dB below the loudest or beyond the audio."""
    L = N_SAMPLES // BRIGHT_SLICES
    n, hop = 2048, 1024
    win = np.hanning(n)
    f = np.fft.rfftfreq(n, 1 / SR)
    out = np.full(BRIGHT_SLICES, np.nan)
    rms = []
    for i in range(BRIGHT_SLICES):
        seg = x[i * L:(i + 1) * L].astype(np.float64)
        rms.append(math.sqrt(float(np.mean(seg ** 2))))
    ref = max(rms)
    for i in range(BRIGHT_SLICES):
        if i * L >= valid or rms[i] < ref * 10 ** (-45 / 20) or rms[i] < 10 ** (-70 / 20):
            continue
        seg = x[i * L:(i + 1) * L].astype(np.float64)
        fr = np.lib.stride_tricks.sliding_window_view(seg, n)[::hop] * win
        p = (np.abs(np.fft.rfft(fr, axis=1)) ** 2).sum(axis=0)
        if p.sum() > 0:
            out[i] = float((f * p).sum() / p.sum())
    return out


def extract_features(audio):
    """Features of one rendering/reference (see module docstring). Arrays use NaN for 'unknown'."""
    a, valid = prepare(audio)
    pk = peak_dbfs(a)
    if pk < SILENT_DBFS:
        return {"silent": True, "peakDbfs": round(pk, 1)}
    x = _mono(a)
    rms = rms_dbfs(a)
    bands = band_levels(a)
    lufs = lufs_integrated(a)
    cen = spectral_centroid_hz(a)
    f0 = fundamental_hz(a, 0.0)
    return {
        "silent": False,
        "peakDbfs": round(pk, 1),
        "rmsDbfs": round(rms, 1),
        "lufs": None if lufs is None else round(lufs, 1),
        "spec": np.maximum(_third_octave_db(a), -100.0),
        "bands": np.array([bands[k] - rms for k in BAND_NAMES]),
        "centroidHz": cen,
        "f0Hz": f0,
        "flatness": _flatness(x),
        "width": stereo_width(a),
        "env": _envelope(x, valid),
        "bright": _brightness(x, valid),
    }


def _num(v, nd=1):
    return None if v is None or (isinstance(v, float) and not math.isfinite(v)) else round(float(v), nd)


def features_to_json(f, note):
    if f["silent"]:
        return {"note": note, "silent": True, "peakDbfs": f["peakDbfs"]}
    return {
        "note": note, "silent": False, "peakDbfs": f["peakDbfs"], "rmsDbfs": f["rmsDbfs"], "lufs": f["lufs"],
        "f0": _num(f["f0Hz"], 2), "centroid": _num(f["centroidHz"], 1),
        "spec": [_num(v, 1) for v in f["spec"]],
        "bands": [_num(v, 1) for v in f["bands"]],
        "flatness": _num(f["flatness"], 4), "width": _num(f["width"], 3),
        "env": [_num(v, 1) for v in f["env"]],
        "bright": [_num(v, 0) for v in f["bright"]],
    }


def transposition(renders):
    """Patch transposition in semitones (median over pitched renders of measured-vs-played pitch), or None."""
    d = []
    for r in renders:
        if not r.get("silent") and r.get("f0"):
            expected = 440.0 * 2 ** ((r["note"] - 69) / 12)
            d.append(12 * math.log2(r["f0"] / expected))
    return int(round(float(np.median(d)))) if d else None


# --------------------------------------------------------------------------- build
def list_presets(library):
    out = []
    for root, _, files in os.walk(library):
        for fn in files:
            if fn.lower().endswith(".serumpreset"):
                out.append(os.path.relpath(os.path.join(root, fn), library))
    return sorted(out)


def render_preset(serum, library, rel):
    from . import state
    path = os.path.join(library, rel)
    meta, _, _ = state.sp.read(path)
    renders = []
    for note in NOTES:
        serum.reset()
        serum.inject_preset(path)
        audio = np.asarray(serum.render_notes([(note, 0.0, NOTE_LEN_S)], VELOCITY, RENDER_S), dtype=np.float32)
        renders.append(features_to_json(extract_features(audio), note))
    tags = meta.get("tags") or []
    return {
        "v": FEATURE_VERSION, "path": rel,
        "name": meta.get("presetName") or os.path.splitext(os.path.basename(rel))[0],
        "author": meta.get("presetAuthor") or "", "tags": [str(t) for t in tags] if isinstance(tags, list) else [],
        "transposeSemitones": transposition(renders),
        "silent": all(r["silent"] for r in renders),
        "renders": renders,
    }


def _worker(wid, library, tasks, results):
    os.dup2(2, 1)  # native chatter must not reach stdout
    try:
        os.nice(10)
    except OSError:
        pass
    try:
        from .host import Serum
        serum = Serum()  # this is the process's main thread, as pedalboard requires
    except Exception as e:  # noqa: BLE001
        results.put(("dead", wid, f"{type(e).__name__}: {e}"))
        return
    results.put(("ready", wid))
    while True:
        rel = tasks.get()
        if rel is None:
            return
        results.put(("start", wid, rel))
        try:
            results.put(("ok", wid, render_preset(serum, library, rel)))
        except Exception as e:  # noqa: BLE001
            results.put(("err", wid, rel, f"{type(e).__name__}: {e}"))


def _read_cache(path):
    done = {}
    if os.path.isfile(path):
        with open(path, encoding="utf-8") as f:
            for line in f:
                try:
                    e = json.loads(line)
                except json.JSONDecodeError:
                    continue  # truncated last line after a crash
                if e.get("v") == FEATURE_VERSION and "renders" in e:
                    done[e["path"]] = e
    return done


def _write_json_atomic(path, obj):
    tmp = f"{path}.tmp{os.getpid()}"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(obj, f, allow_nan=False, separators=(",", ":"))
    os.replace(tmp, path)


def build(library=None, index=None, workers=4, limit=None):
    library = os.path.abspath(os.path.expanduser(library or DEFAULT_LIBRARY))
    if not os.path.isdir(library):
        raise InputError(f"library not found: {library}")
    if workers < 1:
        raise InputError("workers must be >= 1")
    ipath = index_path(index)
    os.makedirs(os.path.dirname(ipath), exist_ok=True)
    cache_path, progress_path = ipath + ".cache.jsonl", ipath + ".progress.json"
    presets = list_presets(library)
    if limit is not None:
        presets = presets[: max(0, limit)]
    wanted = set(presets)
    done = {p: e for p, e in _read_cache(cache_path).items() if p in wanted}
    todo = [p for p in presets if p not in done]
    total = len(presets)
    failed = {}
    t0 = time.time()
    started_at = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    n_resumed = len(done)
    log(f"index build: {total} presets, {n_resumed} cached, {len(todo)} to render, {workers} workers")

    def write_progress(final=False):
        el = time.time() - t0
        rendered = len(done) - n_resumed + len(failed)
        eta = 0 if final else (None if rendered == 0 else round(el / rendered * (total - len(done) - len(failed))))
        _write_json_atomic(progress_path, {
            "done": len(done), "total": total, "failed": len(failed), "startedAt": started_at,
            "updatedAt": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "etaS": eta})

    write_progress()
    if todo:
        ctx = mp.get_context("spawn")
        tasks, results = ctx.Queue(), ctx.Queue()
        for p in todo:
            tasks.put(p)
        nw = min(workers, len(todo))
        for _ in range(nw):
            tasks.put(None)
        procs, current, since = {}, {}, {}

        def spawn(wid):
            pr = ctx.Process(target=_worker, args=(wid, library, tasks, results), daemon=True)
            pr.start()
            procs[wid] = pr
            current.pop(wid, None)
            since[wid] = time.time()

        for wid in range(nw):
            spawn(wid)
        last_prog = 0.0
        cache = open(cache_path, "a", encoding="utf-8")
        respawns = 0
        try:
            while len(done) + len(failed) < total:
                try:
                    msg = results.get(timeout=1.0)
                except queue.Empty:
                    msg = None
                if msg:
                    kind, wid = msg[0], msg[1]
                    if kind == "start":
                        current[wid] = msg[2]
                        since[wid] = time.time()
                    elif kind == "ok":
                        e = msg[2]
                        current.pop(wid, None)
                        done[e["path"]] = e
                        cache.write(json.dumps(e, allow_nan=False, separators=(",", ":")) + "\n")
                        cache.flush()
                    elif kind == "err":
                        current.pop(wid, None)
                        failed[msg[2]] = msg[3]
                        log(f"failed: {msg[2]}: {msg[3]}")
                    elif kind == "dead":
                        raise RuntimeError(f"worker {wid} could not start Serum: {msg[2]}")
                # crashed or hung workers: fail the in-flight preset and replace the worker
                for wid, pr in list(procs.items()):
                    hung = wid in current and time.time() - since[wid] > TASK_TIMEOUT_S
                    if (not pr.is_alive() and pr.exitcode not in (0, None) or hung) and len(done) + len(failed) < total:
                        if hung:
                            pr.terminate()
                        if wid in current:
                            failed[current[wid]] = "worker crashed" if not hung else "timeout"
                            log(f"failed: {current[wid]}: worker {'timeout' if hung else 'crashed'}")
                        respawns += 1
                        if respawns > 20:
                            raise RuntimeError("too many worker crashes")
                        tasks.put(None)
                        spawn(wid)
                if time.time() - last_prog >= 2.0:
                    write_progress()
                    last_prog = time.time()
                    log(f"  {len(done)}/{total} done, {len(failed)} failed")
        finally:
            cache.close()
            for pr in procs.values():
                if pr.is_alive():
                    pr.terminate()
    write_progress(final=True)
    entries = [done[p] for p in presets if p in done]
    for e in entries:
        e.pop("v", None)
    _write_json_atomic(ipath, {
        "version": FEATURE_VERSION, "library": library, "builtAt": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "notes": list(NOTES), "noteLengthS": NOTE_LEN_S, "renderS": RENDER_S, "velocity": VELOCITY,
        "failed": [{"path": p, "error": m} for p, m in failed.items()], "entries": entries})
    return {"index": ipath, "indexed": len(entries), "failed": len(failed), "resumed": n_resumed,
            "silent": sum(1 for e in entries if e["silent"]), "sizeBytes": os.path.getsize(ipath),
            "elapsedS": round(time.time() - t0, 1)}


# --------------------------------------------------------------------------- query
class Index:
    """Loaded index as numpy arrays: N presets x 3 notes x features."""

    def __init__(self, path):
        with open(path, encoding="utf-8") as f:
            raw = json.load(f)
        self.path, self.raw = path, raw
        self.library = raw.get("library", "")
        self.notes = raw.get("notes", list(NOTES))
        es = raw["entries"]
        self.entries = es
        n, k = len(es), len(self.notes)
        nan = np.nan
        self.silent = np.ones((n, k), bool)
        self.spec = np.full((n, k, N_BANDS), -100.0)
        self.bands = np.full((n, k, len(BAND_NAMES)), nan)
        self.f0 = np.full((n, k), nan)
        self.cent = np.full((n, k), nan)
        self.flat = np.full((n, k), nan)
        self.width = np.full((n, k), nan)
        self.env = np.full((n, k, ENV_POINTS), nan)
        self.bright = np.full((n, k, BRIGHT_SLICES), nan)

        def vec(v, size):
            return np.array([nan if x is None else x for x in v], dtype=float) if v else np.full(size, nan)

        for i, e in enumerate(es):
            for j, r in enumerate(e["renders"][:k]):
                if r.get("silent"):
                    continue
                self.silent[i, j] = False
                self.spec[i, j] = vec(r["spec"], N_BANDS)
                self.bands[i, j] = vec(r["bands"], len(BAND_NAMES))
                self.f0[i, j] = nan if r["f0"] is None else r["f0"]
                self.cent[i, j] = nan if r["centroid"] is None else r["centroid"]
                self.flat[i, j] = r["flatness"]
                self.width[i, j] = r["width"]
                self.env[i, j] = vec(r["env"], ENV_POINTS)
                self.bright[i, j] = vec(r["bright"], BRIGHT_SLICES)
        self.tagsets = [search_terms(e) for e in es]
        self.fallback_col = self.notes.index(48) if 48 in self.notes else 0


# Category prefixes used in preset names ("BA - Perfect Pluck", "SN_LD_tryme", "pluck - yuki").
NAME_CATEGORIES = {
    "ba": "bass", "bass": "bass", "bs": "bass", "808": "bass", "pl": "pluck", "pluck": "pluck",
    "pad": "pad", "pd": "pad", "ld": "lead", "lead": "lead", "ky": "keys", "key": "keys", "keys": "keys",
    "sy": "synth", "syn": "synth", "synth": "synth", "fx": "fx", "arp": "arp", "seq": "seq", "sq": "seq",
    "chord": "chord", "ch": "chord", "vox": "vox", "kit": "drums", "drum": "drums", "perc": "drums",
    "sc": "soundscape", "or": "organ", "organ": "organ", "str": "strings", "brs": "brass", "loop": "loop", "hit": "hit",
}


def search_terms(entry):
    """Lower-case terms a tag filter can match: browser tags, library folder names, and the
    category implied by the preset name's prefix (BA -> bass, PL -> pluck, PAD -> pad, ...)."""
    import re
    terms = {t.lower() for t in entry.get("tags", [])}
    parts = entry.get("path", "").replace("\\", "/").split("/")
    terms |= {p.lower() for p in parts[:-1] if p}
    for tok in re.split(r"[\s_\-]+", (entry.get("name") or "").lower())[:3]:
        if tok in NAME_CATEGORIES:
            terms.add(NAME_CATEGORIES[tok])
    for p in parts[:-1]:  # folder names like "Bass", "Pad" also count as categories
        if p.lower() in NAME_CATEGORIES:
            terms.add(NAME_CATEGORIES[p.lower()])
    return terms


# Similarity tuning (see module docstring).
TUNE = {"wd_scale": 8.0, "base": 0.3, "w_env": 0.4, "w_bright": 0.3, "bright_scale": 1.0,
        "width_scale": 0.75, "flat_scale": 0.25, "alphas": (0.0, 0.25, 0.5, 0.75, 1.0), "w_pow": 0.15, "cap": 8.0}

_CACHE = {}


def load_index(path=None):
    """Load (and cache in-process, reloading when the file's mtime/size changes) the index."""
    p = index_path(path)
    try:
        st = os.stat(p)
    except OSError:
        raise InputError(f"index not found: {p} (build it with: python -m serum_host.index build)")
    key = (st.st_mtime_ns, st.st_size)
    hit = _CACHE.get(p)
    if hit is None or hit[0] != key:
        try:
            hit = (key, Index(p))
        except (ValueError, KeyError, TypeError) as e:
            raise InputError(f"unreadable index {p}: {e}")
        _CACHE.clear()
        _CACHE[p] = hit
    return hit[1]


def _masked_corr(a, B):
    """Pearson correlation of a (M,) against rows of B (N, M) over entries valid in both. Flat vs flat -> 1."""
    m = ~np.isnan(a)[None, :] & ~np.isnan(B)
    cnt = m.sum(axis=1)
    A = np.where(m, a[None, :], 0.0)
    Bz = np.where(m, B, 0.0)
    c = np.maximum(cnt, 1)
    am = A.sum(1, keepdims=True) / c[:, None]
    bm = Bz.sum(1, keepdims=True) / c[:, None]
    da, db = np.where(m, A - am, 0.0), np.where(m, Bz - bm, 0.0)
    va, vb = (da ** 2).sum(1), (db ** 2).sum(1)
    cov = (da * db).sum(1)
    eps = 0.25 * max(a.size, 1)  # variance below ~0.5 dB rms counts as a flat envelope
    corr = np.where((va > eps) & (vb > eps), cov / np.sqrt(np.maximum(va * vb, 1e-12)), 0.0)
    corr = np.where((va <= eps) & (vb <= eps), 1.0, corr)
    return np.where(cnt >= 3, corr, 0.5)


def best_columns(ix, ref):
    """Score the reference against every rendered note of every preset; keep the best note per preset.
    Returns (cols, scores) where scores holds the per-preset arrays of the winning note (silent notes excluded)."""
    n, k = ix.f0.shape
    per = []
    for c in range(k):
        sc = score(ix, ref, np.full(n, c))
        sc["sim"] = np.where(ix.silent[:, c], -1.0, sc["sim"])
        per.append(sc)
    sims = np.stack([p["sim"] for p in per], axis=1)
    cols = np.argmax(sims, axis=1)
    rows = np.arange(n)
    return cols, {key: np.stack([p[key] for p in per], axis=1)[rows, cols] for key in per[0]}


def score(ix, ref, cols):
    """Vectorised similarity of reference features to each preset's selected render. -> dict of arrays."""
    T = TUNE
    n = len(ix.entries)
    rows = np.arange(n)
    spec = ix.spec[rows, cols]
    rp = 10 ** (ref["spec"] / 10)
    power = 10 ** (spec / 10)

    # Residual pitch (ref - candidate, in octaves), 0 when either f0 is unknown.
    with np.errstate(invalid="ignore", divide="ignore"):
        res_oct = np.log2(ref["f0Hz"] / ix.f0[rows, cols]) if ref["f0Hz"] else np.full(n, np.nan)
    res_oct = np.where(np.isfinite(res_oct), res_oct, 0.0)
    idx = np.arange(N_BANDS)[None, :]

    def distance(c):
        cp = 10 ** (c / 10)
        loudest = np.maximum(rp[None, :], cp)
        w = np.where(loudest >= 1e-5, loudest ** T["w_pow"], 0.0)
        w = w / np.maximum(w.sum(1, keepdims=True), FLOOR)
        delta = np.clip(c - ref["spec"][None, :], -T["cap"], T["cap"])
        return np.sqrt((w * delta ** 2).sum(1))

    # Pitch alignment: the chosen render is within ~6 semitones of the reference, which still moves
    # harmonics across 1/3-octave bands.  Try the candidate spectrum as-is (fixed filters/formants),
    # fully shifted by the residual pitch (harmonic structure), and halfway; keep the best per preset.
    wd, alpha = None, np.zeros(n)
    for a in T["alphas"]:
        pos = idx - 3.0 * a * res_oct[:, None]  # source position in band units
        lo = np.floor(pos).astype(int)
        fr = pos - lo
        def at(k):
            ok = (k >= 0) & (k < N_BANDS)
            return np.where(ok, np.take_along_axis(power, np.clip(k, 0, N_BANDS - 1), axis=1), 1e-10)
        # interpolate in the power domain (dB interpolation would invent mid levels between a peak and a hole)
        shifted = 10 * np.log10(at(lo) * (1 - fr) + at(lo + 1) * fr + 1e-10) if a else spec
        d = distance(shifted)
        if wd is None:
            wd = d
        else:
            better = d < wd
            wd, alpha = np.where(better, d, wd), np.where(better, a, alpha)
    corr = _masked_corr(ref["env"], ix.env[rows, cols])
    # brightness over time: rms of log2 centroid ratios over slices valid in both, 0.5 if <2 common slices
    rb, cb = ref["bright"][None, :], ix.bright[rows, cols] * 2.0 ** (alpha * res_oct)[:, None]
    with np.errstate(invalid="ignore", divide="ignore"):
        lr = np.log2(cb / rb)
    ok = ~np.isnan(lr)
    cnt = ok.sum(1)
    bd = np.sqrt(np.where(ok, lr ** 2, 0).sum(1) / np.maximum(cnt, 1))
    bd = np.where(cnt >= 1, bd, np.nan)
    bf = np.where(np.isnan(bd), 0.5, np.exp(-bd / T["bright_scale"]))
    flat_diff = ix.flat[rows, cols] - ref["flatness"]
    width_diff = ix.width[rows, cols] - ref["width"]
    shape = T["base"] + T["w_env"] * np.maximum(corr, 0) + T["w_bright"] * bf
    pen = np.exp(-abs(width_diff) / T["width_scale"]) * np.exp(-abs(flat_diff) / T["flat_scale"])
    sim = 100 * np.exp(-wd / T["wd_scale"]) * shape * pen
    return {"sim": np.clip(sim, 0, 100), "wd": wd, "corr": corr, "bright": bd, "flat": flat_diff, "width": width_diff}


def query(audio, start=0.0, end=None, top=10, tags=None, index=None):
    from .analyze import load_audio
    if end is not None and end <= start:
        raise InputError("end must be after start")
    if top < 1:
        raise InputError("top must be >= 1")
    ix = load_index(index)
    raw = load_audio(audio, start, end)
    ref = extract_features(raw)
    if ref["silent"]:
        raise InputError("reference audio is silent")
    f0_ref = ref["f0Hz"]
    cols, s = best_columns(ix, ref)
    n = len(ix.entries)
    rows = np.arange(n)
    mask = ~ix.silent.all(axis=1)
    want = {t.strip().lower() for t in (tags or []) if t.strip()}
    if want:
        mask &= np.array([want <= ts for ts in ix.tagsets], dtype=bool)
    sim = np.where(mask, s["sim"], -1.0)
    order = np.argsort(-sim, kind="stable")[: min(top, int(mask.sum()))]
    results = []
    for i in order:
        c = cols[i]
        e = ix.entries[i]
        cf0, ccen = ix.f0[i, c], ix.cent[i, c]
        results.append({
            "path": e["path"], "absolutePath": os.path.join(ix.library, e["path"]), "name": e["name"],
            "author": e["author"], "tags": e["tags"], "similarity": round(float(sim[i]), 1),
            "note": int(ix.notes[c]),
            "details": {
                "pitchSemitones": _num(12 * math.log2(cf0 / f0_ref), 2) if f0_ref and not np.isnan(cf0) else None,
                "brightnessOctaves": _num(math.log2(ccen / ref["centroidHz"]), 2)
                if ref["centroidHz"] and not np.isnan(ccen) else None,
                "bandBalanceDb": {k: _num(v, 1) for k, v in zip(BAND_NAMES, ix.bands[i, c] - ref["bands"])},
                "envelopeCorrelation": _num(s["corr"][i], 3),
                "flatnessDiff": _num(s["flat"][i], 3),
                "widthDiff": _num(s["width"][i], 3),
                "brightnessCurveOctaves": _num(s["bright"][i], 2),
                "spectralDistanceDb": _num(s["wd"][i], 1),
            }})
    return {
        "reference": {
            "fundamentalHz": _num(f0_ref, 2), "spectralCentroidHz": _num(ref["centroidHz"], 1),
            "rmsDbfs": ref["rmsDbfs"], "lufsIntegrated": ref["lufs"], "stereoWidth": _num(ref["width"], 3),
            "flatness": _num(ref["flatness"], 4), "peakDbfs": ref["peakDbfs"],
            "bands": {k: _num(v, 1) for k, v in zip(BAND_NAMES, ref["bands"])}, "silent": False},
        "index": {"path": ix.path, "entries": n, "searched": int(mask.sum())},
        "results": results,
    }


def status(index=None):
    p = index_path(index) + ".progress.json"
    try:
        with open(p, encoding="utf-8") as f:
            return {"exists": True, **json.load(f)}
    except (OSError, ValueError):
        return {"exists": False}


# --------------------------------------------------------------------------- CLI
def run(argv):
    ap = _Parser(prog="serum_host.index", description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build")
    b.add_argument("--library", default=None)
    b.add_argument("--index", default=None)
    b.add_argument("--workers", type=int, default=4)
    b.add_argument("--limit", type=int, default=None)
    q = sub.add_parser("query")
    q.add_argument("audio")
    q.add_argument("--start", type=float, default=0.0)
    q.add_argument("--end", type=float, default=None)
    q.add_argument("--top", type=int, default=10)
    q.add_argument("--tags", default="")
    q.add_argument("--index", default=None)
    a = ap.parse_args(argv)
    if a.cmd == "build":
        return build(a.library, a.index, a.workers, a.limit)
    return query(a.audio, a.start, a.end, a.top, [t for t in a.tags.split(",") if t.strip()], a.index)


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
