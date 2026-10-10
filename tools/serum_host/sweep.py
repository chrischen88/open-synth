"""Value-mapping sweep for Serum 2's automatable host parameters.

Stages (all take --cache, a JSON file used as checkpoint/resume store):

  collect   host Serum, and for every automatable param set raw r in {0, 1/8, ..., 1} from a
            freshly reset state, settle, and record the changed processor-state paths, the
            stored plain values and the host display string.  Stepped params (strings /
            integers / repeated values) are re-sampled on a dense grid and their step edges
            bisected.
  fit       turn the cache into serum2_units.json (affine / power / exp / round / toggle /
            stepped / enum / table transforms, display laws, side effects).
  validate  check every path against serum2_schema.json.
  spot      set a mid value via the host for N transforms and compare with the prediction.

Usage (from tools/):
  python -m serum_host.sweep collect --cache /tmp/sweep.json [--only a_level,a_pan]
  python -m serum_host.sweep fit --cache /tmp/sweep.json --out ../crates/osyn-params/data/serum2_units.json
  python -m serum_host.sweep validate --units ... --schema ...
  python -m serum_host.sweep spot --units ... --n 5

Env: SERUM_VST3 overrides the plugin path.
"""
import argparse
import json
import math
import os
import re
import sys
import time

POINTS = [i / 8 for i in range(9)]
SKIP_RE = re.compile(r"(^pitch_bend|^mod_wheel|aftertouch|(^|_)cc\d*(_|$))")
FX_RE = re.compile(r"^fx_(main|bus_\d)_param_\d+$")


def _jsonable(o):
    if isinstance(o, bytes):
        return {"__bytes__": o.hex()}
    if isinstance(o, dict):
        return {str(k): _jsonable(v) for k, v in o.items()}
    if isinstance(o, (list, tuple)):
        return [_jsonable(v) for v in o]
    return o


def _atomic_dump(obj, path):
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(obj, f)
    os.replace(tmp, path)


# ------------------------------------------------------------------------------------- collect

def _diff(base, cur):
    """Paths whose value differs from baseline; a path missing on one side is None there."""
    out = {}
    for k in set(base) | set(cur):
        if base.get(k) != cur.get(k):
            out[k] = _jsonable(cur.get(k))
    return out


def _is_str(v):
    return isinstance(v, str)


class Sampler:
    def __init__(self, serum):
        from . import state
        self.s, self.state = serum, state
        serum.reset()
        b1 = state.flatten(serum.processor_state())
        serum.reset()
        b2 = state.flatten(serum.processor_state())
        self.noisy = {k for k in set(b1) | set(b2) if b1.get(k) != b2.get(k)}
        self.base = {k: v for k, v in b2.items() if k not in self.noisy and not k.startswith("/tags")}
        self.cbase = state.flatten(serum.controller_state())

    def point(self, name, r, reset=True):
        s = self.s
        if reset:
            s.reset()
        s.set(name, r)
        s.settle()
        flat = self.state.flatten(s.processor_state())
        flat = {k: v for k, v in flat.items() if k not in self.noisy and not k.startswith("/tags")}
        cflat = self.state.flatten(s.controller_state())
        return {"r": r, "readback": s.parameters[name].raw_value, "disp": s.display(name),
                "changes": _diff(self.base, flat), "ctrl": _diff(self.cbase, cflat),
                "baseVals": {k: _jsonable(self.base.get(k)) for k in _diff(self.base, flat)}}


def _primary(points):
    """Pick the path with the most distinct values (None = default counts as a value)."""
    paths = set()
    for p in points:
        paths |= set(p["changes"])
    best, bestn = None, 0
    for k in sorted(paths):
        n = len({json.dumps(p["changes"].get(k)) for p in points})
        if n > bestn:
            best, bestn = k, n
    return best


PROBES = [0.0625, 0.19, 0.44, 0.93]


def _integer_valued(points, primary):
    present = [p["changes"].get(primary) for p in points if p["changes"].get(primary) is not None]
    return bool(present) and all(isinstance(v, (int, float)) and abs(v - round(v)) < 1e-6 for v in present)


def _needs_dense(points, primary, probes=()):
    vals = [p["changes"].get(primary) for p in points]
    present = [v for v in vals if v is not None]
    if not present:
        return False
    if any(_is_str(v) for v in present):
        return True
    if _integer_valued(points, primary):
        pv = [p["changes"].get(primary) for p in probes]
        # smooth if some probe is non-integer or the probes disagree with a step-free integer lattice
        return all(v is None or abs(v - round(v)) < 1e-6 for v in pv)
    return len({json.dumps(v) for v in vals}) < len(vals)  # repeated values -> stepped


def _dense(sm, name, primary, seed, default_raw):
    """Adaptive dense sampling: reuse the 9 seed points; if >2 distinct values add grid 16, then
    64 (>=6 distinct), then 256 (>=30); finally bisect every step edge (7 halvings)."""
    pts = {p["r"]: p for p in seed}
    key = lambda r: json.dumps(pts[r]["changes"].get(primary))
    few = len({key(r) for r in pts}) <= 2
    for grid in (() if few else (16, 64, 256)):
        for i in range(grid + 1):
            r = i / grid
            if r not in pts:
                pts[r] = sm.point(name, r, reset=False)
        n = len({key(r) for r in pts})
        if n < (6 if grid == 16 else 30):
            break
    rs = sorted(pts)
    edges = [(a, b) for a, b in zip(rs, rs[1:]) if key(a) != key(b)]
    if len(edges) > 64:
        edges = []  # too many steps to bisect; grid resolution only
    for a, b in edges:
        if True:
            if default_raw in (a, b) and "null" in (key(a), key(b)):
                continue  # default value is stored as a missing key; not a real edge
            lo, hi = a, b
            for _ in range(7):
                mid = (lo + hi) / 2
                pts[mid] = sm.point(name, mid, reset=False)
                if key(mid) == key(lo):
                    lo = mid
                else:
                    hi = mid
    sm.s.reset()
    return [{"r": r, "disp": pts[r]["disp"], "v": pts[r]["changes"].get(primary)} for r in sorted(pts)]


def collect(args):
    from .host import Serum
    cache = json.load(open(args.cache)) if os.path.exists(args.cache) else {}
    t0 = time.time()
    serum = Serum()
    print(f"loaded in {time.time() - t0:.1f}s", flush=True)
    sm = Sampler(serum)
    names = [n for n, p in serum.parameters.items() if p.is_automatable]
    if args.only:
        names = [n for n in args.only.split(",") if n in serum.parameters]
    todo = [n for n in names if n not in cache and not SKIP_RE.search(n) and not FX_RE.match(n)]
    for extra in (args.skip_cache or "").split(","):
        if extra and os.path.exists(extra):
            done_elsewhere = json.load(open(extra))
            todo = [n for n in todo if n not in done_elsewhere]
    if args.shard:
        k, n_ = map(int, args.shard.split("/"))
        todo = [n for i, n in enumerate(todo) if i % n_ == k]
    print(f"{len(names)} automatable, {len(todo)} to sweep", flush=True)
    done = 0
    for n in todo:
        prm = serum.parameters[n]
        t = time.time()
        pts = [sm.point(n, r) for r in POINTS]
        ent = {"default_raw": prm.default_raw_value, "points": pts, "primary": None, "dense": None}
        ent["primary"] = _primary(pts)
        ent["probes"] = []
        if ent["primary"] and _integer_valued(pts, ent["primary"]):
            # integer values at the 9 points may still be a smooth map (e.g. 200r-100): probe off-grid
            ent["probes"] = [sm.point(n, r) for r in PROBES]
        if ent["primary"] and _needs_dense(pts, ent["primary"], ent["probes"]):
            ent["dense"] = _dense(sm, n, ent["primary"], pts, prm.default_raw_value)
        serum.reset()
        cache[n] = ent
        done += 1
        print(f"{n}: primary={ent['primary']} dense={bool(ent['dense'])} ({time.time() - t:.1f}s)", flush=True)
        if done % 10 == 0:
            _atomic_dump(cache, args.cache)
        if args.max and done >= args.max:
            break
    _atomic_dump(cache, args.cache)
    print(f"collect done: {done} params in {time.time() - t0:.0f}s", flush=True)


# ---------------------------------------------------------------------------------------- fit

def _maxerr(pred, ys):
    return max(abs(p - y) for p, y in zip(pred, ys))


def _sig(x, n):
    return float(f"{x:.{n}g}") if abs(x) > 1e-12 else 0.0


def _clean(t, pts, tol):
    """Round fitted coefficients to the fewest significant digits that keep the fit within tol."""
    for n in (4, 6, 8, 10, 12):
        c = dict(t)
        for k in ("a", "b"):
            c[k] = _sig(t[k], n)
        e = max(abs(predict(c, r) - y) for r, y in pts)
        if e <= tol:
            c["maxErr"] = e
            return c
    return t


def fit_numeric(pts):
    """pts [(r, y)] all numeric.  Return transform dict or None."""
    import numpy as np
    r = np.array([p[0] for p in pts], float)
    y = np.array([p[1] for p in pts], float)
    rng = float(y.max() - y.min())
    tol = 1e-4 * max(rng, 1e-12)
    if len(pts) < 2 or rng == 0:
        return None
    best = None
    b, a = np.polyfit(r, y, 1)
    e = float(np.abs(a + b * r - y).max())
    if e <= tol:
        return _clean({"type": "affine", "a": float(a), "b": float(b), "maxErr": e}, pts, tol)
    for k in (2, 3, 4, 5):
        b, a = np.polyfit(r ** k, y, 1)
        e = float(np.abs(a + b * r ** k - y).max())
        if e <= tol:
            return _clean({"type": "power", "a": float(a), "b": float(b), "k": k, "maxErr": e}, pts, tol)
    if (y > 0).all():
        lb, la = np.polyfit(r, np.log(y), 1)
        e = float(np.abs(math.exp(la) * np.exp(lb * r) - y).max())
        if e <= tol:
            return _clean({"type": "exp", "a": math.exp(la), "b": math.exp(lb), "maxErr": e}, pts, tol)
    return None


def fit_round(pts):
    """Integer maps: round(lo + (hi-lo)*r) or lo + floor((hi-lo+1)*r) clamped to hi.  Values within
    2e-3 of a step edge may land on either side (edges were bisected to ~1.5e-4)."""
    lo, hi = pts[0][1], pts[-1][1]
    if pts[0][0] != 0.0 or pts[-1][0] != 1.0 or lo == hi:
        return None
    for t in ({"type": "round", "a": lo, "b": hi - lo, "maxErr": 0.0},
              {"type": "floor", "a": lo, "n": hi - lo + 1, "hi": hi, "maxErr": 0.0}):
        if all(any(predict(t, min(1.0, max(0.0, r + d))) == y for d in (0, -2e-3, 2e-3)) for r, y in pts):
            return t
    return None


def predict(t, r):
    ty = t["type"]
    if ty == "affine":
        return t["a"] + t["b"] * r
    if ty == "power":
        return t["a"] + t["b"] * r ** t["k"]
    if ty == "exp":
        return t["a"] * t["b"] ** r
    if ty == "round":
        return float(math.floor(t["a"] + t["b"] * r + 0.5))
    if ty == "floor":
        return float(min(t["hi"], t["a"] + math.floor(t["n"] * r)))
    if ty == "toggle":
        return t["high"] if r >= t["threshold"] else t["low"]
    if ty in ("enum", "stepped"):
        for lo, hi, tok, _ in t["steps"]:
            if lo <= r < hi or (r == 1.0 and hi == 1.0):
                return tok
    return None


_UNIT_RE = re.compile(r"^\s*([-+]?\d+(?:\.\d+)?)\s*([A-Za-z%#]*)")


def parse_display(s):
    m = _UNIT_RE.match(s or "")
    if not m:
        return None
    v, u = float(m.group(1)), m.group(2)
    if u == "ms":
        v, u = v / 1000, "s"
    elif u == "kHz":
        v, u = v * 1000, "Hz"
    return v, u


def display_info(samples):
    """samples [[r, str]] -> {"unit", "samples", optional "fit"}."""
    out = {"unit": "", "samples": samples}
    parsed = [(r, parse_display(s)) for r, s in samples]
    if not parsed or any(p is None for _, p in parsed):
        return out
    units = [p[1] for _, p in parsed]
    out["unit"] = max(set(units), key=units.count)
    pts = [(r, p[0]) for r, p in parsed]
    if len(pts) < 3:
        return out
    import numpy as np
    r = np.array([p[0] for p in pts])
    y = np.array([p[1] for p in pts])
    rng = float(y.max() - y.min())
    if rng == 0:
        return out
    tol = 2e-2 * rng
    cands = []
    b, a = np.polyfit(r, y, 1)
    cands.append(({"type": "affine", "a": float(a), "b": float(b)}, np.abs(a + b * r - y).max()))
    for k in (2, 3, 4, 5):
        b, a = np.polyfit(r ** k, y, 1)
        cands.append(({"type": "power", "a": float(a), "b": float(b), "k": k}, np.abs(a + b * r ** k - y).max()))
    if (y > 0).all():
        lb, la = np.polyfit(r, np.log(y), 1)
        cands.append(({"type": "exp", "a": math.exp(la), "b": math.exp(lb)},
                      np.abs(math.exp(la) * np.exp(lb * r) - y).max()))
    for t, e in cands:
        if e <= tol:
            t["maxRelErr"] = float(e / rng)
            out["fit"] = t
            break
    return out


def _steps(dense, key="v"):
    steps = []
    for d in dense:
        v = d[key]
        if steps and steps[-1][2] == v:
            continue
        steps.append([d["r"], None, v, d["disp"]])
    for i, st in enumerate(steps):
        st[1] = steps[i + 1][0] if i + 1 < len(steps) else 1.0
    steps[0][0] = 0.0
    return steps


def _enum_law(data, tr):
    """If step edges match floor(n*r) or round((n-1)*r) within the sampling interval, snap them to it."""
    steps = tr["steps"]
    idx = {d["r"]: i for i, d in enumerate(data)}
    iv = []  # (last r of previous token, first r of next token]
    for st in steps[1:]:
        j = idx[st[0]]
        iv.append((data[j - 1]["r"], st[0]))
    if len(iv) < 2:
        return
    n0 = len(steps)
    for n in range(n0 - 2, n0 + 3):
        for law, edge in (("floor", lambda k, n=n: k / n), ("round", lambda k, n=n: (k - 0.5) / (n - 1) if n > 1 else 9)):
            es = [edge(k + 1) for k in range(len(iv))]
            if all(a < e <= b + 1e-9 for (a, b), e in zip(iv, es)):
                # tokens must run k=0..: step i covers index i
                edges = [0.0] + [round(e, 9) for e in es] + [1.0]
                for i, st in enumerate(steps):
                    st[0], st[1] = edges[i], edges[i + 1]
                tr["law"] = law
                tr["n"] = n
                return


def fit_param(host, ent, enum_defaults):
    """Return (entry, None) or (None, reason)."""
    pts = ent["points"]
    primary = ent["primary"]
    ctrl_only = any(p["ctrl"] for p in pts)
    if not primary:
        return None, ("changes only controller (UI) state" if ctrl_only else
                      "no processor-state change at any sampled value (unbound or state-dependent)")
    dr = ent["default_raw"]
    base = None
    for p in pts:
        if primary in p["baseVals"] and p["baseVals"][primary] is not None:
            base = p["baseVals"][primary]
    samples = [[p["r"], p["changes"].get(primary)] for p in pts]
    paths_all = []
    for p in pts:
        for k in p["changes"]:
            if k not in paths_all:
                paths_all.append(k)
    from .state import to_repo_path
    side = [to_repo_path(k) for k in paths_all if k != primary]
    disp = display_info([[p["r"], p["disp"]] for p in pts])
    data = ent["dense"] or [{"r": p["r"], "disp": p["disp"], "v": p["changes"].get(primary)}
                            for p in sorted(pts + ent.get("probes", []), key=lambda p: p["r"])]
    if ent["dense"] and any(d["v"] is None for d in data):
        # a key omitted at the default raw value: take the plain of the nearest neighbour showing the same
        # display string.  Other omissions (e.g. a "Off" token that is never stored) stay None.
        have = [d for d in data if d["v"] is not None]
        for d in data:
            if d["v"] is None and have:
                h = min(have, key=lambda h: abs(h["r"] - d["r"]))
                if h["disp"] == d["disp"] and abs(d["r"] - dr) < 2e-3:
                    d["v"] = h["v"]
                    if base is None:
                        base = h["v"]
    present = [d for d in data if d["v"] is not None]
    nonnum = any(not isinstance(d["v"], (int, float)) or isinstance(d["v"], bool) for d in present)
    default_plain = base
    has_missing = len(present) != len(data)
    tr = None
    if nonnum:
        # enum: stored tokens; token null = key omitted (the default / "Off" state is not written)
        steps = _steps(data)
        tr = {"type": "enum", "steps": steps}
        _enum_law(data, tr)
        if default_plain is None:
            at = [st[2] for st in tr["steps"] if st[0] <= dr < st[1]]
            default_plain = at[0] if at else None
        if any(st[2] is None for st in steps):
            tr["nullMeansOmitted"] = True
    else:
        npts = [(d["r"], d["v"]) for d in present]
        distinct = sorted({round(v, 9) for _, v in npts})
        if has_missing:
            # missing = default value; take it from baseline, else infer below
            if default_plain is not None:
                npts = [(d["r"], d["v"] if d["v"] is not None else default_plain) for d in data]
        smooth = not ent["dense"]
        tr = fit_numeric(npts) if smooth else None
        if tr is None and len(distinct) == 2 and not smooth or (tr is None and len(distinct) == 2 and not has_missing):
            lo_v, hi_v = distinct
            thr = None
            seq = [(r, v) for r, v in npts]
            for (r1, v1), (r2, v2) in zip(seq, seq[1:]):
                if v1 != v2:
                    thr = round((r1 + r2) / 2, 3)
                    break
            if thr is not None and seq[0][1] == lo_v:
                tr = {"type": "toggle", "threshold": thr, "low": lo_v, "high": hi_v}
        if tr is None and not smooth:
            rr = fit_round(npts)
            if rr:
                tr = rr
        if tr is None and has_missing and default_plain is None:
            # missing key == default; try inferring the default as 0/1 complement for toggles
            if len(distinct) == 1 and distinct[0] in (0.0, 1.0):
                default_plain = 1.0 - distinct[0]
                full = [(d["r"], d["v"] if d["v"] is not None else default_plain) for d in data]
                tr = {"type": "toggle", "low": min(v for _, v in full), "high": max(v for _, v in full),
                      "threshold": next(round((r1 + r2) / 2, 3) for (r1, v1), (r2, v2) in zip(full, full[1:]) if v1 != v2)}
                tr["defaultInferred"] = True
            else:
                # drop missing points, fit on the rest, then predict the default
                tr = fit_numeric(npts)
                if tr:
                    default_plain = _sig(predict(tr, dr), 9)
        if tr is None and not smooth:
            tr = fit_numeric(npts)
        if tr is None and len(distinct) <= 40 and ent["dense"]:
            full = [{"r": d["r"], "disp": d["disp"], "v": d["v"] if d["v"] is not None else default_plain}
                    for d in data]
            if all(d["v"] is not None for d in full):
                tr = {"type": "stepped", "steps": _steps(full)}
        if tr is None:
            tr = {"type": "table", "samples": [[r, v] for r, v in samples]}
        if default_plain is None and tr["type"] not in ("table", "enum", "stepped"):
            default_plain = _sig(predict(tr, dr), 9)
        elif default_plain is None and tr["type"] in ("stepped",):
            default_plain = predict(tr, dr)
        if default_plain is not None and not isinstance(default_plain, str):
            default_plain = _sig(default_plain, 9)
    full_samples = [[r, (v if v is not None else default_plain)] for r, v in samples]
    entry = {"host": host, "path": to_repo_path(primary), "defaultRaw": dr, "defaultPlain": default_plain,
             "transform": tr, "display": disp, "sideEffects": side, "samples": full_samples}
    if ent["dense"] and tr["type"] in ("enum", "stepped"):
        pass
    return entry, None


def primary_key(flat):
    return flat


def load_caches(spec):
    cache = {}
    for f in spec.split(","):
        if f and os.path.exists(f):
            cache.update(json.load(open(f)))
    return cache


def fit(args):
    cache = load_caches(args.cache)
    params, unmapped = [], []
    enum_defaults = {}
    for host, ent in cache.items():
        entry, why = fit_param(host, ent, enum_defaults)
        if entry:
            params.append(entry)
        else:
            if re.match(r"^[abc]_param\d+$", host):
                why = ("osc-type-specific generic slot: no processor-state change on a wavetable osc, nor with the "
                       "oscillator type forced to Sample (display stays a bare number)")
            unmapped.append({"host": host, "reason": why})
    out = {"generatedFrom": {"serumVersion": args.serum_version, "points": len(POINTS)},
           "params": params, "unmapped": unmapped + _static_unmapped(args)}
    json.dump(out, open(args.out, "w"), indent=1, ensure_ascii=False)
    from collections import Counter
    print(Counter(p["transform"]["type"] for p in params), "unmapped", len(out["unmapped"]))


def _static_unmapped(args):
    """Params skipped on purpose (listed from the cache-independent params dump if given)."""
    out = []
    if args.params_dump and os.path.exists(args.params_dump):
        cache = load_caches(args.cache)
        for p in json.load(open(args.params_dump)):
            n = p["key"]
            if not p["automatable"] or n in cache:
                continue
            if FX_RE.match(n):
                out.append({"host": n, "reason": "FX slot param: generic unbound host slot; with FX modules loaded "
                            "in the rack, setting it changes neither FXRack state nor display (needs GUI binding)"})
            elif SKIP_RE.search(n):
                out.append({"host": n, "reason": "MIDI controller input (pitch bend / mod wheel / cc / aftertouch), skipped"})
    return out


# ------------------------------------------------------------------------------- validate / spot

def validate(args):
    schema = json.load(open(args.schema))["modules"]
    units = json.load(open(args.units))
    bad = []
    for p in units["params"]:
        mod, _, prm = p["path"].rpartition(".")
        seg = mod.split("/")[-1]
        typ = re.sub(r"\d+$", "", seg)
        names = {x["name"] for x in schema.get(typ, {}).get("params", [])}
        if typ not in schema:
            bad.append((p["host"], p["path"], f"module {typ} not in schema"))
        elif prm not in names:
            bad.append((p["host"], p["path"], f"param {prm} not in {typ}"))
    print(f"{len(units['params'])} paths checked, {len(bad)} not in schema")
    for b in bad:
        print("  ", b)
    return bad


def spot(args):
    from .host import Serum
    from . import state
    units = json.load(open(args.units))
    # one param per transform family, preferring varied modules
    chosen, seen = [], set()
    for ty in ("power", "enum", "floor", "affine", "toggle", "round", "exp", "stepped"):
        for p in units["params"]:
            if p["transform"]["type"] == ty and p["path"].split(".")[0] not in seen:
                chosen.append(p)
                seen.add(p["path"].split(".")[0])
                break
    chosen = chosen[:args.n]
    s = Serum()
    ok = 0
    for p in chosen:
        r = args.r
        s.reset()
        s.set(p["host"], r)
        s.settle()
        flat = state.flatten(s.processor_state())
        got = None
        for k, v in flat.items():
            if state.to_repo_path(k) == p["path"]:
                got = v
        pred = predict(p["transform"], r)
        if got is None:
            got = p["defaultPlain"]
        good = (got == pred) if isinstance(pred, str) or pred is None else abs(got - pred) <= 1e-4 * max(1.0, abs(pred))
        ok += bool(good)
        print(f"{p['host']:24s} {p['transform']['type']:8s} r={r} stored={got} predicted={pred} {'OK' if good else 'MISMATCH'}")
    print(f"spot-check {ok}/{len(chosen)} ok")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("collect")
    c.add_argument("--cache", required=True)
    c.add_argument("--only")
    c.add_argument("--max", type=int, default=0)
    c.add_argument("--shard", help="k/n: handle every n-th pending param (run n processes in parallel)")
    c.add_argument("--skip-cache", help="comma-separated other cache files whose params are skipped")
    f = sub.add_parser("fit")
    f.add_argument("--cache", required=True, help="comma-separated cache files")
    f.add_argument("--out", required=True)
    f.add_argument("--params-dump", help="JSON dump of host params (key/automatable) to list skipped ones")
    f.add_argument("--serum-version", default="2.1.5")
    v = sub.add_parser("validate")
    v.add_argument("--units", required=True)
    v.add_argument("--schema", required=True)
    sp_ = sub.add_parser("spot")
    sp_.add_argument("--units", required=True)
    sp_.add_argument("--n", type=int, default=5)
    sp_.add_argument("--r", type=float, default=0.37)
    a = ap.parse_args(argv)
    {"collect": collect, "fit": fit, "validate": validate, "spot": spot}[a.cmd](a)


if __name__ == "__main__":
    main()
