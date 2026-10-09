#!/usr/bin/env python3
"""Read/write Serum 2 .SerumPreset files (reference implementation for the exporter).

Container:
  b"XferJson\\0" | u64le len | metadata JSON | u32le cbor_len | u32le 2 | zstd(CBOR)
  metadata.hash = md5(zstd frame).hexdigest()
CBOR: map keys keep insertion order; floats are float32 when lossless, else float64 (never float16).

Requires: pip install cbor2 zstandard   (or the `zstd` CLI on PATH as a fallback)

Usage:
  serum2_preset.py dump   <in.SerumPreset> [out.json]
  serum2_preset.py build  <in.json> <out.SerumPreset>
  serum2_preset.py verify <dir>            # byte-exact CBOR round-trip over a library
"""
import hashlib
import json
import os
import struct
import subprocess
import sys

import cbor2

MAGIC = b"XferJson\x00"
META_KEYS = ("fileType", "hash", "presetAuthor", "presetDescription", "presetName",
             "product", "productVersion", "tags", "url", "vendor", "version")


def _zstd_decompress(data):
    try:
        import zstandard
        return zstandard.ZstdDecompressor().decompressobj().decompress(data)
    except ImportError:
        return subprocess.run(["zstd", "-dqc"], input=data, capture_output=True, check=True).stdout


def _zstd_compress(data, level=3):
    try:
        import zstandard
        return zstandard.ZstdCompressor(level=level, write_content_size=True).compress(data)
    except ImportError:
        return subprocess.run(["zstd", f"-{level}", "-qc"], input=data, capture_output=True, check=True).stdout


# --- CBOR encoder matching Serum's byte layout ------------------------------------------

def _head(major, n, out):
    if n < 24:
        out.append(major << 5 | n)
    elif n < 0x100:
        out += bytes((major << 5 | 24, n))
    elif n < 0x10000:
        out += bytes((major << 5 | 25,)) + struct.pack(">H", n)
    elif n < 0x100000000:
        out += bytes((major << 5 | 26,)) + struct.pack(">I", n)
    else:
        out += bytes((major << 5 | 27,)) + struct.pack(">Q", n)


def _enc(o, out):
    if o is None:
        out.append(0xF6)
    elif o is True:
        out.append(0xF5)
    elif o is False:
        out.append(0xF4)
    elif isinstance(o, int):
        _head(0, o, out) if o >= 0 else _head(1, -1 - o, out)
    elif isinstance(o, float):
        try:
            f32 = struct.pack(">f", o)
            lossless = struct.unpack(">f", f32)[0] == o or o != o
        except OverflowError:
            lossless = False
        out += b"\xFA" + f32 if lossless else b"\xFB" + struct.pack(">d", o)
    elif isinstance(o, str):
        b = o.encode()
        _head(3, len(b), out)
        out += b
    elif isinstance(o, (bytes, bytearray)):
        _head(2, len(o), out)
        out += o
    elif isinstance(o, (list, tuple)):
        _head(4, len(o), out)
        for x in o:
            _enc(x, out)
    elif isinstance(o, dict):
        _head(5, len(o), out)
        for k, v in o.items():
            _enc(k, out)
            _enc(v, out)
    else:
        raise TypeError(f"cannot encode {type(o).__name__}")


def cbor_dumps(o):
    out = bytearray()
    _enc(o, out)
    return bytes(out)


# --- container ---------------------------------------------------------------------------

def read(path):
    """Return (metadata dict, preset dict, raw cbor bytes)."""
    d = open(path, "rb").read()
    if not d.startswith(MAGIC):
        raise ValueError("not a SerumPreset (bad magic)")
    (n,) = struct.unpack_from("<Q", d, 9)
    meta = json.loads(d[17:17 + n])
    cbor_len, const = struct.unpack_from("<II", d, 17 + n)
    frame = d[17 + n + 8:]
    raw = _zstd_decompress(frame)
    if len(raw) != cbor_len:
        raise ValueError(f"size mismatch {len(raw)} != {cbor_len}")
    return meta, cbor2.loads(raw), raw


def write(path, preset, tags=()):
    """Serialize a preset dict. Metadata is derived from the preset's own top-level fields."""
    raw = cbor_dumps(preset)
    frame = _zstd_compress(raw)
    meta = {k: preset.get(k, "") for k in META_KEYS if k not in ("hash", "tags")}
    meta["hash"] = hashlib.md5(frame).hexdigest()
    meta["tags"] = list(tags)
    meta = {k: meta[k] for k in META_KEYS}  # Serum writes keys in sorted order
    mj = json.dumps(meta, separators=(",", ":"), ensure_ascii=False).encode()
    with open(path, "wb") as f:
        f.write(MAGIC + struct.pack("<Q", len(mj)) + mj + struct.pack("<II", len(raw), 2) + frame)


def _jsonable(o):
    if isinstance(o, bytes):
        return {"__bytes__": o.hex()}
    raise TypeError(type(o).__name__)


def _from_json(o):
    return bytes.fromhex(o["__bytes__"]) if set(o) == {"__bytes__"} else o


def main(argv):
    cmd = argv[1] if len(argv) > 1 else ""
    if cmd == "dump":
        meta, preset, _ = read(argv[2])
        text = json.dumps({"meta": meta, "preset": preset}, indent=1, default=_jsonable)
        open(argv[3], "w").write(text) if len(argv) > 3 else print(text)
    elif cmd == "build":
        doc = json.load(open(argv[2]), object_hook=_from_json)
        write(argv[3], doc["preset"], doc.get("meta", {}).get("tags", ()))
    elif cmd == "verify":
        ok = bad = 0
        for dp, _, fs in os.walk(argv[2]):
            for fn in fs:
                if not fn.endswith(".SerumPreset"):
                    continue
                p = os.path.join(dp, fn)
                try:
                    _, preset, raw = read(p)
                    if cbor_dumps(preset) == raw:
                        ok += 1
                    else:
                        bad += 1
                        print("MISMATCH", p)
                except Exception as e:  # noqa: BLE001 - report and continue over a library
                    bad += 1
                    print("ERROR", p, e)
        print(f"byte-exact: {ok}  failed: {bad}")
    else:
        print(__doc__)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
