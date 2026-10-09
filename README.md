# open-synth

A standalone, Serum 2–class synthesizer with a built-in MCP server. Its presets export as Serum 2
`.SerumPreset` files. See [`docs/PLAN.md`](docs/PLAN.md) for the architecture and roadmap.

Status: **Milestone A**, the patch model, MCP server and Serum 2 export. There is no audio engine yet.

## Crates

| Crate | Purpose |
|---|---|
| `osyn-serum` | `.SerumPreset` read/write. Re-encoding is byte-exact against Serum's factory library. |
| `osyn-params` | Serum 2 parameter registry: modules, param ids, enums and mod sources (`data/serum2_schema.json`) |
| `osyn-patch` | Editable patch model over Serum's module tree. Atomic commands with undo/redo, mod slots and FX racks. |
| `osyn-mcp` | MCP server (stdio) for designing presets with an AI agent |

## Use with Claude Code

```sh
cargo build --release -p osyn-mcp
claude mcp add open-synth -- "$PWD/target/release/osyn-mcp"
```

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

## Legal

open-synth is an independent project, not affiliated with Xfer Records. Serum is a trademark of Xfer Records.
Preset import/export is provided for interoperability with files you own. No Xfer content is distributed here.
