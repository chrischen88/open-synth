"""Serum 2 state containers.

pedalboard's ``raw_state`` for Serum 2 (a JUCE VST3 wrapper) is::

    b"VC2!" | u32le xml_len | XML | NUL
    XML = <VST3PluginState><IComponent>B64</IComponent><IEditController>B64</IEditController>...

``B64`` is JUCE's MemoryBlock base64 ("<nbytes>.<chars>", LSB-first 6-bit groups, custom
alphabet).  Each decoded blob is an XferJson container (the same layout as a .SerumPreset):
``XferJson\\0 | u64le meta_len | meta JSON | u32le cbor_len | u32le 2 | zstd(CBOR)``.
The IComponent blob is the processor state (module/param tree), IEditController is UI state.

CBOR encoding is delegated to ``tools/serum2_preset.py`` so output matches Serum's layout.
"""
import hashlib
import json
import os
import re
import struct
import sys

import cbor2
import zstandard

_TOOLS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if _TOOLS not in sys.path:
    sys.path.insert(0, _TOOLS)
import serum2_preset as sp  # noqa: E402

_ALPHA = ".ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+"
_INDEX = {c: i for i, c in enumerate(_ALPHA)}


def b64_dec(s):
    """Decode JUCE MemoryBlock base64 ("<n>.<chars>")."""
    n, _, body = s.partition(".")
    n = int(n)
    out = bytearray(n)
    for ci, c in enumerate(body):
        v = _INDEX[c]
        for b in range(6):
            pos = ci * 6 + b
            if pos >= n * 8:
                break
            if v >> b & 1:
                out[pos >> 3] |= 1 << (pos & 7)
    return bytes(out)


def b64_enc(data):
    n = len(data)
    chars = []
    for ci in range((n * 8 + 5) // 6):
        v = 0
        for b in range(6):
            pos = ci * 6 + b
            if pos < n * 8 and data[pos >> 3] >> (pos & 7) & 1:
                v |= 1 << b
        chars.append(_ALPHA[v])
    return f"{n}." + "".join(chars)


def parse_raw_state(raw):
    """raw_state bytes -> (IComponent blob, IEditController blob) as XferJson bytes."""
    if raw[:4] != b"VC2!":
        raise ValueError("unexpected raw_state header")
    xml = raw[8:].decode()
    comp = re.search(r"<IComponent>(.*?)</IComponent>", xml, re.S).group(1)
    ctrl = re.search(r"<IEditController>(.*?)</IEditController>", xml, re.S).group(1)
    return b64_dec(comp), b64_dec(ctrl)


def build_raw_state(comp, ctrl):
    xml = ('<?xml version="1.0" encoding="UTF-8"?> <VST3PluginState><IComponent>%s</IComponent>'
           "<IEditController>%s</IEditController></VST3PluginState>" % (b64_enc(comp), b64_enc(ctrl))).encode()
    return b"VC2!" + struct.pack("<I", len(xml)) + xml + b"\x00"


def unpack(blob):
    """XferJson bytes -> (meta dict, decoded CBOR object)."""
    if not blob.startswith(sp.MAGIC):
        raise ValueError("not an XferJson blob")
    (n,) = struct.unpack_from("<Q", blob, 9)
    meta = json.loads(blob[17:17 + n])
    frame = blob[17 + n + 8:]
    raw = zstandard.ZstdDecompressor().decompressobj().decompress(frame)
    return meta, cbor2.loads(raw)


def pack(meta, obj):
    """(meta dict, CBOR-able object) -> XferJson bytes with a fresh md5 hash in the metadata."""
    cbor_bytes = sp.cbor_dumps(obj)
    frame = zstandard.ZstdCompressor(level=3, write_content_size=True).compress(cbor_bytes)
    meta = dict(meta)
    meta["hash"] = hashlib.md5(frame).hexdigest()
    mj = json.dumps(meta, separators=(",", ":")).encode()
    return sp.MAGIC + struct.pack("<Q", len(mj)) + mj + struct.pack("<II", len(cbor_bytes), 2) + frame


def decode_raw_state(raw):
    """raw_state -> (processor meta, processor obj, controller meta, controller obj)."""
    c, e = parse_raw_state(raw)
    cm, co = unpack(c)
    em, eo = unpack(e)
    return cm, co, em, eo


def flatten(o, prefix=""):
    """Nested dict/list -> {"/A/B[0]/C": leaf}."""
    out = {}
    if isinstance(o, dict):
        for k, v in o.items():
            out.update(flatten(v, f"{prefix}/{k}"))
    elif isinstance(o, list):
        for i, v in enumerate(o):
            out.update(flatten(v, f"{prefix}[{i}]"))
    else:
        out[prefix] = o
    return out


def to_repo_path(flat_key):
    """"/Oscillator0/WTOsc0/plainParams/kParamTablePos" -> "Oscillator0/WTOsc0.kParamTablePos"."""
    parts = [p for p in flat_key.split("/") if p and p != "plainParams"]
    return "/".join(parts[:-1]) + "." + parts[-1] if len(parts) > 1 else flat_key
