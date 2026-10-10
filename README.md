# open-synth

A standalone, Serum 2–class synthesizer with a built-in MCP server. Its presets export as Serum 2
`.SerumPreset` files. See [`docs/PLAN.md`](docs/PLAN.md) for the architecture and roadmap.

Status: **Milestone A**, the patch model, MCP server and Serum 2 export. There is no audio engine yet.

## Crates

| Crate | Purpose |
|---|---|
| `osyn-serum` | `.SerumPreset` read/write. Re-encoding is byte-exact against Serum's factory library. |
| `osyn-params` | Serum 2 parameter registry: modules, param ids, enums and mod sources (`data/serum2_schema.json`). Also knob ↔ stored value ↔ display conversions for 455 params (`data/serum2_units.json`), measured by hosting Serum. |
| `osyn-patch` | Editable patch model over Serum's module tree. Atomic commands with undo/redo, mod slots and FX racks. |
| `osyn-wt` | Wavetables: Serum `clm` WAV read/write, band-limited shapes, additive spectra and formula tables |
| `osyn-mcp` | MCP server (stdio) for designing presets with an AI agent |

## Use with Claude Code

```sh
cargo build --release -p osyn-mcp
claude mcp add open-synth -- "$PWD/target/release/osyn-mcp"
```

Values can be given the way Serum displays them (`"800 Hz"`, `"5 ms"`, `"-6 dB"`, `"Acid Ladder"`), as a
knob position (`{"knob": 0.5}`), or as the raw stored number. `get_patch` shows both.

Other tools:
- **Wavetables:** `list_wavetables`, and `set_wavetable`, which takes a Serum library table, an imported WAV,
  morphing shapes, harmonic keyframes or a formula. Generated tables are embedded in the preset.
- **LFOs:** `set_lfo_shape` draws a shape.
- **Auditioning:** `render` plays the patch through your installed Serum 2. It returns loudness, spectrum, an
  envelope and a spectrogram image, and needs `tools/serum_host/setup.sh` to have been run once.

Then ask for a sound, for example "make a detuned reese bass with an LFO on the filter and export it". Bare export
names are saved to Serum 2's `Presets/User/open-synth/` folder, so they appear in Serum's browser.

## Tests

```sh
cargo test --workspace
```

`osyn-serum`'s factory round-trip test runs against the local Serum 2 preset library when it's installed
(override the location with `SERUM2_PRESETS_DIR`), and is skipped otherwise.

## Tools

`tools/` contains the Python reference implementations used to reverse-engineer the format:
- `serum2_preset.py`: container read/write.
- `serum2_schema.py`: observed-parameter dump.
- `serum2_extract_schema.py`: builds `serum2_schema.json` from the plugin binary's strings and the preset library.
- `serum_host/`: hosts the installed Serum 2 headlessly via pedalboard. It measures parameter mappings (`sweep.py`)
  and checks exported presets in real Serum (`python -m serum_host.verify <preset>`, run from `tools/`).

## Legal

open-synth is an independent project, not affiliated with Xfer Records. Serum is a trademark of Xfer Records.
Preset import/export is provided for interoperability with files you own. No Xfer content is distributed here.
