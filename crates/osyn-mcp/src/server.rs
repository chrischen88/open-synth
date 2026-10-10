//! Tool surface. Every edit goes through `osyn_patch::Patch::apply`, so it is atomic and
//! undoable. Values use Serum's own units and enum names; `describe_params` tells the agent
//! what those are.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use osyn_params::units::{ParamUnits, Plain, units};
use osyn_params::{MOD_SOURCES, mod_source_id, schema};
use osyn_patch::{Command, CurvePoint, Modulation, ParamPath, Patch, to_json, value_from_json};
use osyn_wt::Morph;
use rmcp::model::CallToolResult;

use crate::content;
use osyn_serum::{Map, Preset, Value};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value as Json, json};

const SERUM_USER_DIR: &str = "/Library/Audio/Presets/Xfer Records/Serum 2 Presets/Presets/User";
const EXPORT_SUBDIR: &str = "open-synth";

const INSTRUCTIONS: &str = "\
Design Serum 2 presets. The patch mirrors Serum 2's internal module tree; export writes a \
.SerumPreset that loads in Serum 2.

Addressing: params are `<module path>.<param>`, e.g. `Oscillator0.kParamVolume`, \
`Oscillator1/WTOsc1.kParamTablePos`, `VoiceFilter0.kParamType`, `Env1.kParamAttack`, \
`LFO0.kParamRate`, `Macro0.kParamValue`, `Global0.kParamMasterVolume`. \
Oscillator0..2 = OSC A..C, Oscillator3 = Noise, Oscillator4 = Sub; VoiceFilter0/1 = Filter 1/2; \
Env0..3 = ENV 1..4 (ENV 1 drives amp); LFO0..9; Macro0..7.

Workflow: call describe_modules, then describe_params for the modules you need (it shows ids, \
enum values, and value ranges observed in Serum's factory presets - use those as the unit guide). \
Use set_params / add_modulation / set_wavetable / set_lfo_shape, inspect with get_patch, \
audition with render (plays the patch through the user's Serum 2 and returns analysis plus a \
spectrogram), then export_preset.

Recreating a sound: analyze_audio the reference first (pitch, brightness, envelope, spectrogram), \
build a starting patch, then loop compare_to_reference -> adjust the biggest difference -> compare \
again. Fix pitch first, then tonal balance (oscillators/filter), then envelope, then width/FX. A param that is \
absent from get_patch is at Serum's default. Use undo if a change was wrong.";

#[derive(Clone)]
pub struct SynthServer {
    patch: Arc<Mutex<Patch>>,
    tool_router: ToolRouter<Self>,
}

impl Default for SynthServer {
    fn default() -> Self {
        Self::new()
    }
}

// --- tool parameter types -------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct ModuleArg {
    /// Module type, e.g. `Oscillator`, `WTOsc`, `VoiceFilter`, `Env`, `LFO`, `FXReverb`.
    pub module: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ParamSet {
    /// `<module path>.<param>`, e.g. `VoiceFilter0.kParamFreq`.
    pub path: String,
    /// One of: a value as Serum displays it (`"800 Hz"`, `"50 ms"`, `"-6 dB"`, `"75%"`,
    /// `"MG Low 12"`); an enum token (`"LadderAcid"`); `{"knob": 0..1}` (Serum's knob position);
    /// or a raw stored number.
    pub value: Json,
}

#[derive(Deserialize, JsonSchema)]
pub struct SetParamsArgs {
    pub params: Vec<ParamSet>,
    /// Allow params/values unknown to the schema (default false).
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct PathsArg {
    /// Param paths to reset to Serum's default.
    pub paths: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AddModArgs {
    /// Source name or id, e.g. `"LFO 1"`, `"Env 2"`, `"Macro 1"`, `"Velo"`. See list_mod_sources.
    pub source: Json,
    /// Destination param path, e.g. `Oscillator0/WTOsc0.kParamTablePos`.
    pub destination: String,
    /// Amount in percent, -100..100.
    pub amount: f64,
    /// Optional aux source (name or id) that scales the amount.
    #[serde(default)]
    pub aux_source: Option<Json>,
    /// Bipolar modulation (default false).
    #[serde(default)]
    pub bipolar: bool,
    /// Specific slot 0..63; default first free.
    #[serde(default)]
    pub slot: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AddFxArgs {
    /// 0 = Main, 1 = Bus 1, 2 = Bus 2.
    pub rack: usize,
    /// FX module type, e.g. `FXReverb`.
    pub fx_type: String,
    /// Insert position; default appends to the end of the chain.
    #[serde(default)]
    pub position: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RemoveFxArgs {
    pub rack: usize,
    pub index: usize,
}

#[derive(Deserialize, JsonSchema)]
pub struct ListTablesArgs {
    /// Case-insensitive filter on the path, e.g. "saw" or "Analog/".
    #[serde(default)]
    pub query: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SetWavetableArgs {
    /// Oscillator: "A", "B" or "C".
    pub osc: String,
    /// A table from Serum's Tables folder (path from list_wavetables), e.g. "Analog/MB Saw.wav".
    #[serde(default)]
    pub file: Option<String>,
    /// With `file`: embed the table in the preset instead of referencing it (portable; default false).
    #[serde(default)]
    pub embed: bool,
    /// Absolute path to a WAV to import (embedded). Serum-style tables carry their cycle length;
    /// otherwise pass `frame_size`.
    #[serde(default)]
    pub import: Option<String>,
    /// Cycle length in samples for `import` when the WAV has no clm chunk.
    #[serde(default)]
    pub frame_size: Option<usize>,
    /// Basic shapes to morph across the table, e.g. ["sine"] or ["sine","saw","square"].
    /// Names: sine, saw, square, triangle, pulse:<duty 0..1>.
    #[serde(default)]
    pub shapes: Option<Vec<String>>,
    /// Additive keyframes: each is harmonic amplitudes [h1, h2, h3, ...]; frames interpolate between them.
    #[serde(default)]
    pub harmonics: Option<Vec<Vec<f64>>>,
    /// Formula per sample: x = phase -1..1, w = phase 0..1, y = table position 0..1, n = frame index.
    /// Functions: sin cos tan tanh abs sqrt exp log floor ceil round sign frac min max pow clamp
    /// saw square tri (period 1). Example: "sin(x*pi) * (1-y) + saw(w*(1+3*y)) * y".
    #[serde(default)]
    pub formula: Option<String>,
    /// Number of frames to generate (1..256). Default: 1 for a single shape, else 64.
    #[serde(default)]
    pub frames: Option<usize>,
    /// How Serum morphs between stored frames: none, crossfade, spectral (embedded tables only).
    #[serde(default)]
    pub morph: Option<String>,
    /// Table name shown in Serum.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ListSamplesArgs {
    /// "samples" (for sample/granular/spectral oscillators) or "multisamples" (SFZ instruments).
    pub kind: String,
    /// Case-insensitive filter on the path, e.g. "piano" or "Factory/Bass".
    #[serde(default)]
    pub query: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SetSampleArgs {
    /// Oscillator: "A", "B" or "C".
    pub osc: String,
    /// Engine: "sample", "granular" or "spectral". The oscillator is switched to it.
    pub engine: String,
    /// A file from Serum's Samples folder (path from list_samples), referenced by path.
    #[serde(default)]
    pub file: Option<String>,
    /// Absolute path to a WAV to embed in the preset (portable, up to 120 s).
    #[serde(default)]
    pub import: Option<String>,
    /// MIDI note of the sample's pitch (root key), e.g. 60 for C4.
    #[serde(default)]
    pub base_note: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SetMultisampleArgs {
    /// Oscillator: "A", "B" or "C".
    pub osc: String,
    /// An .sfz from Serum's Multisamples folder (path from list_samples kind=multisamples).
    pub file: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct LfoPointArg {
    /// 0..1 across the LFO cycle; first point 0, last point 1, never decreasing.
    pub x: f64,
    /// 0 = bottom, 1 = top.
    pub value: f64,
    /// Segment bend from this point to the next: 0.5 = straight, toward 0 or 1 = curved.
    #[serde(default = "half")]
    pub curve: f64,
}

fn half() -> f64 {
    0.5
}

#[derive(Deserialize, JsonSchema)]
pub struct SetLfoShapeArgs {
    /// LFO number 1..10.
    pub lfo: usize,
    /// Either explicit points...
    #[serde(default)]
    pub points: Option<Vec<LfoPointArg>>,
    /// ...or a preset shape: sine, triangle, saw_up, saw_down, square, ramp_up (one-shot rise), pulse.
    #[serde(default)]
    pub shape: Option<String>,
    /// Shape name shown in Serum.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RenderArgs {
    /// Notes as "midi:start_s:length_s" comma list. Default "48:0:1.5" (one C3 note).
    /// Chords: "48:0:1.5,52:0:1.5,55:0:1.5".
    #[serde(default)]
    pub notes: Option<String>,
    /// Velocity 1..127 (default 100).
    #[serde(default)]
    pub velocity: Option<u8>,
    /// Render length in seconds (default: last note end + 0.5, at least 2.5).
    #[serde(default)]
    pub duration: Option<f64>,
    /// Macro positions as "index:percent", index 0..7 = Macro 1..8, e.g. "0:100,1:25".
    #[serde(default)]
    pub macros: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AnalyzeArgs {
    /// Absolute path to an audio file (WAV, FLAC, AIFF, MP3).
    pub path: String,
    /// Only analyse from this time (s).
    #[serde(default)]
    pub start: Option<f64>,
    /// Only analyse up to this time (s).
    #[serde(default)]
    pub end: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CompareArgs {
    /// Absolute path to the reference sound to match (WAV, FLAC, AIFF, MP3). Ideally one note,
    /// dry, without other instruments.
    pub reference: String,
    /// Trim the reference: start time (s).
    #[serde(default)]
    pub start: Option<f64>,
    /// Trim the reference: end time (s).
    #[serde(default)]
    pub end: Option<f64>,
    /// Notes to render the patch with ("midi:start_s:length_s,..."). Default: one note at the
    /// reference's detected pitch, lasting 75% of its length.
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub velocity: Option<u8>,
    /// Macro positions "index:percent".
    #[serde(default)]
    pub macros: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct PlayArgs {
    /// Notes "midi:start_s:length_s,..." (default one C3 note). Chords and phrases work too.
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub velocity: Option<u8>,
    #[serde(default)]
    pub duration: Option<f64>,
    /// Macro positions "index:percent".
    #[serde(default)]
    pub macros: Option<String>,
    /// Output device name (default: system output). See the error message for available names.
    #[serde(default)]
    pub device: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct PlayAudioArgs {
    /// Absolute path to an audio file (e.g. the reference, or a candidateWav from compare).
    pub path: String,
    #[serde(default)]
    pub device: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SlotArg {
    pub slot: usize,
}

#[derive(Deserialize, JsonSchema)]
pub struct InfoArgs {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Browser tags, e.g. ["Bass", "Wavetable"].
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Deserialize, JsonSchema)]
pub struct PathArg {
    /// Absolute path to a .SerumPreset file.
    pub path: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ExportArgs {
    /// File name or absolute path ending in .SerumPreset. A bare name is saved to Serum's
    /// `User/open-synth` folder so it shows up in Serum's browser.
    pub path: String,
    /// Replace an existing file (default false).
    #[serde(default)]
    pub overwrite: bool,
}

// --- tools ---------------------------------------------------------------------------------

type ToolResult = Result<String, String>;

fn ok(v: Json) -> ToolResult {
    Ok(serde_json::to_string(&v).unwrap_or_default())
}

#[tool_router]
impl SynthServer {
    pub fn new() -> Self {
        SynthServer { patch: Arc::new(Mutex::new(Patch::default())), tool_router: Self::tool_router() }
    }

    fn with_patch<T>(&self, f: impl FnOnce(&mut Patch) -> T) -> T {
        let mut guard = self.patch.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    #[tool(description = "List Serum 2 module types with where they live in the patch and how many params each has.")]
    async fn describe_modules(&self) -> ToolResult {
        let modules: Vec<Json> = schema()
            .modules
            .iter()
            .map(|(name, m)| json!({"module": name, "paths": m.paths, "params": m.params.len()}))
            .collect();
        ok(json!(modules))
    }

    #[tool(description = "Describe a module's params: id, whether automatable, enum values, and the value range observed across Serum's factory presets (a guide to units).")]
    async fn describe_params(&self, Parameters(arg): Parameters<ModuleArg>) -> ToolResult {
        let s = schema();
        let m = s.module(&arg.module).ok_or_else(|| format!("unknown module `{}`; see describe_modules", arg.module))?;
        let params: Vec<Json> = m
            .params
            .iter()
            .map(|p| {
                let mut o = json!({"name": p.name, "id": p.id, "automatable": p.automatable});
                if let Some(u) = units().for_type(&arg.module, &p.name) {
                    o["default"] = json!(u.display(u.default_knob));
                    o["range"] = json!([u.display(0.0), u.display(1.0)]);
                    let accepts: Vec<String> = u.units().iter().map(|x| format!("{x:?}")).collect();
                    if !accepts.is_empty() {
                        o["accepts"] = json!(accepts);
                    }
                } else if let Some(d) = units().fx_default(&arg.module, &p.name) {
                    // FX defaults are plain values; enum defaults are ordinals into the value list.
                    let named = s.enum_values(p).zip(d.as_f64()).and_then(|(vals, i)| vals.get(i as usize).cloned());
                    o["default"] = named.map_or_else(|| d.clone(), Json::String);
                }
                if let Some(values) = s.enum_values(p) {
                    o["values"] = json!(values);
                }
                if let Some(obs) = &p.observed {
                    if let (Some(min), Some(max)) = (obs.min, obs.max)
                        && o.get("range").is_none()
                    {
                        o["observedRange"] = json!([min, max]);
                    }
                    if !obs.strings.is_empty() && p.enum_name.is_none() {
                        o["observedStrings"] = json!(obs.strings);
                    }
                }
                o
            })
            .collect();
        ok(json!({"module": arg.module, "paths": m.paths, "params": params}))
    }

    #[tool(description = "List modulation sources with their ids.")]
    async fn list_mod_sources(&self) -> ToolResult {
        ok(json!(MOD_SOURCES.iter().enumerate().skip(1).map(|(i, n)| json!({"id": i, "name": n})).collect::<Vec<_>>()))
    }

    #[tool(description = "Start a new, empty (Serum init) patch. Undoable.")]
    async fn new_patch(&self) -> ToolResult {
        self.with_patch(|p| p.apply(Command::Replace(Box::new(osyn_patch::init_preset()))))
            .map_err(|e| e.to_string())?;
        ok(json!({"status": "new patch"}))
    }

    #[tool(description = "Show the current patch: info, every non-default param, and modulation routings.")]
    async fn get_patch(&self) -> ToolResult {
        self.with_patch(|p| {
            let preset = p.preset();
            let params: serde_json::Map<String, Json> =
                p.non_default()
                    .into_iter()
                    .filter(|(path, _)| path.module.module_type() != "ModSlot") // shown under modulations
                    .map(|(path, v)| {
                        let key = path.to_string();
                        let shown = unit_for(&path).and_then(|u| shows(u, &v));
                        let j = match shown {
                            Some(s) => json!(format!("{s}  [{}]", to_json(&v))),
                            None => to_json(&v),
                        };
                        (key, j)
                    })
                    .collect();
            let mods: Vec<Json> = p.modulations().into_iter().map(|(slot, m)| modulation_json(slot, &m)).collect();
            ok(json!({
                "name": preset.name(),
                "tags": preset.tags(),
                "params": params,
                "modulations": mods,
            }))
        })
    }

    #[tool(description = "Set one or more params atomically (all or nothing). Values are validated against the schema: param names must exist and enum params must use a listed value.")]
    async fn set_params(&self, Parameters(args): Parameters<SetParamsArgs>) -> ToolResult {
        let mut sets = Vec::with_capacity(args.params.len());
        let mut resets = Vec::new();
        let mut results = Vec::new();
        let mut warnings = Vec::new();
        for ps in &args.params {
            let path: ParamPath = ps.path.parse().map_err(|e: osyn_patch::Error| e.to_string())?;
            let unit = unit_for(&path);
            let resolved = resolve_value(&path, unit, &ps.value)?;
            let Some(value) = resolved else {
                // Enum "Off" entries are stored by omitting the key.
                results.push(json!({"path": ps.path, "value": null, "shows": "Off (default)"}));
                resets.push(path);
                continue;
            };
            match validate(&path, &value) {
                Ok(Some(w)) => warnings.push(w),
                Ok(None) => {}
                Err(e) if args.force => warnings.push(format!("forced: {e}")),
                Err(e) => return Err(e),
            }
            results.push(json!({"path": ps.path, "value": to_json(&value), "shows": unit.and_then(|u| shows(u, &value))}));
            sets.push((path, value));
        }
        self.with_patch(|p| {
            p.apply(Command::SetParams(sets))?;
            if !resets.is_empty() {
                p.apply(Command::ResetParams(resets))?;
            }
            Ok::<_, osyn_patch::Error>(())
        })
        .map_err(|e| e.to_string())?;
        ok(json!({"set": results, "warnings": warnings}))
    }

    #[tool(description = "Reset params to Serum's default.")]
    async fn reset_params(&self, Parameters(arg): Parameters<PathsArg>) -> ToolResult {
        let paths = arg
            .paths
            .iter()
            .map(|s| s.parse::<ParamPath>().map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let n = paths.len();
        self.with_patch(|p| p.apply(Command::ResetParams(paths))).map_err(|e| e.to_string())?;
        ok(json!({"reset": n}))
    }

    #[tool(description = "Route a modulation source to a param (uses the first free of 64 mod slots unless `slot` is given).")]
    async fn add_modulation(&self, Parameters(args): Parameters<AddModArgs>) -> ToolResult {
        let source = resolve_source(&args.source)?;
        let aux = args.aux_source.as_ref().map(resolve_source).transpose()?.unwrap_or(0);
        let dest: ParamPath = args.destination.parse().map_err(|e: osyn_patch::Error| e.to_string())?;
        let module_type = dest.module.module_type().to_owned();
        let dest_module_id = if module_type.starts_with("FX") {
            fx_location(&dest).map(|(rack, idx)| osyn_patch::fx_dest_id(rack, idx))?
        } else {
            i64::from(dest.module.instance())
        };
        let param = schema()
            .param(&module_type, &dest.param)
            .ok_or_else(|| format!("unknown param `{}` on module `{module_type}`", dest.param))?;
        let param_id = param.id.ok_or_else(|| format!("`{}` has no known param id", dest.param))?;
        if !(-100.0..=100.0).contains(&args.amount) {
            return Err("amount must be within -100..100".into());
        }
        let mut params = Map::new();
        params.insert("kParamAmount".into(), Value::Float(args.amount));
        if args.bipolar {
            params.insert("kParamBipolar".into(), Value::Float(1.0));
        }
        let modulation = Modulation {
            source,
            aux_source: aux,
            dest_module_type: module_type,
            dest_module_id,
            dest_param_name: dest.param.clone(),
            dest_param_id: param_id,
            params,
        };
        let slot = self
            .with_patch(|p| p.apply(Command::SetModulation { slot: args.slot, modulation: modulation.clone() }))
            .map_err(|e| e.to_string())?
            .expect("SetModulation returns its slot");
        ok(modulation_json(slot, &modulation))
    }

    #[tool(description = "Clear a mod slot.")]
    async fn remove_modulation(&self, Parameters(arg): Parameters<SlotArg>) -> ToolResult {
        self.with_patch(|p| p.apply(Command::ClearModulation(arg.slot))).map_err(|e| e.to_string())?;
        ok(json!({"cleared": arg.slot}))
    }

    #[tool(description = "Set preset name, author, description and/or browser tags.")]
    async fn set_info(&self, Parameters(args): Parameters<InfoArgs>) -> ToolResult {
        let mut cmds = Vec::new();
        for (key, v) in [("presetName", args.name), ("presetAuthor", args.author), ("presetDescription", args.description)] {
            if let Some(value) = v {
                cmds.push(Command::SetInfo { key: key.into(), value });
            }
        }
        if let Some(tags) = args.tags {
            cmds.push(Command::SetTags(tags));
        }
        self.with_patch(|p| cmds.into_iter().try_for_each(|c| p.apply(c).map(drop))).map_err(|e| e.to_string())?;
        ok(json!({"status": "updated"}))
    }

    #[tool(description = "Load an existing .SerumPreset as the current patch (undoable).")]
    async fn load_preset(&self, Parameters(arg): Parameters<PathArg>) -> ToolResult {
        let preset = Preset::read(&arg.path).map_err(|e| e.to_string())?;
        let name = preset.name().map(str::to_owned);
        self.with_patch(|p| p.apply(Command::Replace(Box::new(preset)))).map_err(|e| e.to_string())?;
        ok(json!({"loaded": name}))
    }

    #[tool(description = "Export the patch as a .SerumPreset that Serum 2 can load. Bare file names go to Serum's User/open-synth folder.")]
    async fn export_preset(&self, Parameters(args): Parameters<ExportArgs>) -> ToolResult {
        let path = export_path(&args.path)?;
        if path.exists() && !args.overwrite {
            return Err(format!("{} exists; pass overwrite=true to replace it", path.display()));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let bytes = self.with_patch(|p| p.preset().to_bytes()).map_err(|e| e.to_string())?;
        std::fs::write(&path, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        ok(json!({"exported": path, "bytes": bytes.len()}))
    }

    #[tool(description = "Insert an FX module (e.g. FXReverb, FXDistortion, FXDelay) into a rack: 0 = Main, 1 = Bus 1, 2 = Bus 2. Returns its path; set its params with set_params.")]
    async fn add_fx(&self, Parameters(args): Parameters<AddFxArgs>) -> ToolResult {
        let module = schema().module(&args.fx_type).filter(|_| args.fx_type.starts_with("FX"));
        let type_id = module
            .and_then(|m| m.fx_type_id)
            .ok_or_else(|| format!("`{}` is not a known FX module type; see describe_modules", args.fx_type))?;
        let index = self
            .with_patch(|p| {
                p.apply(Command::InsertFx {
                    rack: args.rack,
                    index: args.position.unwrap_or(usize::MAX),
                    fx_type: args.fx_type.clone(),
                    type_id,
                })
            })
            .map_err(|e| e.to_string())?
            .expect("InsertFx returns its index");
        ok(json!({"path": format!("FXRack{}/FX[{index}]/{}", args.rack, args.fx_type)}))
    }

    #[tool(description = "Remove the FX module at `index` in a rack. Later modules shift down; their modulations follow.")]
    async fn remove_fx(&self, Parameters(args): Parameters<RemoveFxArgs>) -> ToolResult {
        self.with_patch(|p| p.apply(Command::RemoveFx { rack: args.rack, index: args.index })).map_err(|e| e.to_string())?;
        ok(json!({"removed": format!("FXRack{}/FX[{}]", args.rack, args.index)}))
    }

    #[tool(description = "List wavetables in Serum's Tables folder (paths for set_wavetable `file`).")]
    async fn list_wavetables(&self, Parameters(args): Parameters<ListTablesArgs>) -> ToolResult {
        let tables = content::list_tables(args.query.as_deref())?;
        let total = tables.len();
        ok(json!({"total": total, "tables": tables.into_iter().take(200).collect::<Vec<_>>()}))
    }

    #[tool(description = "Load a wavetable into OSC A, B or C. Give exactly one source: `file` (Serum library table), `import` (any WAV), `shapes`, `harmonics` or `formula`. Generated and imported tables are embedded in the preset.")]
    async fn set_wavetable(&self, Parameters(a): Parameters<SetWavetableArgs>) -> ToolResult {
        let osc = parse_osc(&a.osc)?;
        let sources = [a.file.is_some(), a.import.is_some(), a.shapes.is_some(), a.harmonics.is_some(), a.formula.is_some()];
        if sources.iter().filter(|s| **s).count() != 1 {
            return Err("give exactly one of file, import, shapes, harmonics, formula".into());
        }
        let morph = a
            .morph
            .as_deref()
            .map(|m| Morph::from_name(m).ok_or_else(|| format!("unknown morph `{m}` (none, crossfade, spectral)")))
            .transpose()?;
        let (table, frames) = if let Some(file) = &a.file {
            content::table_from_library(file, a.embed, morph)?
        } else {
            let generated = if let Some(path) = &a.import {
                content::import_wav(path, a.frame_size)?
            } else if let Some(shapes) = &a.shapes {
                content::from_shapes(shapes, a.frames.unwrap_or(if shapes.len() == 1 { 1 } else { 64 }))?
            } else if let Some(keys) = &a.harmonics {
                osyn_wt::generate::harmonics(keys, a.frames.unwrap_or(if keys.len() == 1 { 1 } else { 64 })).map_err(|e| e.to_string())?
            } else {
                let f = a.formula.as_deref().unwrap_or_default();
                osyn_wt::generate::formula(f, a.frames.unwrap_or(64)).map_err(|e| e.to_string())?
            };
            let name = a.name.clone().unwrap_or_else(|| "open-synth".into());
            content::embedded(generated, name, morph.unwrap_or_default())
        };
        let embedded = matches!(table, osyn_patch::WavetableSource::Embedded { .. });
        self.with_patch(|p| p.apply(Command::SetWavetable { osc, table })).map_err(|e| e.to_string())?;
        let osc_name = ["A", "B", "C"][osc];
        ok(json!({"osc": osc_name, "frames": frames, "embedded": embedded,
                  "hint": "kParamTablePos scans 1..256 across the table's frames"}))
    }

    #[tool(description = "List audio in Serum's Samples folder (kind=samples) or SFZ instruments in its Multisamples folder (kind=multisamples).")]
    async fn list_samples(&self, Parameters(a): Parameters<ListSamplesArgs>) -> ToolResult {
        let (kind, exts): (&str, &[&str]) = match a.kind.as_str() {
            "samples" => ("samples", &["wav", "flac", "aif", "aiff"]),
            "multisamples" => ("multisamples", &["sfz"]),
            other => return Err(format!("kind must be samples or multisamples, got `{other}`")),
        };
        let files = content::list_files(&content::content_dir(kind), exts, a.query.as_deref())?;
        let total = files.len();
        ok(json!({"total": total, "files": files.into_iter().take(200).collect::<Vec<_>>()}))
    }

    #[tool(description = "Load audio into OSC A, B or C as a sample, granular or spectral oscillator (switches the oscillator type). Use `file` for Serum's Samples library or `import` for any WAV (embedded).")]
    async fn set_sample(&self, Parameters(a): Parameters<SetSampleArgs>) -> ToolResult {
        let osc = parse_osc(&a.osc)?;
        let engine = match a.engine.to_ascii_lowercase().as_str() {
            "sample" => osyn_patch::SampleEngine::Sample,
            "granular" => osyn_patch::SampleEngine::Granular,
            "spectral" => osyn_patch::SampleEngine::Spectral,
            other => return Err(format!("engine must be sample, granular or spectral, got `{other}`")),
        };
        if a.base_note.is_some_and(|n| !(0..=127).contains(&n)) {
            return Err("base_note must be a MIDI note 0..127".into());
        }
        let source = content::sample_source(a.file.as_deref(), a.import.as_deref())?;
        let (embedded, frames, channels) = match &source {
            osyn_patch::SampleSource::File { info, .. } => (false, info.length, info.channels),
            osyn_patch::SampleSource::Embedded { channels, .. } => (true, channels[0].len() as i64, channels.len() as i64),
        };
        self.with_patch(|p| p.apply(Command::SetSample { osc, engine, source, base_note: a.base_note })).map_err(|e| e.to_string())?;
        ok(json!({"osc": OSC_NAMES[osc], "engine": a.engine, "embedded": embedded, "frames": frames, "channels": channels}))
    }

    #[tool(description = "Load an SFZ multisample instrument from Serum's Multisamples folder into OSC A, B or C (switches the oscillator to Multisample).")]
    async fn set_multisample(&self, Parameters(a): Parameters<SetMultisampleArgs>) -> ToolResult {
        let osc = parse_osc(&a.osc)?;
        let (text, files) = content::multisample(&a.file)?;
        let n = files.len();
        self.with_patch(|p| p.apply(Command::SetMultisample { osc, sfz_relative: a.file.clone(), sfz_text: text, files }))
            .map_err(|e| e.to_string())?;
        ok(json!({"osc": OSC_NAMES[osc], "sfz": a.file, "samples": n}))
    }

    #[tool(description = "Draw an LFO's shape from points or a named preset shape.")]
    async fn set_lfo_shape(&self, Parameters(a): Parameters<SetLfoShapeArgs>) -> ToolResult {
        if !(1..=10).contains(&a.lfo) {
            return Err("lfo must be 1..10".into());
        }
        let points: Vec<CurvePoint> = match (&a.points, &a.shape) {
            (Some(pts), None) => pts.iter().map(|p| CurvePoint { x: p.x, value: p.value, curve: p.curve }).collect(),
            (None, Some(name)) => preset_lfo_shape(name)?,
            _ => return Err("give either points or shape".into()),
        };
        let n = points.len();
        let name = a.name.clone().or_else(|| a.shape.clone());
        self.with_patch(|p| p.apply(Command::SetLfoShape { lfo: a.lfo - 1, points, name })).map_err(|e| e.to_string())?;
        ok(json!({"lfo": a.lfo, "points": n}))
    }

    #[tool(description = "Audition the current patch through the installed Serum 2 (offline; ~10 s for the first call while Serum loads, then ~0.5 s). Returns loudness (LUFS, peak, RMS), spectral centroid, band levels, stereo width, an RMS envelope, Serum's own readouts of key knobs, and a spectrogram image.")]
    async fn render(&self, Parameters(a): Parameters<RenderArgs>) -> Result<CallToolResult, String> {
        let bytes = self.with_patch(|p| p.preset().to_bytes()).map_err(|e| e.to_string())?;
        let opts = content::RenderOpts { notes: a.notes, velocity: a.velocity, duration: a.duration, macros: a.macros };
        content::render(bytes, &opts).await
    }

    #[tool(description = "Analyse an audio file (e.g. a reference sound to recreate): loudness, pitch, brightness, band balance, stereo width, envelope, and a spectrogram image.")]
    async fn analyze_audio(&self, Parameters(a): Parameters<AnalyzeArgs>) -> Result<CallToolResult, String> {
        content::analyze(&a.path, a.start, a.end).await
    }

    #[tool(description = "Render the current patch through Serum 2 and compare it with a reference sound. Returns per-aspect differences (pitch in semitones, brightness in octaves, band balance dB, loudness, stereo width, attack/decay, envelope correlation), the largest 1/3-octave spectral differences, a 0..100 similarity score, hints, and an image with both spectrograms and overlaid spectra. Iterate: adjust the patch, compare again.")]
    async fn compare_to_reference(&self, Parameters(a): Parameters<CompareArgs>) -> Result<CallToolResult, String> {
        let bytes = self.with_patch(|p| p.preset().to_bytes()).map_err(|e| e.to_string())?;
        let opts = content::RenderOpts { notes: a.notes, velocity: a.velocity, duration: None, macros: a.macros };
        content::compare(bytes, &a.reference, a.start, a.end, opts).await
    }

    #[tool(description = "Play the current patch on the user's speakers through Serum 2 (renders in ~0.5 s once Serum is warm, then plays). Use it so the user can hear changes.")]
    async fn play(&self, Parameters(a): Parameters<PlayArgs>) -> ToolResult {
        let bytes = self.with_patch(|p| p.preset().to_bytes()).map_err(|e| e.to_string())?;
        let opts = content::RenderOpts { notes: a.notes, velocity: a.velocity, duration: a.duration, macros: a.macros };
        let report = content::render_report(bytes, &opts).await?;
        let wav = report.get("wav").and_then(Json::as_str).ok_or("render returned no wav")?;
        let played = content::play_file(wav, a.device.as_deref()).await?;
        ok(json!({"playing": played, "lufsIntegrated": report.get("lufsIntegrated"), "peakDbfs": report.get("peakDbfs")}))
    }

    #[tool(description = "Play an audio file on the user's speakers, e.g. the reference sound for an A/B with play.")]
    async fn play_audio(&self, Parameters(a): Parameters<PlayAudioArgs>) -> ToolResult {
        ok(content::play_file(&a.path, a.device.as_deref()).await?)
    }

    #[tool(description = "Undo the last change.")]
    async fn undo(&self) -> ToolResult {
        ok(json!({"undone": self.with_patch(Patch::undo)}))
    }

    #[tool(description = "Redo the last undone change.")]
    async fn redo(&self) -> ToolResult {
        ok(json!({"redone": self.with_patch(Patch::redo)}))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SynthServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("open-synth", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

// --- helpers -------------------------------------------------------------------------------

/// Ok(None) = valid, Ok(Some(w)) = valid with warning, Err = invalid.
fn validate(path: &ParamPath, value: &Value) -> Result<Option<String>, String> {
    let s = schema();
    let module_type = path.module.module_type();
    let module = s.module(module_type).ok_or_else(|| format!("{path}: unknown module type `{module_type}`"))?;
    let param = module
        .params
        .iter()
        .find(|p| p.name == path.param)
        .ok_or_else(|| format!("{path}: `{}` is not a param of {module_type}", path.param))?;
    if let Some(allowed) = s.enum_values(param) {
        return match value {
            Value::Text(t) if allowed.contains(t) => Ok(None),
            _ => {
                let near: Vec<&String> = match value {
                    Value::Text(t) => {
                        let t = t.to_ascii_lowercase();
                        allowed.iter().filter(|a| a.to_ascii_lowercase().contains(&t) || t.contains(&a.to_ascii_lowercase())).collect()
                    }
                    _ => Vec::new(),
                };
                let hint = if near.is_empty() { allowed.iter().take(12).collect() } else { near };
                Err(format!(
                    "{path}: expected one of {} enum strings (e.g. {hint:?}); call describe_params({module_type}) for all",
                    allowed.len()
                ))
            }
        };
    }
    if let (Some(v), Some(obs)) = (value.as_f64(), &param.observed)
        && let (Some(min), Some(max)) = (obs.min, obs.max)
        && (v < min || v > max)
    {
        return Ok(Some(format!("{path}: {v} is outside the range seen in factory presets [{min}, {max}]")));
    }
    Ok(None)
}

/// Turn a tool-supplied value into the stored plain value. `Ok(None)` = omit the key.
fn resolve_value(path: &ParamPath, unit: Option<&ParamUnits>, v: &Json) -> Result<Option<Value>, String> {
    let from_plain = |p: Plain| match p {
        Plain::Number(n) => Some(Value::Float(n)),
        Plain::Token(t) => t.map(Value::Text),
    };
    match (v, unit) {
        (Json::Object(o), _) if o.contains_key("knob") => {
            let u = unit.ok_or_else(|| format!("{path}: knob positions are not mapped for this param; pass a stored value"))?;
            let r = o["knob"].as_f64().filter(|r| (0.0..=1.0).contains(r)).ok_or_else(|| format!("{path}: knob must be 0..1"))?;
            Ok(from_plain(u.knob_to_plain(r)))
        }
        (Json::Bool(b), _) => Ok(Some(Value::Float(if *b { 1.0 } else { 0.0 }))),
        (Json::String(s), Some(u)) => {
            if let Some(p) = u.enum_token(s) {
                return Ok(from_plain(p));
            }
            if matches!(u.transform, osyn_params::units::Transform::Enum { .. }) {
                return Ok(Some(Value::Text(s.clone()))); // let validate() report valid names
            }
            match osyn_params::units::parse_display(s) {
                Some((n, osyn_params::units::Unit::Number)) if u.units().is_empty() => Ok(Some(Value::Float(n))),
                Some(_) => Ok(from_plain(u.knob_to_plain(u.display_to_knob(s).map_err(|e| format!("{path}: {e}"))?))),
                None => Err(format!("{path}: cannot interpret `{s}`")),
            }
        }
        _ => value_from_json(v).map(Some).ok_or_else(|| format!("{path}: value must not be null")),
    }
}

/// Units for a param: exact address first, else any instance of the same module type
/// (FX modules move around racks; instances of a module type share their laws).
fn unit_for(path: &ParamPath) -> Option<&'static ParamUnits> {
    units().get(&path.to_string()).or_else(|| units().for_type(path.module.module_type(), &path.param))
}

/// Display string for a stored value, when the param's units are known.
fn shows(u: &ParamUnits, v: &Value) -> Option<String> {
    let plain = match v {
        Value::Float(f) => Plain::Number(*f),
        Value::Int(i) => Plain::Number(*i as f64),
        Value::Text(t) => Plain::Token(Some(t.clone())),
        _ => return None,
    };
    u.plain_to_knob(&plain).map(|r| u.display(r))
}

const OSC_NAMES: [&str; 3] = ["A", "B", "C"];

fn parse_osc(s: &str) -> Result<usize, String> {
    match s.trim().to_ascii_uppercase().as_str() {
        "A" | "0" => Ok(0),
        "B" | "1" => Ok(1),
        "C" | "2" => Ok(2),
        other => Err(format!("osc must be A, B or C, got `{other}`")),
    }
}

fn preset_lfo_shape(name: &str) -> Result<Vec<CurvePoint>, String> {
    let pts: &[(f64, f64, f64)] = match name.to_ascii_lowercase().replace([' ', '-'], "_").as_str() {
        "triangle" | "tri" => &[(0.0, 0.0, 0.5), (0.5, 1.0, 0.5), (1.0, 0.0, 0.5)],
        // Two curved halves approximate a sine (curve bends each segment like Serum's sine preset).
        "sine" | "sin" => &[(0.0, 0.5, 0.27), (0.25, 1.0, 0.73), (0.5, 0.5, 0.27), (0.75, 0.0, 0.73), (1.0, 0.5, 0.5)],
        "saw_up" | "ramp" => &[(0.0, 0.0, 0.5), (1.0, 1.0, 0.5)],
        "saw_down" => &[(0.0, 1.0, 0.5), (1.0, 0.0, 0.5)],
        "square" => &[(0.0, 1.0, 0.5), (0.5, 1.0, 0.5), (0.5, 0.0, 0.5), (1.0, 0.0, 0.5)],
        "pulse" => &[(0.0, 1.0, 0.5), (0.125, 1.0, 0.5), (0.125, 0.0, 0.5), (1.0, 0.0, 0.5)],
        "ramp_up" => &[(0.0, 0.0, 0.5), (1.0, 1.0, 0.5)],
        other => return Err(format!("unknown LFO shape `{other}` (sine, triangle, saw_up, saw_down, square, pulse)")),
    };
    Ok(pts.iter().map(|&(x, value, curve)| CurvePoint { x, value, curve }).collect())
}

fn resolve_source(v: &Json) -> Result<i64, String> {
    let id = match v {
        Json::Number(n) => n.as_i64(),
        Json::String(s) => mod_source_id(s),
        _ => None,
    };
    id.filter(|&i| i > 0 && (i as usize) < MOD_SOURCES.len())
        .ok_or_else(|| format!("unknown mod source {v}; see list_mod_sources"))
}

fn modulation_json(slot: usize, m: &Modulation) -> Json {
    let name = |id: i64| MOD_SOURCES.get(id as usize).copied().unwrap_or("?");
    let dest_module = match m.dest_module_type.as_str() {
        // Nested osc engines live under their oscillator.
        t @ ("WTOsc" | "SampleOsc" | "MultiSampleOsc" | "GranularOsc" | "SpectralOsc" | "NoiseOsc" | "SubOsc") => {
            format!("Oscillator{0}/{t}{0}", m.dest_module_id)
        }
        t if t.starts_with("FX") => format!("FXRack{}/FX[{}]/{t}", m.dest_module_id / 100, m.dest_module_id % 100),
        t => format!("{t}{}", m.dest_module_id),
    };
    json!({
        "slot": slot,
        "source": name(m.source),
        "aux": (m.aux_source != 0).then(|| name(m.aux_source)),
        "destination": format!("{dest_module}.{}", m.dest_param_name),
        "params": to_json(&Value::Map(m.params.clone())),
    })
}

/// `FXRack{r}/FX[i]/FXType` -> (r, i).
fn fx_location(path: &ParamPath) -> Result<(usize, usize), String> {
    let bad = || format!("FX params are addressed as FXRack<r>/FX[<i>]/<FXType>.<param>, got `{path}`");
    match path.module.0.as_slice() {
        [rack, fx, _module] if rack.key.starts_with("FXRack") && fx.key == "FX" => {
            let r = rack.key["FXRack".len()..].parse().map_err(|_| bad())?;
            Ok((r, fx.index.ok_or_else(bad)?))
        }
        _ => Err(bad()),
    }
}

fn export_path(arg: &str) -> Result<PathBuf, String> {
    let p = Path::new(arg);
    let path = if p.is_absolute() { p.to_path_buf() } else { Path::new(SERUM_USER_DIR).join(EXPORT_SUBDIR).join(p) };
    if path.extension().is_none_or(|e| e != "SerumPreset") {
        return Err("export path must end with .SerumPreset".into());
    }
    if !p.is_absolute() && p.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
        return Err("relative export names may not contain `..` or roots".into());
    }
    Ok(path)
}
