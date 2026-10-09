//! Editable patch model.
//!
//! The patch *is* a Serum 2 module tree (see `osyn-serum`), so export is plain serialization.
//! Params left at Serum's default are simply absent; a module with no non-default params is
//! stored as `"plainParams": "default"`, exactly as Serum writes it.
//!
//! All edits go through [`Patch::apply`], which validates nothing about *meaning* (that is the
//! param registry's job) but guarantees structural consistency and records undo history.

mod json;
pub mod path;

use osyn_serum::{Map, Preset, Value};

pub use json::{to_json, value_from_json};
pub use path::{ModulePath, ParamPath, Segment};

const PLAIN: &str = "plainParams";
const DEFAULT: &str = "default";
const MOD_SLOTS: usize = 64;
const HISTORY_LIMIT: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Path(String),
    #[error("module `{0}` not found")]
    NoModule(String),
    #[error("`{0}` is not a map")]
    NotAMap(String),
    #[error("all {MOD_SLOTS} mod slots are in use")]
    ModSlotsFull,
    #[error("mod slot {0} out of range")]
    BadModSlot(usize),
    #[error("FX rack {0} out of range (0 = Main, 1 = Bus 1, 2 = Bus 2)")]
    BadRack(usize),
    #[error("FX rack {rack} has no module at index {index}")]
    BadFxIndex { rack: usize, index: usize },
    #[error(transparent)]
    Serum(#[from] osyn_serum::Error),
}

/// A modulation routing as Serum stores it in `ModSlot{n}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Modulation {
    /// Mod source id (see the registry's source table); 0 = none.
    pub source: i64,
    /// Aux source id; 0 = none.
    pub aux_source: i64,
    pub dest_module_type: String,
    pub dest_module_id: i64,
    pub dest_param_name: String,
    pub dest_param_id: i64,
    /// Mod-slot plain params, e.g. `kParamAmount` (percent), `kParamBipolar`.
    pub params: Map,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    SetParams(Vec<(ParamPath, Value)>),
    ResetParams(Vec<ParamPath>),
    /// Place a modulation in the given slot, or the first free slot when `None`.
    SetModulation { slot: Option<usize>, modulation: Modulation },
    ClearModulation(usize),
    /// Top-level preset fields: presetName, presetAuthor, presetDescription.
    SetInfo { key: String, value: String },
    SetTags(Vec<String>),
    /// Insert an FX module into rack 0..=2 (Main, Bus 1, Bus 2) at `index` (clamped to the end).
    /// `type_id` is the module's numeric FX type (schema `fxTypeId`).
    InsertFx { rack: usize, index: usize, fx_type: String, type_id: i64 },
    /// Remove an FX module; mod slots targeting it are cleared, later ones renumbered.
    RemoveFx { rack: usize, index: usize },
    /// Replace the whole tree (import / load).
    Replace(Box<Preset>),
}

#[derive(Debug, Clone)]
pub struct Patch {
    preset: Preset,
    undo: Vec<Preset>,
    redo: Vec<Preset>,
}

impl Default for Patch {
    fn default() -> Self {
        Self::new(init_preset())
    }
}

impl Patch {
    pub fn new(preset: Preset) -> Self {
        Patch { preset, undo: Vec::new(), redo: Vec::new() }
    }

    pub fn preset(&self) -> &Preset {
        &self.preset
    }

    /// Apply a command atomically: on error the patch is unchanged. Returns the slot used for
    /// `SetModulation`, otherwise `None`.
    pub fn apply(&mut self, cmd: Command) -> Result<Option<usize>, Error> {
        let mut next = self.preset.clone();
        let out = apply_to(&mut next, cmd)?;
        let prev = std::mem::replace(&mut self.preset, next);
        self.undo.push(prev);
        if self.undo.len() > HISTORY_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        Ok(out)
    }

    pub fn undo(&mut self) -> bool {
        match self.undo.pop() {
            Some(prev) => {
                self.redo.push(std::mem::replace(&mut self.preset, prev));
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(std::mem::replace(&mut self.preset, next));
                true
            }
            None => false,
        }
    }

    /// Current value, or `None` when the param is at Serum's default.
    pub fn get(&self, path: &ParamPath) -> Option<&Value> {
        let module = resolve(&self.preset.data, &path.module)?;
        module.get(PLAIN)?.as_map()?.get(&path.param)
    }

    /// Every non-default param in the patch, in tree order.
    pub fn non_default(&self) -> Vec<(ParamPath, Value)> {
        let mut out = Vec::new();
        collect_params(&self.preset.data, &mut Vec::new(), &mut out);
        out
    }

    pub fn modulations(&self) -> Vec<(usize, Modulation)> {
        (0..MOD_SLOTS)
            .filter_map(|i| {
                let slot = self.preset.data.get(&format!("ModSlot{i}"))?.as_map()?;
                read_modulation(slot).map(|m| (i, m))
            })
            .collect()
    }
}

/// Minimal patch Serum accepts (verified in Serum 2.1.5: omitted modules load as defaults).
pub fn init_preset() -> Preset {
    let mut data = Map::new();
    let text = |s: &str| Value::Text(s.to_owned());
    for (k, v) in [
        ("arpBankDisplayName", text("")),
        ("clipBankDisplayName", text("")),
        ("fileType", text("SerumPreset")),
        ("lockOversampling", Value::Bool(false)),
        ("lockTuning", Value::Bool(false)),
        ("mpeConfig", Value::Int(0)),
        ("mpeEnabled", Value::Bool(false)),
        ("mpePitchBendRange", Value::Int(48)),
        ("presetAuthor", text("")),
        ("presetDescription", text("")),
        ("presetName", text("Init")),
        ("product", text("Serum2")),
        ("productVersion", text("2.0.24")),
        ("url", text("https://xferrecords.com/")),
        ("vendor", text("Xfer Records")),
        ("version", Value::Float(9.0)),
    ] {
        data.insert(k.to_owned(), v);
    }
    Preset { meta: Default::default(), data }
}

// --- command implementation ----------------------------------------------------------------

fn apply_to(p: &mut Preset, cmd: Command) -> Result<Option<usize>, Error> {
    match cmd {
        Command::SetParams(items) => {
            for (path, value) in items {
                plain_params_mut(&mut p.data, &path.module)?.insert(path.param, value);
            }
        }
        Command::ResetParams(paths) => {
            for path in paths {
                let Some(module) = resolve_mut(&mut p.data, &path.module) else { continue };
                if let Some(Value::Map(params)) = module.get_mut(PLAIN) {
                    params.shift_remove(&path.param);
                    if params.is_empty() {
                        module.insert(PLAIN.into(), Value::Text(DEFAULT.into()));
                    }
                }
            }
        }
        Command::SetModulation { slot, modulation } => {
            let idx = match slot {
                Some(i) if i < MOD_SLOTS => i,
                Some(i) => return Err(Error::BadModSlot(i)),
                None => (0..MOD_SLOTS)
                    .find(|i| {
                        p.data.get(&format!("ModSlot{i}")).and_then(Value::as_map).is_none_or(|m| !m.contains_key("source"))
                    })
                    .ok_or(Error::ModSlotsFull)?,
            };
            p.data.insert(format!("ModSlot{idx}"), Value::Map(write_modulation(&modulation)));
            return Ok(Some(idx));
        }
        Command::ClearModulation(i) => {
            if i >= MOD_SLOTS {
                return Err(Error::BadModSlot(i));
            }
            p.data.shift_remove(&format!("ModSlot{i}"));
        }
        Command::SetInfo { key, value } => {
            if !matches!(key.as_str(), "presetName" | "presetAuthor" | "presetDescription") {
                return Err(Error::Path(format!("`{key}` is not an editable info field")));
            }
            p.data.insert(key, Value::Text(value));
        }
        Command::SetTags(tags) => {
            p.meta.insert("tags".into(), serde_json::Value::from(tags));
        }
        Command::InsertFx { rack, index, fx_type, type_id } => {
            let list = fx_list_mut(&mut p.data, rack)?;
            let index = index.min(list.len());
            let mut module = Map::new();
            module.insert(PLAIN.into(), Value::Text(DEFAULT.into()));
            let mut item = Map::new();
            item.insert(fx_type, Value::Map(module));
            item.insert("type".into(), Value::Int(type_id));
            list.insert(index, Value::Map(item));
            shift_fx_mod_targets(&mut p.data, rack, index, 1);
            return Ok(Some(index));
        }
        Command::RemoveFx { rack, index } => {
            let list = fx_list_mut(&mut p.data, rack)?;
            if index >= list.len() {
                return Err(Error::BadFxIndex { rack, index });
            }
            list.remove(index);
            shift_fx_mod_targets(&mut p.data, rack, index, -1);
        }
        Command::Replace(preset) => *p = *preset,
    }
    Ok(None)
}

pub const FX_RACKS: usize = 3;

/// Mod-slot destination id of an FX module: `rack * 100 + index` (verified on factory presets).
pub fn fx_dest_id(rack: usize, index: usize) -> i64 {
    (rack * 100 + index) as i64
}

fn fx_list_mut(root: &mut Map, rack: usize) -> Result<&mut Vec<Value>, Error> {
    if rack >= FX_RACKS {
        return Err(Error::BadRack(rack));
    }
    let entry = root.entry(format!("FXRack{rack}")).or_insert_with(|| {
        let mut m = Map::new();
        m.insert("FX".into(), Value::Array(Vec::new()));
        m.insert("displayName".into(), Value::Text(String::new()));
        m.insert(PLAIN.into(), Value::Text(DEFAULT.into()));
        Value::Map(m)
    });
    let rack_map = entry.as_map_mut().ok_or_else(|| Error::NotAMap(format!("FXRack{rack}")))?;
    match rack_map.entry("FX".to_owned()).or_insert_with(|| Value::Array(Vec::new())) {
        Value::Array(list) => Ok(list),
        _ => Err(Error::NotAMap(format!("FXRack{rack}/FX"))),
    }
}

/// Keep mod-slot FX destinations pointing at the same modules after an insert (`delta = 1`)
/// or removal (`delta = -1`, slots targeting the removed module are cleared).
fn shift_fx_mod_targets(root: &mut Map, rack: usize, at: usize, delta: i64) {
    let (lo, hi) = (fx_dest_id(rack, 0), fx_dest_id(rack, 99));
    let removed = fx_dest_id(rack, at);
    let mut clear = Vec::new();
    for i in 0..MOD_SLOTS {
        let key = format!("ModSlot{i}");
        let Some(slot) = root.get_mut(&key).and_then(Value::as_map_mut) else { continue };
        let is_fx = slot.get("destModuleTypeString").and_then(Value::as_str).is_some_and(|t| t.starts_with("FX"));
        let Some(Value::Int(id)) = slot.get_mut("destModuleID") else { continue };
        if !is_fx || *id < lo || *id > hi || *id < removed {
            continue;
        }
        if delta < 0 && *id == removed {
            clear.push(key);
        } else {
            *id += delta;
        }
    }
    for key in clear {
        root.shift_remove(&key);
    }
}

fn resolve<'a>(root: &'a Map, path: &ModulePath) -> Option<&'a Map> {
    let mut cur = root;
    for seg in &path.0 {
        let v = cur.get(&seg.key)?;
        cur = match seg.index {
            Some(i) => match v {
                Value::Array(items) => items.get(i)?.as_map()?,
                _ => return None,
            },
            None => v.as_map()?,
        };
    }
    Some(cur)
}

fn resolve_mut<'a>(root: &'a mut Map, path: &ModulePath) -> Option<&'a mut Map> {
    let mut cur = root;
    for seg in &path.0 {
        let v = cur.get_mut(&seg.key)?;
        cur = match seg.index {
            Some(i) => match v {
                Value::Array(items) => items.get_mut(i)?.as_map_mut()?,
                _ => return None,
            },
            None => v.as_map_mut()?,
        };
    }
    Some(cur)
}

/// Module's `plainParams` map, creating keyed modules on demand. List items (`FX[i]`) must
/// already exist: inserting FX modules is a separate, structural operation.
fn plain_params_mut<'a>(root: &'a mut Map, path: &ModulePath) -> Result<&'a mut Map, Error> {
    let mut cur = root;
    for seg in &path.0 {
        let entry = cur.entry(seg.key.clone()).or_insert_with(|| Value::Map(Map::new()));
        cur = match seg.index {
            Some(i) => match entry {
                Value::Array(items) => items
                    .get_mut(i)
                    .and_then(Value::as_map_mut)
                    .ok_or_else(|| Error::NoModule(path.to_string()))?,
                _ => return Err(Error::NoModule(path.to_string())),
            },
            None => entry.as_map_mut().ok_or_else(|| Error::NotAMap(path.to_string()))?,
        };
    }
    let plain = cur.entry(PLAIN.to_owned()).or_insert_with(|| Value::Map(Map::new()));
    if !matches!(plain, Value::Map(_)) {
        *plain = Value::Map(Map::new()); // was "default"
    }
    Ok(plain.as_map_mut().expect("just ensured map"))
}

fn collect_params(map: &Map, prefix: &mut Vec<Segment>, out: &mut Vec<(ParamPath, Value)>) {
    for (k, v) in map {
        if k == PLAIN {
            if let Value::Map(params) = v {
                for (name, value) in params {
                    out.push((ParamPath { module: ModulePath(prefix.clone()), param: name.clone() }, value.clone()));
                }
            }
            continue;
        }
        match v {
            Value::Map(child) => {
                prefix.push(Segment { key: k.clone(), index: None });
                collect_params(child, prefix, out);
                prefix.pop();
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    // FX list items are {"FXDistortion": {...}, "type": n}: descend into item keys.
                    if let Value::Map(child) = item {
                        prefix.push(Segment { key: k.clone(), index: Some(i) });
                        collect_params(child, prefix, out);
                        prefix.pop();
                    }
                }
            }
            _ => {}
        }
    }
}

fn read_modulation(slot: &Map) -> Option<Modulation> {
    let Value::Array(src) = slot.get("source")? else { return None };
    let int = |v: Option<&Value>| match v {
        Some(Value::Int(i)) => Some(*i),
        _ => None,
    };
    Some(Modulation {
        source: int(src.first())?,
        aux_source: int(src.get(1)).unwrap_or(0),
        dest_module_type: slot.get("destModuleTypeString").and_then(Value::as_str).unwrap_or_default().to_owned(),
        dest_module_id: int(slot.get("destModuleID")).unwrap_or(0),
        dest_param_name: slot.get("destModuleParamName").and_then(Value::as_str).unwrap_or_default().to_owned(),
        dest_param_id: int(slot.get("destModuleParamID")).unwrap_or(0),
        params: slot.get(PLAIN).and_then(Value::as_map).cloned().unwrap_or_default(),
    })
}

fn write_modulation(m: &Modulation) -> Map {
    // Key order as Serum writes it.
    let mut slot = Map::new();
    slot.insert("destModuleID".into(), Value::Int(m.dest_module_id));
    slot.insert("destModuleParamID".into(), Value::Int(m.dest_param_id));
    slot.insert("destModuleParamName".into(), Value::Text(m.dest_param_name.clone()));
    slot.insert("destModuleTypeString".into(), Value::Text(m.dest_module_type.clone()));
    let plain = if m.params.is_empty() { Value::Text(DEFAULT.into()) } else { Value::Map(m.params.clone()) };
    slot.insert(PLAIN.into(), plain);
    slot.insert("source".into(), Value::Array(vec![Value::Int(m.source), Value::Int(m.aux_source)]));
    slot
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pp(s: &str) -> ParamPath {
        s.parse().unwrap()
    }

    #[test]
    fn set_reset_and_default_marker() {
        let mut patch = Patch::default();
        let path = pp("VoiceFilter0.kParamType");
        patch.apply(Command::SetParams(vec![(path.clone(), Value::Text("LadderAcid".into()))])).unwrap();
        assert_eq!(patch.get(&path), Some(&Value::Text("LadderAcid".into())));

        patch.apply(Command::ResetParams(vec![path.clone()])).unwrap();
        assert_eq!(patch.get(&path), None);
        let module = patch.preset().data["VoiceFilter0"].as_map().unwrap();
        assert_eq!(module[PLAIN], Value::Text(DEFAULT.into()));
    }

    #[test]
    fn nested_paths_and_listing() {
        let mut patch = Patch::default();
        patch.apply(Command::SetParams(vec![(pp("Oscillator0/WTOsc0.kParamTablePos"), Value::Float(0.5))])).unwrap();
        let all = patch.non_default();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0.to_string(), "Oscillator0/WTOsc0.kParamTablePos");
    }

    #[test]
    fn missing_fx_list_item_is_an_error_and_patch_unchanged() {
        let mut patch = Patch::default();
        let before = patch.preset().clone();
        let err = patch.apply(Command::SetParams(vec![
            (pp("Global0.kParamMasterVolume"), Value::Float(0.5)),
            (pp("FXRack0/FX[3]/FXReverb.kParamWet"), Value::Float(50.0)),
        ]));
        assert!(err.is_err());
        assert_eq!(patch.preset(), &before);
    }

    #[test]
    fn modulation_slots_and_undo() {
        let mut patch = Patch::default();
        let m = Modulation {
            source: 6,
            aux_source: 0,
            dest_module_type: "Oscillator".into(),
            dest_module_id: 0,
            dest_param_name: "kParamVolume".into(),
            dest_param_id: 1,
            params: Map::from_iter([("kParamAmount".to_owned(), Value::Float(50.0))]),
        };
        assert_eq!(patch.apply(Command::SetModulation { slot: None, modulation: m.clone() }).unwrap(), Some(0));
        assert_eq!(patch.apply(Command::SetModulation { slot: None, modulation: m.clone() }).unwrap(), Some(1));
        assert_eq!(patch.modulations(), vec![(0, m.clone()), (1, m.clone())]);
        assert!(patch.undo());
        assert_eq!(patch.modulations().len(), 1);
        assert!(patch.redo());
        assert_eq!(patch.modulations().len(), 2);
    }

    #[test]
    fn fx_insert_remove_keeps_mod_targets() {
        let mut patch = Patch::default();
        for (i, t) in ["FXDistortion", "FXReverb"].into_iter().enumerate() {
            patch.apply(Command::InsertFx { rack: 0, index: i, fx_type: t.into(), type_id: i as i64 }).unwrap();
        }
        let reverb_mod = Modulation {
            source: 6,
            aux_source: 0,
            dest_module_type: "FXReverb".into(),
            dest_module_id: fx_dest_id(0, 1),
            dest_param_name: "kParamWet".into(),
            dest_param_id: 0,
            params: Map::new(),
        };
        patch.apply(Command::SetModulation { slot: None, modulation: reverb_mod }).unwrap();
        patch.apply(Command::SetParams(vec![(pp("FXRack0/FX[1]/FXReverb.kParamWet"), Value::Float(30.0))])).unwrap();

        // Insert at the front: the reverb moves to index 2 and its mod follows.
        patch.apply(Command::InsertFx { rack: 0, index: 0, fx_type: "FXEQ".into(), type_id: 7 }).unwrap();
        assert_eq!(patch.modulations()[0].1.dest_module_id, 2);
        assert!(patch.get(&pp("FXRack0/FX[2]/FXReverb.kParamWet")).is_some());

        // Removing the reverb clears its mod slot.
        patch.apply(Command::RemoveFx { rack: 0, index: 2 }).unwrap();
        assert!(patch.modulations().is_empty());
        assert!(patch.apply(Command::RemoveFx { rack: 0, index: 9 }).is_err());
        assert!(patch.apply(Command::InsertFx { rack: 3, index: 0, fx_type: "FXEQ".into(), type_id: 7 }).is_err());
    }

    #[test]
    fn exported_bytes_reload() {
        let mut patch = Patch::default();
        patch.apply(Command::SetInfo { key: "presetName".into(), value: "Test".into() }).unwrap();
        patch.apply(Command::SetTags(vec!["Bass".into()])).unwrap();
        let bytes = patch.preset().to_bytes().unwrap();
        let back = Preset::from_bytes(&bytes).unwrap();
        assert_eq!(back.name(), Some("Test"));
        assert_eq!(back.tags(), vec!["Bass".to_owned()]);
    }
}
