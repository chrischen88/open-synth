"""Index tests on synthetic signals (no Serum needed). Run from tools/: python -m pytest serum_host/test_index.py"""
import json
import os

import numpy as np
import pytest

from serum_host import index as ix
from serum_host.host import write_wav

SR = ix.SR


def tone(f, harmonics=1, rolloff=1.0, decay=0.0, secs=2.0, stereo=True, db=-8.0):
    t = np.arange(int(SR * secs)) / SR
    x = sum(np.sin(2 * np.pi * f * h * t) / h ** rolloff for h in range(1, harmonics + 1) if f * h < 18000)
    x = x * np.exp(-decay * t)
    x = (10 ** (db / 20) * x / np.max(np.abs(x))).astype(np.float32)
    return np.stack([x, x]) if stereo else x[None]


def noise(secs=2.0, seed=0):
    r = np.random.default_rng(seed).standard_normal((2, int(SR * secs))).astype(np.float32)
    return 0.2 * r


def render_json(audio, note):
    return ix.features_to_json(ix.extract_features(audio), note)


def make_entry(path, tags, fn, silent=False):
    """fn(note) -> audio. Synthetic 'preset' rendered at the three index notes."""
    renders = []
    for n in ix.NOTES:
        if silent:
            renders.append(render_json(np.zeros((2, 2 * SR), np.float32), n))
        else:
            renders.append(render_json(fn(n), n))
    return {"path": path, "name": os.path.basename(path), "author": "t", "tags": tags,
            "transposeSemitones": ix.transposition(renders), "silent": silent, "renders": renders}


def midi_hz(n):
    return 440.0 * 2 ** ((n - 69) / 12)


@pytest.fixture()
def small_index(tmp_path):
    entries = [
        make_entry("a/saw_pluck.SerumPreset", ["Bass", "Pluck"], lambda n: tone(midi_hz(n), 12, 1.0, 4.0)),
        make_entry("a/sine_pad.SerumPreset", ["Pad"], lambda n: tone(midi_hz(n), 1, 1.0, 0.0)),
        make_entry("a/bright_buzz.SerumPreset", ["Lead", "Pluck"], lambda n: tone(midi_hz(n), 40, 0.3, 0.5)),
        make_entry("a/noise_fx.SerumPreset", ["FX"], lambda n: noise()),
        make_entry("a/silent_fx.SerumPreset", ["FX"], None, silent=True),
        # a 110 Hz-ish tone patch: plays 110 Hz at note 45, so notes 36/48/60 give 65/131/262 Hz
        make_entry("a/organ.SerumPreset", ["Keys"], lambda n: tone(midi_hz(n), 4, 1.5, 0.0)),
    ]
    p = tmp_path / "idx.json"
    p.write_text(json.dumps({"version": 1, "library": "/lib", "notes": list(ix.NOTES), "entries": entries}))
    return str(p)


def query_audio(tmp_path, audio, index, **kw):
    wav = str(tmp_path / "q.wav")
    write_wav(wav, audio)
    return ix.query(wav, index=index, **kw)


def test_features_sine():
    f = ix.extract_features(tone(440, db=-6.0))
    assert not f["silent"]
    assert f["f0Hz"] == pytest.approx(440, rel=0.01)
    assert f["centroidHz"] == pytest.approx(440, rel=0.05)
    assert f["width"] == 0.0
    assert f["flatness"] < 0.01
    assert f["spec"].shape == (ix.N_BANDS,) and np.argmax(f["spec"]) == 12  # band centred ~397 Hz
    assert f["env"].shape == (ix.ENV_POINTS,) and np.nanmax(f["env"]) == pytest.approx(0.0)
    assert f["bright"].shape == (ix.BRIGHT_SLICES,)
    assert np.all(np.abs(f["bright"] - 440) < 60)


def test_features_noise_vs_tone_and_decay():
    n = ix.extract_features(noise())
    assert n["flatness"] > 0.2 and n["f0Hz"] is None
    d = ix.extract_features(tone(220, decay=3.0))
    assert d["env"][0] == pytest.approx(0.0) and d["env"][-1] < -20
    assert ix.extract_features(tone(220, 1))["env"][-1] > -1


def test_features_stereo_width_and_brightness_curve():
    t = np.arange(2 * SR) / SR
    a = np.stack([np.sin(2 * np.pi * 300 * t), np.sin(2 * np.pi * 330 * t)]).astype(np.float32) * 0.3
    assert ix.extract_features(a)["width"] > 0.5
    # brightness rising over time: 300 Hz for 1 s then 3 kHz
    x = np.where(t < 1, np.sin(2 * np.pi * 300 * t), np.sin(2 * np.pi * 3000 * t)).astype(np.float32) * 0.3
    b = ix.extract_features(np.stack([x, x]))["bright"]
    assert b[0] < 400 and b[-1] > 2500


def test_silent_and_short_reference():
    f = ix.extract_features(np.zeros((2, SR), np.float32))
    assert f["silent"]
    short = ix.extract_features(tone(440, secs=0.5))
    assert np.isnan(short["env"][-1]) and np.isnan(short["bright"][-1])


def test_query_finds_own_entry_first(small_index, tmp_path):
    # 110 Hz, harmonic series like the 'organ' entry (which plays 130.8 Hz at note 48)
    r = query_audio(tmp_path, tone(110, 4, 1.5), small_index, top=3)
    assert r["reference"]["fundamentalHz"] == pytest.approx(110, rel=0.02)
    assert r["results"][0]["path"] == "a/organ.SerumPreset"
    assert r["results"][0]["similarity"] > 60


def test_exact_render_ranks_first_and_octave_shift_top3(small_index, tmp_path):
    for name, fn in (("a/saw_pluck.SerumPreset", lambda n: tone(midi_hz(n), 12, 1.0, 4.0)),
                     ("a/bright_buzz.SerumPreset", lambda n: tone(midi_hz(n), 40, 0.3, 0.5))):
        r = query_audio(tmp_path, fn(48), small_index)
        assert r["results"][0]["path"] == name and r["results"][0]["note"] == 48
        assert r["results"][0]["similarity"] > 90
        # a note not in the index, an octave and a fifth off: still the same preset
        r = query_audio(tmp_path, fn(55 + 12), small_index)
        assert name in [x["path"] for x in r["results"][:3]]
        r = query_audio(tmp_path, fn(40), small_index)
        assert name in [x["path"] for x in r["results"][:3]]


def test_tag_filter_and_silent_exclusion(small_index, tmp_path):
    audio = tone(midi_hz(48), 12, 1.0, 4.0)
    r = query_audio(tmp_path, audio, small_index, top=10, tags=["pluck"])
    assert {x["path"] for x in r["results"]} == {"a/saw_pluck.SerumPreset", "a/bright_buzz.SerumPreset"}
    r = query_audio(tmp_path, audio, small_index, top=10, tags=["Pluck", "Lead"])
    assert [x["path"] for x in r["results"]] == ["a/bright_buzz.SerumPreset"]
    r = query_audio(tmp_path, audio, small_index, top=10)
    assert "a/silent_fx.SerumPreset" not in [x["path"] for x in r["results"]]
    assert len(r["results"]) == 5
    assert query_audio(tmp_path, audio, small_index, tags=["nonexistent"])["results"] == []


def test_result_shape(small_index, tmp_path):
    r = query_audio(tmp_path, tone(midi_hz(48), 12, 1.0, 4.0), small_index, top=1)
    x = r["results"][0]
    assert x["absolutePath"] == "/lib/" + x["path"]
    assert set(x["details"]) >= {"pitchSemitones", "brightnessOctaves", "bandBalanceDb", "envelopeCorrelation",
                                 "flatnessDiff", "widthDiff"}
    json.dumps(r, allow_nan=False)


def test_errors(small_index, tmp_path):
    with pytest.raises(ix.InputError):
        ix.query(str(tmp_path / "missing.wav"), index=small_index)
    with pytest.raises(ix.InputError):
        ix.query(str(tmp_path / "x.wav"), index=str(tmp_path / "nope.json"))
    with pytest.raises(ix.InputError):
        query_audio(tmp_path, np.zeros((2, SR), np.float32), small_index)


def test_index_cache_reload(small_index):
    a = ix.load_index(small_index)
    assert ix.load_index(small_index) is a
    d = json.load(open(small_index))
    d["entries"] = d["entries"][:2]
    with open(small_index, "w") as f:
        json.dump(d, f)
    os.utime(small_index, (1e9, 1e9 + 5))
    assert len(ix.load_index(small_index).entries) == 2


def test_status(tmp_path):
    p = str(tmp_path / "i.json")
    assert ix.status(p) == {"exists": False}
    with open(p + ".progress.json", "w") as f:
        json.dump({"done": 1, "total": 2}, f)
    assert ix.status(p)["done"] == 1


def test_transposition():
    rs = [render_json(tone(midi_hz(n) * 2, 3), n) for n in ix.NOTES]
    assert ix.transposition(rs) == 12


def test_search_terms_cover_folders_and_name_prefixes():
    from serum_host.index import search_terms
    t = search_terms({"path": "Factory/Bass/Misc/BA - Perfect Pluck.SerumPreset", "name": "BA - Perfect Pluck", "tags": ["Mono"]})
    assert {"mono", "factory", "bass", "misc"} <= t
    t = search_terms({"path": "User/x/SN_PL_bubble.SerumPreset", "name": "SN_PL_bubble", "tags": []})
    assert "pluck" in t
