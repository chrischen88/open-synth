# serum_host

Host Serum 2 (VST3) headlessly with [pedalboard](https://github.com/spotify/pedalboard) to
inspect its state and measure how host parameters map to stored values.

Setup: `tools/serum_host/setup.sh` creates `tools/serum_host/.venv` (gitignored) from the pinned
`requirements.txt` and prints the venv's python path. Idempotent. Requires Serum 2 installed
(`SERUM_VST3` overrides `/Library/Audio/Plug-Ins/VST3/Serum2.vst3`). Run modules from `tools/`.

## Render CLI

```
.venv/bin/python -m serum_host.render <preset.SerumPreset> --out <dir> \
    [--notes "48:0:1.5,55:0:1.5"] [--velocity 100] [--duration 2.5] [--macros "0:50,1:20"]
```

* `--notes` comma list of `midi:start_s:length_s` (default `48:0:1.5`); `--macros` `index:percent`
  (index 0..7 -> host `macro_1..8`, applied after preset injection). `--duration` defaults to
  `max(2.5, last note end + 0.5)`.
* Writes `<out>/render.wav` (16-bit stereo 44.1 kHz) and `<out>/spectrogram.png` (800x400, log frequency).
* stdout is exactly one JSON object: `wav`, `png`, `durationS`, `peakDbfs`, `rmsDbfs`, `lufsIntegrated`
  (BS.1770-4 K-weighting + gating, 400 ms blocks; null if below gate), `spectralCentroidHz`,
  `fundamentalHz` (autocorrelation on the first note; null if unclear; can land an octave off for
  detuned/sub-heavy patches), `stereoWidth` (side/mid RMS), `silent` (peak < -60 dBFS), `clipping`
  (peak >= 0.999), `bands` (sub/low/lowmid/highmid/high RMS dB), `envelope` (RMS dB per 50 ms),
  `serum` (`presetName` + display strings), `timing` (`loadS`, `renderS`). dB values floor at -120.
* Errors: stdout `{"error": "..."}`; exit 1 = bad input, 2 = runtime failure or 90 s timeout. Logs go to stderr.
* Plugin load is ~10 s; the render itself is ~0.2 s.
* Tests (no Serum needed): `.venv/bin/python -m pytest serum_host/test_render.py` from `tools/`.

| module     | purpose |
|------------|---------|
| `state.py` | JUCE `raw_state` (`VC2!` + XML + JUCE base64) and XferJson pack/unpack; CBOR encoding comes from `tools/serum2_preset.py`. `flatten`/`to_repo_path` turn state into repo-style addresses (`Oscillator0/WTOsc0.kParamTablePos`). |
| `host.py`  | `Serum` class: load, `set(name, raw)`, `settle()`, `processor_state()`, `reset()`, `set_processor_state()`, `inject_preset(path)` (overlay a `.SerumPreset` on the live state, read-only on the file), `render_notes()` and `write_wav()`. |
| `sweep.py` | `collect` / `fit` / `validate` / `spot` commands that produce `crates/osyn-params/data/serum2_units.json`. |

Things worth knowing:

* Parameter writes reach `raw_state` only after audio is processed (`settle`).
* Assigning `raw_state` resets host params to that state (~0.1 s vs ~5-10 s for a reload), so the
  sweep resets between every sample and side effects cannot leak.
* The state stores only non-default values; a missing key is the default.
* Some params are slow to settle (e.g. turning an oscillator off takes ~10 s).
* Host `fx_main_param_N` / `fx_bus_N_param_N` are generic slots that are not bound to the FX
  modules without the GUI; they are skipped. This holds for the "Serum 2 FX" plugin in the same bundle
  too (identical 2621 params, same generic slots; effect plugin needs audio input to `settle`), even with
  FX loaded in all three racks. FX default plain values are visible in the
  controller state (`FXRackN.FX[i].defaultPlainValues`).
* Because no knob->display measurement exists for FX params, `sweep fx` records only
  `fxDefaults` (controller defaults per FX type) and `"source": "observed"` entries
  (`FXRack0/FX[0]/<FXType>.<param>`, `transform: null`, `observedRange` from the schema). Run `fx` after
  `fit` (`fit` rewrites the file without FX entries): `python -m serum_host.sweep fx --units ... --schema ...`.

```
python -m serum_host.sweep collect  --cache /tmp/sweep.json          # resumable; ~10 min split over 6 processes (--shard k/6, one cache each; fit takes comma-separated caches)
python -m serum_host.sweep fit      --cache /tmp/sweep.json --out ../crates/osyn-params/data/serum2_units.json \
        --params-dump params.json
python -m serum_host.sweep validate --units ../crates/osyn-params/data/serum2_units.json \
        --schema ../crates/osyn-params/data/serum2_schema.json
python -m serum_host.sweep spot     --units ../crates/osyn-params/data/serum2_units.json --n 5
```

Transform types in `serum2_units.json`: `affine` (a+b·r), `power` (a+b·r^k), `exp` (a·b^r),
`round` (round(a+b·r)), `floor` (min(hi, a+floor(n·r))), `toggle`, `stepped` (numeric steps `[lo,hi,value,display]`),
`enum` (string tokens, same step layout; `law`/`n` when edges follow floor(n·r); token null = key omitted) and `table` (raw samples, nothing fit within 1e-4).

## analyze.py

```
python -m serum_host.analyze <audio> --out <dir> [--start S] [--end S]
python -m serum_host.analyze <reference> --compare <candidate> --out <dir> [--start S] [--end S]
```
Single mode returns render.py's analysis plus a spectrogram. Compare mode returns both analyses and a `diff`
(candidate minus reference): pitch (semitones), brightness (octaves), level-independent band balance, loudness,
stereo width, attack and decay, envelope correlation, the largest 1/3-octave differences, `similarity` (0–100) and
`hints`. It also writes `comparison.png`. Brightness uses a power-weighted spectral centroid, so 16-bit and noisy
files read correctly.
