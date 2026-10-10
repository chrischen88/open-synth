"""Pure-numpy tests (no Serum needed). Run from tools/: python -m pytest serum_host/test_render.py"""
import numpy as np
import pytest

from serum_host import render as r

SR = r.SR


def sine(f, db=-6.0, secs=2.0, stereo=True):
    t = np.arange(int(SR * secs)) / SR
    x = (10 ** (db / 20) * np.sin(2 * np.pi * f * t)).astype(np.float32)
    return np.stack([x, x]) if stereo else x[None]


def test_parse_notes():
    assert r.parse_notes("48:0:1.5,55:0.5:1") == [(48, 0.0, 1.5), (55, 0.5, 1.0)]
    for bad in ("", "48:0", "x:0:1", "200:0:1", "48:-1:1", "48:0:0", "48:0:1:2"):
        with pytest.raises(r.InputError):
            r.parse_notes(bad)


def test_parse_macros():
    assert r.parse_macros("") == []
    assert r.parse_macros("0:50,7:100") == [(0, 0.5), (7, 1.0)]
    for bad in ("8:10", "0:101", "0:-1", "0", "a:b", "0:nan"):
        with pytest.raises(r.InputError):
            r.parse_macros(bad)


def test_sine_440():
    a = r.analyze(sine(440), SR, 0.0)
    assert a["fundamentalHz"] == pytest.approx(440, rel=0.01)
    assert a["spectralCentroidHz"] == pytest.approx(440, rel=0.05)
    assert a["peakDbfs"] == pytest.approx(-6.0, abs=0.1)
    assert a["stereoWidth"] == 0.0
    assert not a["silent"] and not a["clipping"]
    assert a["bands"]["lowmid"] > a["bands"]["high"] + 40
    assert len(a["envelope"]) == 40


def test_low_fundamental():
    assert r.fundamental_hz(sine(55), 0.0) == pytest.approx(55, rel=0.01)


def test_silence():
    a = r.analyze(np.zeros((2, SR)), SR, 0.0)
    assert a["silent"] and a["lufsIntegrated"] is None
    assert a["fundamentalHz"] is None and a["spectralCentroidHz"] is None


def test_square_clipping():
    t = np.arange(SR)
    x = np.where((t // 50) % 2 == 0, 1.0, -1.0).astype(np.float32)
    a = r.analyze(np.stack([x, x]), SR, 0.0)
    assert a["clipping"] and a["peakDbfs"] == pytest.approx(0.0, abs=0.01)


def test_lufs_1khz():
    # BS.1770: 0 dBFS 997 Hz sine on one channel = -3.01 LUFS, so a -20 dBFS sine on both
    # channels = -20.0 LUFS and on one channel = -23.0 LUFS (the "-20.7" figure ignores K-weighting gain).
    assert r.lufs_integrated(sine(1000, -20.0, 3.0)) == pytest.approx(-20.0, abs=0.3)
    one = sine(1000, -20.0, 3.0)
    one[1] = 0
    assert r.lufs_integrated(one) == pytest.approx(-23.0, abs=0.3)


def test_stereo_width():
    x = sine(440)[0]
    assert r.stereo_width(np.stack([x, 0 * x])) == pytest.approx(1.0, abs=1e-3)


def test_main_bad_input(tmp_path, capsys):
    with pytest.raises(r.InputError):
        r.run(["/nonexistent.SerumPreset", "--out", str(tmp_path)])
    p = tmp_path / "x.SerumPreset"
    p.write_bytes(b"x")
    with pytest.raises(r.InputError):
        r.run([str(p), "--out", str(tmp_path), "--notes", "bad"])
