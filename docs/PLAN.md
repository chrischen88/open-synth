# open-synth — Plan for a Serum 2–class synthesizer

## 1. What Serum 2 actually is (from inspecting the local install)

Installed: **Serum 2 v2.1.5** — `/Library/Audio/Plug-Ins/VST3/Serum2.vst3` and `Components/Serum2.component`
(universal x86_64/arm64, 33 MB). Two plugin classes: **"Serum 2"** (instrument) and **"Serum 2 FX"** (effect rack only).
It is built on the **Steinberg VST3 SDK 3.7.12 + VSTGUI**, not JUCE. It links Accelerate (FFT/vector math) and
libxml2 (skins). Content lives in `/Library/Audio/Presets/Xfer Records/Serum 2 Presets/` (Tables, Multisamples
(SFZ + FLAC), Samples, LFO Shapes/Paths, Curves, Clips, Arp Patterns, Effect Chains, Impulses, Skins, ScaleDefinitions.json,
and a SQLite `presets.db` browser index).

### 1.1 File formats (decoded)

| File | Layout |
|---|---|
| `.SerumPreset` | `"XferJson\0"` · u64 LE JSON length · JSON metadata (name, author, tags, hash, productVersion) · u32 uncompressed size · u32 (=2) · **zstd** frame → **CBOR** map of modules |
| Wavetable `.wav` | 32-bit float mono WAV, `clm ` chunk `"<!>2048 …"` = 2048 samples/frame; frames = length / 2048 |
| Multisample | `.sfz` + FLAC |
| Others | `.SerumFX`, `.SerumFXRack`, `.XferShape` (LFO/curves), `.XferClip` (MIDI clips) |

The decoded CBOR is effectively Serum 2's whole architecture, module by module.
`tools/serum2_schema.py` (needs `pip install cbor2` + `zstd` CLI) dumped the union across all 1,391 installed presets → `docs/serum2-param-schema.txt`.

### 1.2 Engine architecture (from the preset schema)

**Oscillators** (`Oscillator0..4`): 3 main oscillators (A/B/C), Noise (N) and Sub (S). Each main oscillator can be
one of five engines: `kOsc_WT`, `kOsc_Sample`, `kOsc_MultiSample`, `kOsc_Granular` and `kOsc_Spectral`.

Common oscillator controls:
- Pitch: octave, coarse, fine, Hz offset, pitch mode (Freq / Ratio / Harmonics), and pitch-tracking/bend options.
- Unison: count, detune with 4 detune modes (Super, Exp, Inv, Random), width, blend span and range, and 7 stack modes
  (center±12, octave 1–3, octave+fifth 1–3). Unison can also spread the WT position and the warp amounts.
- Key and velocity zones.
- Sample-oriented controls: loop modes (Forward, PingPong, Reverse, Tailed), crossfade, slicing (auto or manual) and a
  tempo-locked scan.

Every oscillator engine has **two warp slots** (`WarpMenu`/`WarpMenu2` + amount + variation):
- **WT warps (57)**: sync, PWM, bend ±, asym ±, flip, mirror/remap 1–4, quantize, even/odd, DLM, and the cross-modulation
  family **FM / FMP / FMX / PD / AM / RM**. Each cross-mod source can be another osc (OSC, OSC2), the sub, noise or a
  filter output (FILT1/2). The list also has self-PD, an in-osc LPF/HPF and 13 waveshapers (tube, diode, fold, tape, …).
- **Spectral warps** (FFT domain, in addition): spectral shift and pitch-shift, comb, smear, spread, mirror, gate,
  phase twist, robotize, add harmonics or subharmonics, peak follow/octave/harmonic, Shepard, mask, and vocode.
- **Granular**: density (free/BPM), grain length, max grains, 8 window shapes, and per-grain randomization of offset,
  pitch, pan, gain, direction, warp and window.
- **WT**: table position, crossfade mode, phase memory (per-voice or contiguous), random phase.
  **Sub**: 5 shapes. **Noise**: White, Pink, Brown, Geiger, one-shot sample noises.

**Filters**: 2 voice filters (`VoiceFilter0/1`) plus an FX-rack filter. There are about 98 types: SVF LP/HP/BP/notch in
6–24 dB slopes, ladders (Mg, Acid, EMS), Diode (`DirtyMg`), combs and flangers (±, HL), phasers (12–48), formants,
Scream, ring-mod, sample-and-hold, DJ mixer, a reverb-in-filter, `PZ_SVF` (pole-zero, user-drawable; there is a
`PZ Filter` content folder) and `ZDF_A`. Every filter has freq, reso, drive, var, stereo, key-track, and an X/Y morph.

**Routing** (`RoutingSlot0..6`): each osc goes to Filter, Direct or Master. It also has a filter balance, FX bus 1/2
sends, and a "via Env1" option.

**Modulation**:
- 4 envelopes: DAHDSR with start/end levels and 3 curve controls, plus beat sync.
- 10 LFOs: drawable curves with grid, smooth, rise, delay, phase and swing. Modes are Free and Envelope; types are
  RandomSH, **Lorenz** and **Rössler** chaos, and **Path** (2-D).
- 8 macros, 16 "LFO point" mod buses (LFO points that can themselves be modulated), and **64 mod slots**.
- Each mod slot has a source `[type, index]`, a destination (module type, id, param), amount, bipolar, an aux source,
  in/out curves, rise/fall smoothing, a delay offset and bypass.
- Per-voice randomization (VoicePanel): random detune, pan, cutoff and env time per voice index, plus Mod1/Mod2 values
  per voice.
- Note and velocity scaling curves (`scalars/note`, `scalars/velo`) and a pitch quantizer (key + scale).

**Performance**:
- Arpeggiator: 12 clips, 15 shapes (Played, Up/Down variants, Converge, Rand variants, Thumb, Chord, Pattern), chance,
  repeats, transpose shapes, gate, swing and retrigger.
- **Clip player**: 12 MIDI clips with piano roll and playback modes.
- MPE, poly/mono/legato, portamento with curve, voice priority, same-note polyphony, global tuning (MTS-ESP) and
  oversampling with "ultra on render".

**FX** (`FXRack0..2` = Main + Bus 1 + Bus 2): Distortion (17 modes), EQ, Filter, Chorus, Flanger, Phaser, Delay,
Reverb (UI: Plate, Hall, Vintage, Nitrous; enum names kHall/kSpace/kVintage/kAbyss), Convolution, Compressor (with multiband / up-down), Hyper/Dimension,
Bode freq-shifter and Utility. There are also **Split** containers (2-band, 3-band, Mid/Side) that hold nested sub-chains.
FX params carry their own modulation curves (`flex`).

### 1.3 Additional facts from the official manual (v2.0.18 guide, xferrecords.com/manual/serum-2)

- **Unison**:
  - Up to 16 voices.
  - Modes: Linear, Super, Exp, Inv, Random.
  - Blend defaults to 75%.
  - Per-voice start phases can be edited (Phase Memory: All / Contiguous / Per Voice).
- **Pitch tracking off**: a WT oscillator plays MIDI note 0, so it can be used as an LFO or modulator. A modulator
  oscillator can be fully muted and still drive FM/PM/AM/RM.
- **Envelopes**: ENV 1 is hard-wired to the amp. Stages are AHDSR with editable A/D/R curves, in ms or BPM, with a
  Legato Inverted option.
- **LFOs**:
  - Modes: Free, Retrig, Envelope (one-shot with a loop-back point).
  - Direction: Forward, Reverse, Ping-pong. Poly by default.
  - Rates up to about 1 kHz with ×10. Shapes are `.XferShape` files, shared with LFOTool.
- **Mod matrix sources** (49): includes Active Voices, Note-On Alt 1/2, Mod Wheel, channel and poly aftertouch, Pitch
  Bend, Fixed and MPE. Oscillators and filters can also be mod sources. Macros can be mod destinations.
- **Filters**: the S2 additions are Diffusor, MG Ladder (transistor) and Acid Ladder (diode). The filter display shows
  magnitude, magnitude + FFT, and phase + FFT.
- **Mixer**: each source sends to F1, F2, Bus 1, Bus 2, Main or Direct. Buses can route to Main, Direct or each other.
- **Voicing**: poly count includes unison voices *and* granular grains. Steal priority options are Newest, Oldest,
  Highest, Lowest and Velocity. Quality settings are Draft 1×, High 2×, Ultra 4× (with optional Ultra on render).
  Other options: S1-compat mode and Disable Smoothing (sample-accurate automation).
- **Voice Control**: a 1–8 step per-voice sequence of pan, detune, cutoff and envelope randomization.
- **Tuning**: `.tun` files take precedence over MTS-ESP.
- **Formats**: VST3, AU and AAX on macOS and Windows. There is no Linux build.
- **Wavetable editor**:
  - Up to 256 frames of 2048 samples. Morph frames are generated at load time; only the morph *type* is stored.
  - Morph types: Crossfade, Spectral, Spectral (zero fundamental phase), Spectral (zero all phases).
  - Other features: FFT bin editing (amplitude and phase) and a formula parser.
  - Processing: normalize, flip, X-fade edges, filter, redux, set/subtract/blur spectra, create PWM, sort.
  - Imports: multi-cycle audio, single cycles, and images. A `.txt` override sets samples per cycle (32–9999).
  - Exports: `.256`, 16/32-bit WAV.
- **Community format work**: covers the preset container but not the CBOR schema. It includes
  `github.com/KennethWussmann/serum-preset-packager` (Python), `github.com/CharlesBT/node-serum2-preset-packager`
  (TS), and `gist.github.com/0xdevalias/135a18e979ac8e302ebbc700a50a8d74`.

Implications for us:
- The unison renderer must handle 16 voices per oscillator.
- The "osc as modulator" path needs a muted-but-rendered oscillator.
- The poly budget must count unison voices and grains together.
- Morph frames should be regenerated from a stored morph type, not saved.

## 2. Product (revised 2026-10-09)

**open-synth is an AI-assisted Serum 2 preset designer.** Claude, through MCP, builds, auditions and refines Serum 2
presets, including recreating reference sounds. The user's installed Serum 2 is the sound engine: patches are
rendered through it for auditioning and comparison, and the output is a `.SerumPreset`.

Building our own Serum-class synth engine (formerly Milestones B–M, about 12–15 months) is **deferred**. It only
pays off if the product must make sound without Serum. Sections 3–4 below describe that original standalone-synth
design and are kept for reference.


*(Original direction:)* open-synth as a standalone desktop synthesizer. It has its own **Serum 2–level** engine (full feature parity is the target), a full sound-design UI and a
built-in **MCP server**, so AI agents (Claude Code, Claude Desktop, …) can build patches. Its main output is a
**Serum 2 preset** (`.SerumPreset`) that loads in Serum. It is not a plugin, although the engine is kept host-agnostic
so a CLAP/VST3 build stays possible later.

## 3. Technology choice

Being standalone-only changes the earlier JUCE vs. Rust trade-off. AU/AAX no longer matter, and MCP support becomes a
core requirement.

| Layer | Choice | Why |
|---|---|---|
| DSP engine | **Rust** crate `osyn-engine` | Memory/thread safety on the audio thread. No allocation in render (enforced with `assert_no_alloc` in debug). SIMD via `wide`. |
| Audio / MIDI I/O | `cpal` (CoreAudio/WASAPI/ALSA), `midir` | Mature cross-platform crates |
| App shell + UI | **Tauri 2** + TypeScript (Svelte or React) + Canvas/WebGL | A web UI is the fastest way to build the dense custom UI: wavetable 3-D view, curve editors, drag-to-modulate, mod matrix. The Rust backend is in-process with the engine. |
| MCP | Official Rust SDK **`rmcp`** | First-class MCP in the same process as the engine. C++ has no official SDK, which was decisive against JUCE here. |
| Preset I/O | `ciborium` + `zstd` + `md-5` | Port of `tools/serum2_preset.py` (byte-exact reference) |
| DSP helpers | `realfft`/`rustfft`, `hound` (WAV), `symphonia` (FLAC/other audio) | |
| Tests | `cargo test`, `insta` snapshots, `criterion` benches | |
| License | **MIT/Apache-2.0** (permissive; no JUCE constraint) | |

Alternative considered: **JUCE standalone**. Its UI toolkit is mature, but there's no MCP SDK (we'd hand-roll
JSON-RPC and HTTP), and custom visualisations in JUCE are slower to build than in a web UI. Not chosen.

## 4. Architecture

```
open-synth/                      (cargo workspace)
  crates/
    osyn-params/     # param registry: Serum-mirrored modules/ids/units/enums, generated from the schema dump
    osyn-patch/      # Patch model (typed tree mirroring Serum's CBOR), undo/redo command log, validation
    osyn-serum/      # .SerumPreset read/write + value mapping tables; import + export
    osyn-engine/     # DSP: osc (WT mipmaps, warps, sub, noise, sample, granular, spectral), filters, mod, fx, voices
    osyn-render/     # offline render + analysis (RMS/LUFS, spectrum, spectrogram PNG) for tests and MCP
    osyn-mcp/        # MCP server (rmcp): tools/resources over the patch command bus
  app/               # Tauri app: Rust backend (audio thread, MIDI, MCP host) + web UI (ui/)
  tools/             # Python reference tools (serum2_preset.py, serum2_schema.py)
  docs/
```

Key design rules:
- **One command bus.** UI edits, MCP tool calls and MIDI-learn all become `PatchCommand`s, such as `SetParam`, `AddMod`
  or `LoadTable`. They are validated against the registry, recorded for undo, and published to the audio thread
  through a lock-free queue. The UI and AI share one source of truth, so AI edits are visible live and undoable.
- **Patch model mirrors Serum.** Modules, ids, plain units and enum names follow Serum 2, so export is plain
  serialization (see §5). Engine-internal values are derived from the patch, not the other way round.
- **Block-based modulation.** Mod sources are computed per voice per 32-sample sub-block. Audio-rate paths (FM/PM/AM/RM
  warps, filter FM) use per-sample buffers.
- **Fixed voice graph.** The order is OscA/B/C/N/S, then 2 warp slots each, then routing, then Filter 1/2, then amp,
  then the FX buses and the main rack. Everything is preallocated for max poly × 16 unison.
- **Wavetables.** 2048-sample frames, up to 256 frames. Band-limited mip levels per frame are built by FFT on a
  background thread and swapped in atomically. Morph frames are generated from the stored morph type. Harmonic-
  generating warps run with 2–4× oversampling.

## 4a. MCP server

- **Transports:**
  - **Streamable HTTP** on `127.0.0.1` while the app is open, so an agent edits the patch you're hearing:
    `claude mcp add --transport http open-synth http://127.0.0.1:<port>/mcp`
  - **stdio** via `open-synth mcp --headless`. This runs the engine without a window, for batch or offline
    generation.
- **Tools** (all edits go through the command bus):

| Tool | Purpose |
|---|---|
| `describe_params(module?)` | Registry slice: ids, ranges, units, enum values, docs |
| `get_patch(modules?)` | Current values (non-default only by default) |
| `set_params([{path, value}])` | Batch set with validation errors returned per item |
| `add_modulation / remove_modulation` | Source, destination, amount, curve, aux |
| `set_curve(target, points)` | LFO shapes, env curves, remap/warp curves |
| `set_wavetable(osc, {file \| formula \| harmonics \| frames})` | Load or generate tables |
| `play(notes, duration)` | Audition through the speakers (HTTP mode) |
| `render(notes, seconds)` | Offline render, returns loudness/peak/centroid, a spectrogram PNG and a WAV path, so the agent can "hear" |
| `export_serum_preset(path, name, tags, author)` | Writes a `.SerumPreset` and reports any non-exportable parts |
| `import_serum_preset(path)` / `list_presets` | Start from an existing sound |
| `undo / redo / snapshot / diff` | Safe iteration |

- **Resources:** the param schema and the current patch as JSON. **Prompts:** sound-design recipes such as reese
  bass, supersaw and FM pluck.
- **Safety:** binds to localhost only with a per-session token. File tools are limited to user-chosen folders.

## 4b. Roadmap (revised: AI-assisted preset design)

**Milestone A (done):**
- `.SerumPreset` I/O, byte-exact on the factory library.
- Parameter schema and measured units for 455 knobs.
- Patch model with undo.
- MCP tools covering params, modulation, wavetables, samples, multisamples, LFO shapes, FX racks and export.
- Rendering through the installed Serum, and reference analysis/comparison.

The milestones below, in recommended order:

| Milestone | Scope | Why |
|---|---|---|
| **A2. Fast audition** ✅ | Keep one Serum instance loaded in a long-lived helper process and inject each patch into it (~0.2 s per render instead of ~15–25 s). Optional live playback to the speakers so the user hears every edit. | Iteration speed is the main bottleneck for both Claude and the user |
| **A3. Sound search** | Render every factory/user preset once (a few notes), store analysis features, and add `find_similar_presets(reference)` returning the closest presets as starting points | Starting from the nearest real preset beats building from Init |
| **A4. Auto-match** | `auto_match` tool: Claude picks which params to explore and their ranges; the helper runs a fast optimizer (coordinate / CMA-ES search over the compare score) and reports the best settings | Turns the 25-second manual loop into hundreds of fast evaluations |
| **A5. Richer analysis** | Brightness over time (filter-envelope detection), harmonic profile, noise vs. tone, LFO/wobble rate, multi-note and velocity response | Gives Claude better clues about *how* a sound was made |
| **A6. Packaging** | One-command install (build + Python env + register with Claude Code and the Claude desktop app), versioned releases, docs and example prompts | Usable without the repo knowledge |
| **A7. Companion UI (optional)** | Small local web page: current patch, A/B play against the reference, spectrogram comparison, history of attempts | Comfort for the human side of the loop |
| *Open items from A* | FX param units (needs GUI-bound measurement or audio-based calibration), XY path LFOs, envelope-mode loopback, mod curve editing | As needed |

## 5. Serum 2 export (requirement: exported presets must load in Serum 2)

**Container: solved.**
- `tools/serum2_preset.py` reads and writes `.SerumPreset` files.
- `metadata.hash` is the MD5 of the zstd frame.
- CBOR encoding: keys in insertion order; floats as float32 when lossless, else float64.
- Re-encoding is **byte-exact on 1,391 of 1,392** factory and user presets. The one failure has a corrupt zstd frame
  in the source file.

**Design consequences.** Export fidelity is decided by the engine, not the file writer.
1. **Mirror Serum's parameter model.** Our param registry uses Serum's module/param ids (`VoiceFilter0.kParamFreq`), its
   *plain* value units and ranges, and its enum names (`LadderAcid`, `kFM_OSC`, …). The exporter then just serializes
   the registry. No per-parameter translation table is needed for anything we implement 1:1.
2. **Matching module counts:** 3 main osc + noise + sub, 2 voice filters, 4 envs, 10 LFOs, 8 macros, 64 mod slots, and
   3 FX racks. Mod slots are stored as `source [type, index]` plus `destModuleTypeString/ID/ParamName`, so our mod
   matrix stores the same tuple.
3. **Features Serum lacks**, if we add any, are flagged "not exportable". The UI warns on export and the feature is
   dropped or approximated. Default policy: don't add such features until export parity is reached.
4. **Content:**
   - Wavetables are referenced by `relativePathToWT`. On export we either point at a path inside the user's Serum
     `Tables` folder (copying our `.wav` + `clm` chunk there), or embed the table if Serum allows it (TBD).
   - Samples, noise and IRs are embedded as `embedded_audio` bytes.
5. **Unchanged modules** are written as `"plainParams": "default"`, like Serum does. That requires knowing Serum's
   default for every param. Collect these from an Init preset plus the schema dump.

**Confirmed in Serum 2 (2026-10-09):** generated files load, edits are picked up, and a minimal preset with most
modules omitted also loads, so the exporter can write only non-default modules.

**Open question:**
- What are the value semantics of each param (normalized 0–1 vs plain units vs %)? Automate this: host Serum 2 with
  `pedalboard` (or a tiny JUCE host CLI), set each automatable parameter to known normalized values, read back the
  plugin state and diff the CBOR. This yields a per-param mapping table (normalized ↔ plain). Fallback: save presets
  by hand with known knob positions.

**Verification loop.** Golden test: for each feature, export a preset, load it in Serum 2, render both synths with the
same MIDI, and compare spectra. Rendering Serum needs a host: a small JUCE `AudioPluginHost`-based CLI, or `pedalboard`
(Python) to load the VST3 offline.

## 6. Testing strategy
- Unit tests for every DSP block: frequency response, aliasing via FFT, DC, denormals, NaN guards.
- **Byte-exact preset tests:** decode and re-encode every factory `.SerumPreset` (Rust port must match the Python
  reference).
- **A/B golden renders:** for each feature, export a patch, render it in our engine and in Serum 2 (hosted offline),
  and compare spectra and loudness with a tolerance. Regressions are caught by snapshotting our own renders.
- **MCP contract tests:** scripted tool-call sequences that produce known presets.
- Real-time safety: `assert_no_alloc` in debug builds; no locks on the audio thread.

## 7. Legal / content guardrails
- Write all DSP and UI from scratch. Do **not** copy Xfer's factory wavetables, samples, skins, presets or
  graphics into the repo, or ship them. Ship original content.
- The wavetable `clm` WAV convention is a de-facto community standard, so supporting it for user-owned files is fine.
- Present Serum preset import/export as interoperability for files users own.
- Don't use "Serum" in the product name or branding. Describing compatibility ("exports Serum 2 presets") is fine.

## 8. Immediate next steps (superseded by 4b; kept for history)
1. Scaffold the cargo workspace and port `serum2_preset.py` to `osyn-serum`, with the byte-exact test over the
   factory library.
2. Generate `osyn-params` and `docs/parity.md` from `docs/serum2-param-schema.txt` plus defaults from an Init preset.
3. Prototype automated parameter-value mapping by hosting Serum 2 offline.
4. `osyn-mcp` over stdio with `describe_params`, `get_patch`, `set_params` and `export_serum_preset`. This completes
   Milestone A.
