#!/usr/bin/env python3
"""Build a machine-readable Serum 2 parameter schema.

Combines
  (a) C++ enum declaration strings embedded in the Serum2 plugin binary
      (dump with:  strings -a -arch arm64 .../Serum2.vst3/Contents/MacOS/Serum2 > strings.txt)
  (b) values observed across the installed preset library.

Attribution of a `kParam*` declaration group to a CBOR module type uses
  1. a module-name string sitting just before the declaration ("hint"), and
  2. overlap between the param names observed in presets for that module and the declared names.

Usage:
  serum2_extract_schema.py [strings.txt] [--presets DIR] [--out schema.json] [--cache scan.pkl]
Run with a python that has cbor2 (and `zstd` on PATH), e.g.  python -I serum2_extract_schema.py ...
A validation report is printed to stdout.
"""
import argparse
import collections
import glob
import json
import os
import pickle
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import serum2_preset as sp  # noqa: E402

DEFAULT_STRINGS = "strings.txt"
DEFAULT_PRESETS = "/Library/Audio/Presets/Xfer Records/Serum 2 Presets/Presets"
DEFAULT_OUT = os.path.join(HERE, "..", "crates", "osyn-params", "data", "serum2_schema.json")
SERUM_VERSION = "2.1.5"

# --------------------------------------------------------------------------------------
# (b) preset scan
# --------------------------------------------------------------------------------------

def _tpl(k):
    m = re.match(r'^(.*?)(\d+)$', k)
    return (m.group(1), int(m.group(2))) if m else (k, None)


def scan_presets(root):
    files = sorted(glob.glob(root + "/**/*.SerumPreset", recursive=True))
    inst = collections.defaultdict(collections.Counter)
    idx = collections.defaultdict(lambda: collections.defaultdict(set))
    params = collections.defaultdict(lambda: collections.defaultdict(
        lambda: {'count': 0, 'min': None, 'max': None, 'strings': set(), 'types': set()}))
    fxtype = collections.defaultdict(collections.Counter)
    modslots = []
    uikeys = collections.Counter()
    versions = collections.Counter()
    errors = []

    def rec(t, name, v):
        s = params[t][name]
        s['count'] += 1
        if isinstance(v, bool):
            v = float(v)
        if isinstance(v, (int, float)):
            s['types'].add('num')
            s['min'] = v if s['min'] is None else min(s['min'], v)
            s['max'] = v if s['max'] is None else max(s['max'], v)
        elif isinstance(v, str):
            s['strings'].add(v)
            s['types'].add('str')
        else:
            s['types'].add(type(v).__name__)

    cur_version = [None]

    def handle(t, p, ids, v):
        inst[t]['/'.join(p)] += 1
        idx[t]['/'.join(p)].add(tuple(ids))
        pp = v['plainParams']
        if isinstance(pp, dict):
            for n, x in pp.items():
                rec(t, n, x)
        if t == 'ModSlot' and 'destModuleTypeString' in v:
            modslots.append((v['destModuleTypeString'], v['destModuleParamName'],
                             v['destModuleParamID'], v['destModuleID'], cur_version[0]))
        walk(v, p, ids)

    def walk(o, path, ids):
        for k, v in o.items():
            if k == 'plainParams':
                continue
            if k.startswith('kUI'):
                uikeys[k] += 1
            if isinstance(v, dict):
                if 'plainParams' in v:
                    t, i = _tpl(k)
                    handle(t, path + [k if i is None else t + '{}'], ids + ([i] if i is not None else []), v)
                else:
                    walk(v, path, ids)  # UI-state dicts etc. (only to harvest kUI keys)
            elif isinstance(v, list):
                if k == 'FX' and path and path[-1].startswith('FXRack'):
                    for item in v:
                        ty = item.get('type')
                        for kk, vv in item.items():
                            if kk.startswith('kUI'):
                                uikeys[kk] += 1
                            if isinstance(vv, dict) and 'plainParams' in vv:
                                fxtype[kk][ty] += 1
                                handle(kk, path + ['FX[]', kk], ids, vv)

    n = 0
    for f in files:
        try:
            m, d, _ = sp.read(f)
        except Exception as e:  # corrupt payloads
            errors.append((f, str(e)[:80]))
            continue
        n += 1
        versions[m.get('productVersion')] += 1
        cur_version[0] = m.get('productVersion')
        walk(d, [], [])
    return dict(
        presetsScanned=n, presetFiles=len(files), errors=errors, versions=dict(versions),
        inst={k: dict(v) for k, v in inst.items()},
        idx={t: {p: sorted(s) for p, s in d.items()} for t, d in idx.items()},
        params={t: {nm: dict(s, strings=sorted(s['strings']), types=sorted(s['types']))
                    for nm, s in d.items()} for t, d in params.items()},
        fxtype={k: dict(v) for k, v in fxtype.items()},
        modslots=modslots, uikeys=dict(uikeys))

# --------------------------------------------------------------------------------------
# (a) binary enum declarations
# --------------------------------------------------------------------------------------

TOK = r'[A-Za-z_][A-Za-z0-9_]*'
ITEM = rf'{TOK}\s*(=\s*[-A-Za-z0-9_ +]+?)?'
ENUM_RE = re.compile(rf'^{ITEM}(\s*,\s*{ITEM})+\s*,?$')


def split_tokens(line):
    out = []
    for raw in line.split(','):
        raw = raw.strip()
        if not raw:
            continue
        if '=' in raw:
            n, e = raw.split('=', 1)
            out.append((n.strip(), e.strip()))
        else:
            out.append((raw, None))
    return out


def parse_strings(path):
    with open(path, encoding='utf-8', errors='replace') as fh:
        lines = [l.rstrip('\n') for l in fh]
    groups, ui, enums = [], [], []
    cur = None
    for i, l in enumerate(lines, 1):
        s = l.strip()
        if s.startswith('kUIParam') and ',' in s:
            toks = [t for t, _ in split_tokens(s)]
            ui.append({'line': i, 'names': [t for t in toks if t != 'kNumParams']})
            continue
        if s.startswith('kParam') or s.startswith('kNumAutomatableParams = 0') or s.startswith('kNumParams ='):
            if not ENUM_RE.match(s) and not s.startswith('kNumAutomatableParams = 0'):
                continue
            toks = split_tokens(s)
            names = [t for t, _ in toks]
            is_priv_start = bool(toks) and (toks[0][1] == 'kNumAutomatableParams' or toks[0][0] == 'kNumParams')
            auto_end = ('kNumAutomatableParams' in names) and not is_priv_start
            if cur is not None and not cur['auto_done']:      # continuation of the automatable section
                cur['lines'].append(i)
                cur['tokens'] += toks
                if auto_end:
                    cur['auto_done'] = True
                if 'kNumParams' in names:
                    cur['closed_priv'] = True
                continue
            if is_priv_start and cur is not None and cur['auto_done'] and not cur['closed_priv']:
                cur['lines'].append(i)
                cur['tokens'] += toks
                cur['closed_priv'] = True
                continue
            cur = {'lines': [i], 'tokens': list(toks), 'auto_done': auto_end or is_priv_start,
                   'closed_priv': is_priv_start or ('kNumParams' in names)}
            groups.append(cur)
            continue
        if ENUM_RE.match(s) and len(split_tokens(s)) >= 3:
            toks = split_tokens(s)
            last = toks[-1][0]
            if any(e is not None for _, e in toks) or re.match(r'^(kNum|kCount|NumModes)', last):
                enums.append({'line': i, 'tokens': toks})
    out = []
    for g in groups:
        auto, priv, pos = [], [], 0
        in_priv, n_auto = False, None
        for name, expr in g['tokens']:
            if name == 'kNumAutomatableParams' and expr in (None, '0'):
                n_auto = len(auto) if expr is None else 0
                in_priv, pos = True, n_auto
                continue
            if name in ('kNumParams', 'kNumPrivateParams'):
                continue
            if expr == 'kNumAutomatableParams':
                if n_auto is None:
                    n_auto = len(auto)
                in_priv, pos = True, n_auto
            elif expr is not None and re.fullmatch(r'-?\d+', expr):
                pos = int(expr)
            (priv if in_priv else auto).append({'name': name, 'id': pos, 'automatable': not in_priv})
            pos += 1
        out.append({'lines': g['lines'], 'auto': auto, 'priv': priv})
    return lines, out, ui, enums

# --------------------------------------------------------------------------------------
# naming tables
# --------------------------------------------------------------------------------------

# Value-enum names. Key = (first token, second token).
ENUM_NAMES = {
    ('Off', 'ClipPlayer'): 'MidiOutMode',
    ('Last', 'First'): 'VoicePriority',
    ('Off', 'Sustain'): 'LimitSameNotePolyphony',
    ('Sample', 'White'): 'NoiseType',
    ('kRoutingDestFilter', 'kRoutingDestMaster'): 'RoutingDest',
    ('None', 'Up'): 'StrumType',
    ('Normal', 'Reverse'): 'ClipPlaybackMode',
    ('Off', 'Mono'): 'SpanKeyboardMode',
    ('kNone', 'kForward'): 'LoopMode',
    ('Semitones', 'Steps'): 'PitchMode',
    ('Continuous', 'Coarse'): 'PitchRatioModMode',
    ('Off', 'Auto'): 'SlicingMode',
    ('kDetuneLinear', 'kDetuneSuper'): 'DetuneMode',
    ('kNoUnisonStack', 'kOctave1'): 'UnisonStack',
    ('Pattern', 'Up'): 'ArpShape',
    ('Normal', 'NewOnly'): 'ArpStepAction',
    ('Normal', 'Phantom'): 'RangeWrapMode',
    ('kNoWarp', 'kDetune'): 'SpectralWarp',
    ('kNoWarp', 'kSync'): 'WTWarp',
    ('kWindowHann', 'kWindowWelch'): 'GranularWindowShape',
    ('kDensityFree', 'kDensityBPM'): 'GranularDensityMode',
    ('kLengthFree', 'kLengthBPM'): 'GranularLengthMode',
    ('kTogether', 'kEven'): 'GranularUnisonTrigPattern',
    ('Attribute', 'Automation'): 'ClipLaneType',
    ('Off', 'NotesUsed'): 'PianoRollNoteFold',
    ('Overdub', 'Extend'): 'ClipRecordMode',
    ('Free', 'Retrig'): 'LFOMode',
    ('Curve', 'Path'): 'LFOType',
    ('kAll', 'kContiguous'): 'PhaseMemoryMode',
    ('kSine', 'kRoundRect'): 'SubOscShape',
    ('kTube', 'kSoftClip'): 'DistortionMode',
    ('Bypass', 'MgL6'): 'FilterType',
    ('kPlate', 'kHall'): 'ReverbType',
    ('kOsc_WT', 'kOsc_MultiSample'): 'OscType',
    ('kAutoDest_CC0', 'kAutoDest_CC1'): 'AutomationDest',
}

# (module, param) -> enum name. Checked against observed strings (validation 3).
PARAM_ENUMS = {
    ('Global', 'kParamMidiOut'): 'MidiOutMode',
    ('Global', 'kParamVoicePriority'): 'VoicePriority',
    ('Arp', 'kParamVoicePriority'): 'VoicePriority',
    ('Global', 'kParamLimitSameNotePolyphony'): 'LimitSameNotePolyphony',
    ('NoiseOsc', 'kParamNoiseType'): 'NoiseType',
    ('RoutingSlot', 'kParamRoutingDest'): 'RoutingDest',
    ('MidiClip', 'kParamPlaybackMode'): 'ClipPlaybackMode',
    ('ArpClip', 'kParamPlaybackMode'): 'ClipPlaybackMode',
    ('MidiClip', 'kParamSpanKeyboardMode'): 'SpanKeyboardMode',
    ('Oscillator', 'kParamLoopMode'): 'LoopMode',
    ('Oscillator', 'kParamPitchMode'): 'PitchMode',
    ('Oscillator', 'kParamPitchRatioModMode'): 'PitchRatioModMode',
    ('Oscillator', 'kParamSlicingEnabled'): 'SlicingMode',
    ('Oscillator', 'kParamDetuneMode'): 'DetuneMode',
    ('Oscillator', 'kParamUnisonStack'): 'UnisonStack',
    ('Oscillator', 'kParamType'): 'OscType',
    ('ArpClip', 'kParamShape'): 'ArpShape',
    ('ArpClip', 'kParamTransposeShape'): 'ArpShape',
    ('ArpClip', 'kParamStepAction'): 'ArpStepAction',
    ('ArpClip', 'kParamRangeWrapMode'): 'RangeWrapMode',
    ('SpectralOsc', 'kParamWarpMenu'): 'SpectralWarp',
    ('SpectralOsc', 'kParamWarpMenu2'): 'SpectralWarp',
    ('SpectralOsc', 'kParamYAxisAssignment'): 'SpectralYAxis',
    ('WTOsc', 'kParamWarpMenu'): 'WTWarp',
    ('WTOsc', 'kParamWarpMenu2'): 'WTWarp',
    ('WTOsc', 'kParamPhaseMemory'): 'PhaseMemoryMode',
    ('GranularOsc', 'kParamWarpMenu'): 'GranularWarp',
    ('GranularOsc', 'kParamWarpMenu2'): 'GranularWarp',
    ('GranularOsc', 'kParamWindowShape'): 'GranularWindowShape',
    ('GranularOsc', 'kParamDensityMode'): 'GranularDensityMode',
    ('GranularOsc', 'kParamLengthMode'): 'GranularLengthMode',
    ('GranularOsc', 'kParamYAxisAssignment'): 'GranularYAxis',
    ('GranularOsc', 'kParamUnisonTrigPattern'): 'GranularUnisonTrigPattern',
    ('SampleOsc', 'kParamWarpMenu'): 'SampleWarp',
    ('SampleOsc', 'kParamWarpMenu2'): 'SampleWarp',
    ('MultiSampleOsc', 'kParamWarpMenu'): 'SampleWarp',
    ('MultiSampleOsc', 'kParamWarpMenu2'): 'SampleWarp',
    ('ClipPlayer', 'kParamRecordMode'): 'ClipRecordMode',
    ('LFO', 'kParamMode'): 'LFOMode',
    ('LFO', 'kParamType'): 'LFOType',
    ('SubOsc', 'kParamShape'): 'SubOscShape',
    ('FXDistortion', 'kParamMode'): 'DistortionMode',
    ('FXReverb', 'kParamType'): 'ReverbType',
    ('VoiceFilter', 'kParamType'): 'FilterType',
    ('FXFilter', 'kParamType'): 'FilterType',
}


def enum_name(e):
    toks = e['tokens']
    key = (toks[0][0], toks[1][0])
    if key == ('kYAxisNone', 'kYAxisOscVolume'):
        third = toks[2][0]
        for tag, nm in (('Spectral', 'SpectralYAxis'), ('Granular', 'GranularYAxis')):
            if tag in third:
                return nm
        return f"YAxis_L{e['line']}"
    if key == ('kNoWarp', 'kFilterLPF'):
        return None  # sample-family vs granular, assigned by order
    return ENUM_NAMES.get(key) or f"Enum_L{e['line']}"


# --------------------------------------------------------------------------------------
# build
# --------------------------------------------------------------------------------------

def build(strings_path, presets_root, cache=None):
    if cache and os.path.exists(cache):
        scan = pickle.load(open(cache, 'rb'))
    else:
        scan = scan_presets(presets_root)
        if cache:
            pickle.dump(scan, open(cache, 'wb'))
    lines, groups, ui, enum_decls = parse_strings(strings_path)
    notes = []

    # ---- value enums
    enums, enum_lines = {}, {}
    warp_plain = [e for e in enum_decls if (e['tokens'][0][0], e['tokens'][1][0]) == ('kNoWarp', 'kFilterLPF')]
    for e in enum_decls:
        nm = enum_name(e)
        if nm is None:
            nm = 'GranularWarp' if e is warp_plain[0] else 'SampleWarp'
        if nm in enums:
            nm = f"{nm}_L{e['line']}"
        enums[nm] = [t for t, _ in e['tokens']]
        enum_lines[nm] = e['line']
        odd = [f"{t}={x}" for i, (t, x) in enumerate(e['tokens']) if x is not None and not (i == 0 and x == '0')]
        if odd:
            notes.append(f"enum {nm}: declaration has explicit values {odd}; other entries are sequential from the "
                         f"previous value. List order = declaration order, sentinels (kNum*/kCount) kept."
                         + (" FilterType ordinal = list index - 1 (Bypass=-1, MgL6=0)." if nm == 'FilterType' else ""))
    if len(warp_plain) == 2 and enums.get('GranularWarp') == enums.get('SampleWarp'):
        notes.append("GranularWarp (line %d, declared before the granular params) and SampleWarp (line %d, declared "
                     "after the multisample params) are identical lists; SampleWarp is used for SampleOsc and "
                     "MultiSampleOsc by position only." % (warp_plain[0]['line'], warp_plain[1]['line']))

    # ---- module attribution
    types = sorted(set(scan['params']) | set(scan['inst']))
    obs_names = {t: set(scan['params'].get(t, {})) for t in types}

    def hint_for(g, window=8):
        first = g['lines'][0]
        for j in range(first - 1, max(first - window - 1, 0), -1):
            s = lines[j - 1].strip()
            if s in types or s == 'FXDimE':
                return s
        return None

    def names_of(g):
        return {p['name'] for p in g['auto'] + g['priv']}

    # Per type pick the best declaration group. A name-string hint only counts when it agrees with the
    # observed params (or when the type has no observed params). Groups may be shared by several types
    # (FXFilter and VoiceFilter share one class).
    OVERRIDES = {91203: ('FXSplitMS', "inferred: third of the consecutive 2/3/band splitter decls (91199 FXSplit, 91201 "
                                      "FXSplit3, 91203 FXSplitMS, fxTypeId 13,14,15); declaration has no private line "
                                      "so observed kParamModuleCount1/2 stay id=null")}
    assign = {}      # type -> (gi, cov, jac, hint, how)
    for t in types:
        o = obs_names[t]
        best = None
        for gi, g in enumerate(groups):
            gn = names_of(g)
            h = hint_for(g)
            cov = (len(o & gn) / len(o)) if o else 0.0
            jac = len(o & gn) / len(o | gn) if (o | gn) else 0.0
            if o:
                if cov < 0.75:
                    continue
                score = cov * 100 + jac * 10 + (5 if h == t else 0)
            else:
                if h != t:
                    continue
                score = 1000
            if best is None or score > best[0]:
                best = (score, gi, cov, jac, h)
        if best:
            assign[t] = (best[1], best[2], best[3], best[4], 'auto')
    for ln, (t, why) in OVERRIDES.items():
        gi = next(i for i, g in enumerate(groups) if g['lines'][0] == ln)
        assign[t] = (gi, 0.0, 0.0, hint_for(groups[gi]), 'override')
        notes.append(f"{t}: {why}")
    by_group = collections.defaultdict(list)
    for t, (gi, *_r) in assign.items():
        by_group[gi].append(t)
    for gi, ts in by_group.items():
        if len(ts) > 1:
            notes.append(f"Declaration at lines {groups[gi]['lines']} is shared by {sorted(ts)} (same C++ class); "
                         f"ids are identical for each.")
    evidence, unattributed = {}, []
    for t, (gi, cov, jac, h, how) in assign.items():
        g = groups[gi]
        ev = []
        if h == t:
            ev.append(f"name-string '{h}' precedes decl")
        elif h:
            ev.append(f"nearest preceding name-string '{h}' ignored")
        if how == 'override':
            ev.append("manual inference (see notes)")
        elif obs_names[t]:
            ev.append(f"observed-param coverage {cov:.2f}, jaccard {jac:.2f}")
        else:
            ev.append("no observed params")
        evidence[t] = dict(group=gi, lines=g['lines'], text='; '.join(ev), cov=cov, hint=h, how=how)
    if 'Macro' in assign and 'LFOPointModBus' in assign:
        notes.append("Macro (line %d, automatable kParamValue) vs LFOPointModBus (line %d, private kParamValue): both "
                     "declare only kParamValue = id 0 and have identical observed names; the split is by the "
                     "'LFOPointModBus' name-string preceding the second one and by automatable-ness (Macros are "
                     "host-automatable)." % (groups[assign['Macro'][0]]['lines'][0], groups[assign['LFOPointModBus'][0]]['lines'][0]))
    for gi, g in enumerate(groups):
        if gi not in by_group:
            h = hint_for(g)
            if g['lines'][0] == 90928 or (h == 'FXDimE' and gi == groups.index(g) and 'kParamDetune' in names_of(g)):
                notes.append("Unattributed group at lines %s ({Enable, Wet, Rate, Detune, Unison, Retrig, LevelOut}) is "
                             "a strict subset of FXHyperD's declaration - probably the base 'Hyper' effect class; the "
                             "nearby 'FXDimE' string belongs to the preceding DimE decl." % g['lines'])
            sample = ', '.join(sorted(names_of(g))[:6])
            unattributed.append(f"lines {g['lines']}: [{sample}...] ({len(names_of(g))} params), hint={h}")

    modules = {}
    for t, (gi, cov, jac, h, how) in assign.items():
        g = groups[gi]
        declared = {p['name'] for p in g['auto'] + g['priv']}
        plist = [{'name': p['name'], 'id': p['id'], 'automatable': p['automatable']}
                 for p in g['auto'] + g['priv']]
        for nm in sorted(set(scan['params'].get(t, {})) - declared):
            plist.append({'name': nm, 'id': None, 'automatable': None})
            notes.append(f"{t}.{nm} observed in presets but absent from every declaration (id=null).")
        modules[t] = {'params': plist}
    for t in types:
        if t not in modules:
            plist = [{'name': nm, 'id': None, 'automatable': None} for nm in sorted(scan['params'].get(t, {}))]
            modules[t] = {'params': plist}
            notes.append(f"{t}: no declaration group matched; {len(plist)} observed params have id=null.")
    # declared module with a name-string but never instantiated in presets (e.g. FXDimE)
    for gi, g in enumerate(groups):
        if gi not in by_group:
            h = hint_for(g)
            if h and h not in modules and h not in types:
                modules[h] = {'params': [dict(p) for p in g['auto'] + g['priv']]}
                evidence[h] = dict(group=gi, lines=g['lines'], text=f"name-string '{h}' precedes decl; no observed params",
                                   cov=0.0, hint=h, how='hint-only')
                unattributed = [u for u in unattributed if not u.startswith(f"lines {g['lines']}")]
                notes.append(f"{h}: declared but never instantiated in presets (paths empty).")

    # ---- paths / fx type ids
    for t, m in modules.items():
        paths = []
        for p, ids in sorted(scan['idx'].get(t, {}).items()):
            cols = list(zip(*ids)) if ids and ids[0] else []
            ci, out = 0, []
            for part in p.split('/'):
                if part.endswith('{}'):
                    vals = sorted(set(cols[ci]))
                    ci += 1
                    lo, hi = vals[0], vals[-1]
                    if vals != list(range(lo, hi + 1)):
                        notes.append(f"{t}: index gaps in {p}: {vals}")
                    out.append(part[:-2] + ('{%d}' % lo if lo == hi else '{%d..%d}' % (lo, hi)))
                else:
                    out.append(part)
            paths.append('/'.join(out))
        m['paths'] = paths
        if t in scan['fxtype']:
            ft = scan['fxtype'][t]
            m['fxTypeId'] = int(max(ft, key=ft.get))
            if len(ft) > 1:
                notes.append(f"{t}: multiple fxTypeIds seen {ft}")

    # ---- ModSlot ids per (module,param): current vs legacy
    ids_seen = collections.defaultdict(lambda: collections.defaultdict(collections.Counter))
    for (ty, name, pid, modid, ver) in scan['modslots']:
        ids_seen[(ty, name)][pid][ver] += 1
    for (ty, name), by_id in sorted(ids_seen.items()):
        m = modules.get(ty)
        cur = next((q['id'] for q in m['params'] if q['name'] == name), None) if m else None
        for pid, vers in by_id.items():
            if cur is not None and pid != cur:
                notes.append(f"{ty}.{name}: ModSlot destModuleParamID {pid} (schema id {cur}) seen only in preset "
                             f"version(s) {dict(vers)} - legacy id layout, not the 2.1.5 declaration.")

    # ---- per-param enum + observed
    for t, m in modules.items():
        for p in m['params']:
            en = PARAM_ENUMS.get((t, p['name']))
            if en:
                if en in enums:
                    p['enum'] = en
                else:
                    notes.append(f"PARAM_ENUMS {t}.{p['name']} -> {en}, but that enum was not found in the binary")
            o = scan['params'].get(t, {}).get(p['name'])
            if o:
                ob = {'count': o['count']}
                if o['min'] is not None:
                    ob['min'] = o['min']
                    ob['max'] = o['max']
                if o['strings']:
                    ob['strings'] = o['strings']
                p['observed'] = ob
                if o['strings'] and 'enum' not in p:
                    notes.append(f"{t}.{p['name']} has string values {o['strings'][:4]} but no enum assigned.")

    out_modules = {}
    for t in sorted(modules):
        m = modules[t]
        d = {'paths': m['paths']}
        if 'fxTypeId' in m:
            d['fxTypeId'] = m['fxTypeId']
        d['params'] = m['params']
        out_modules[t] = d

    linked = set(PARAM_ENUMS.values())
    for nm in enums:
        if nm not in linked:
            unattributed.append(f"enum {nm} (line {enum_lines[nm]}) not linked to any module param")

    ui_enums = {}
    for u in ui:
        hint = None
        for j in range(u['line'] - 1, max(u['line'] - 6, 0), -1):
            if lines[j - 1].strip() in types:
                hint = lines[j - 1].strip()
                break
        ui_enums[f"UI_L{u['line']}" + (f"_{hint}" if hint else "")] = u['names']

    schema = {
        'generatedFrom': {'serumVersion': SERUM_VERSION, 'presetsScanned': scan['presetsScanned']},
        'modules': out_modules,
        'enums': enums,
        'uiEnums': ui_enums,
        'unattributed': unattributed,
        'notes': notes,
    }
    return schema, scan, evidence

# --------------------------------------------------------------------------------------
# validation
# --------------------------------------------------------------------------------------

def validate(schema, scan, evidence):
    rep = []
    mods = schema['modules']
    tot = ok = 0
    mism, ex = collections.Counter(), {}
    bad_ver, all_ver = collections.Counter(), collections.Counter()
    for (ty, name, pid, modid, ver) in scan['modslots']:
        tot += 1
        all_ver[ver] += 1
        m = mods.get(ty)
        if m is None:
            k = 'unknown destModuleTypeString'
        else:
            p = next((q for q in m['params'] if q['name'] == name), None)
            if p is None:
                k = 'param name not in schema'
            elif p['id'] is None:
                k = 'schema id is null (undeclared)'
            elif p['id'] != pid:
                k = 'id mismatch'
            else:
                ok += 1
                continue
        mism[(k, ty, name)] += 1
        ex.setdefault((k, ty, name), pid)
        bad_ver[(k, ver)] += 1
    rep.append(f"[1] ModSlot (type,name)->id: {ok}/{tot} match ({100.0 * ok / max(tot, 1):.3f}%); mismatches={tot - ok}")
    for (k, ty, name), c in mism.most_common(40):
        rep.append(f"    {k}: {ty}.{name} x{c} (preset id {ex[(k, ty, name)]})")
    for (k, ver), c in sorted(bad_ver.items()):
        rep.append(f"      by version: {k} @ {ver}: {c} of {all_ver[ver]} modslots in that version")
    miss = []
    for t, ps in scan['params'].items():
        names = {p['name'] for p in mods[t]['params']}
        miss += [f"{t}.{n}" for n in ps if n not in names]
    undeclared = [f"{t}.{p['name']}" for t, m in mods.items() for p in m['params'] if p['id'] is None]
    rep.append(f"[2] observed names missing from schema: {len(miss)}; observed-but-undeclared (id=null): "
               f"{len(undeclared)} {undeclared}")
    bad, nchk = [], 0
    for t, m in mods.items():
        for p in m['params']:
            if 'enum' in p:
                allowed = set(schema['enums'][p['enum']])
                for s in p.get('observed', {}).get('strings', []):
                    nchk += 1
                    if s not in allowed:
                        bad.append(f"{t}.{p['name']}='{s}' not in {p['enum']}")
    rep.append(f"[3] enum containment: {nchk} (param,string) checks, violations={len(bad)} {bad}")
    sup = collections.defaultdict(lambda: [0, 0])
    for (ty, name, pid, modid, ver) in scan['modslots']:
        p = next((q for q in mods[ty]['params'] if q['name'] == name), None) if ty in mods else None
        sup[ty][1] += 1
        if p and p['id'] == pid:
            sup[ty][0] += 1
    rep.append("[4] attribution evidence (text; modslot id support = matching/total ModSlot refs):")
    for t, ev in sorted(evidence.items()):
        if t in sup:
            ev['text'] += f"; modslot ids {sup[t][0]}/{sup[t][1]}"

        weak = '' if (ev['hint'] == t or ev['cov'] >= 0.99) else '   <-- weak/inferred'
        rep.append(f"    {t:16s} lines {ev['lines']}: {ev['text']}{weak}")
    for t, m in mods.items():
        names = [p['name'] for p in m['params']]
        if len(names) != len(set(names)):
            rep.append(f"    DUPLICATE names in {t}")
        ids = [p['id'] for p in m['params'] if p['id'] is not None]
        if len(ids) != len(set(ids)):
            rep.append(f"    DUPLICATE ids in {t}")
    declared_ui = {n for v in schema['uiEnums'].values() for n in v}
    rep.append(f"[5] observed kUI* keys not in any uiEnum: {sorted(k for k in scan['uikeys'] if k not in declared_ui)}")
    return rep


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('strings', nargs='?', default=DEFAULT_STRINGS)
    ap.add_argument('--presets', default=DEFAULT_PRESETS)
    ap.add_argument('--out', default=DEFAULT_OUT)
    ap.add_argument('--cache', default=None, help='pickle cache of the preset scan')
    a = ap.parse_args()
    schema, scan, evidence = build(a.strings, a.presets, a.cache)
    os.makedirs(os.path.dirname(os.path.abspath(a.out)), exist_ok=True)
    with open(a.out, 'w') as fh:
        json.dump(schema, fh, indent=1)
        fh.write('\n')
    mods = schema['modules']
    print(f"presets scanned {scan['presetsScanned']}/{scan['presetFiles']}, errors {len(scan['errors'])}")
    print(f"modules {len(mods)}, params {sum(len(m['params']) for m in mods.values())}, "
          f"enums {len(schema['enums'])}, uiEnums {len(schema['uiEnums'])}")
    print('\n'.join(validate(schema, scan, evidence)))
    print(f"unattributed: {len(schema['unattributed'])}")
    for u in schema['unattributed']:
        print('   ', u[:300])
    print(f"notes: {len(schema['notes'])}")
    for n in schema['notes']:
        print('   ', n[:250])


if __name__ == '__main__':
    main()
