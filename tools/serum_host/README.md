# serum_host

Host Serum 2 (VST3) headlessly with [pedalboard](https://github.com/spotify/pedalboard) to
inspect its state and measure how host parameters map to stored values.

Requirements: `pip install pedalboard cbor2 zstandard numpy mido` and Serum 2 installed
(`SERUM_VST3` overrides `/Library/Audio/Plug-Ins/VST3/Serum2.vst3`). Run from `tools/`.

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
  modules without the GUI; they are skipped. FX default plain values are visible in the
  controller state (`FXRackN.FX[i].defaultPlainValues`).

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
