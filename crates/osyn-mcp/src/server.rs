//! Tool surface. Every edit goes through `osyn_patch::Patch::apply`, so it is atomic and
//! undoable. Values use Serum's own units and enum names; `describe_params` tells the agent
//! what those are.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use osyn_params::{MOD_SOURCES, mod_source_id, schema};
use osyn_patch::{Command, Modulation, ParamPath, Patch, to_json, value_from_json};
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
Use set_params / add_modulation, inspect with get_patch, then export_preset. A param that is \
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
    /// Number, boolean, or enum string (e.g. `"LadderAcid"`), in Serum's units.
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
                if let Some(values) = s.enum_values(p) {
                    o["values"] = json!(values);
                }
                if let Some(obs) = &p.observed {
                    if let (Some(min), Some(max)) = (obs.min, obs.max) {
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
                    .map(|(path, v)| (path.to_string(), to_json(&v)))
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
        let mut items = Vec::with_capacity(args.params.len());
        let mut warnings = Vec::new();
        for ps in &args.params {
            let path: ParamPath = ps.path.parse().map_err(|e: osyn_patch::Error| e.to_string())?;
            let value = value_from_json(&ps.value).ok_or_else(|| format!("{}: value must not be null", ps.path))?;
            match validate(&path, &value) {
                Ok(Some(w)) => warnings.push(w),
                Ok(None) => {}
                Err(e) if args.force => warnings.push(format!("forced: {e}")),
                Err(e) => return Err(e),
            }
            items.push((path, value));
        }
        let n = items.len();
        self.with_patch(|p| p.apply(Command::SetParams(items))).map_err(|e| e.to_string())?;
        ok(json!({"set": n, "warnings": warnings}))
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
