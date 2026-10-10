//! Wavetable sources and Serum rendering for the MCP tools.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use osyn_patch::WavetableSource;
use osyn_wt::{Morph, Wavetable, generate, wav};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value as Json, json};

const DEFAULT_TABLES_DIR: &str = "/Library/Audio/Presets/Xfer Records/Serum 2 Presets/Tables";
const RENDER_TIMEOUT: Duration = Duration::from_secs(150);

pub fn tables_dir() -> PathBuf {
    std::env::var_os("SERUM2_TABLES_DIR").map_or_else(|| PathBuf::from(DEFAULT_TABLES_DIR), PathBuf::from)
}

fn tools_dir() -> PathBuf {
    std::env::var_os("OSYN_TOOLS_DIR")
        .map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools"), PathBuf::from)
}

/// Relative paths of every `.wav` under the Tables folder matching `query` (case-insensitive).
pub fn list_tables(query: Option<&str>) -> Result<Vec<String>, String> {
    let root = tables_dir();
    if !root.is_dir() {
        return Err(format!("Serum tables folder not found at {} (set SERUM2_TABLES_DIR)", root.display()));
    }
    let q = query.map(str::to_ascii_lowercase);
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav")) {
                let rel = p.strip_prefix(&root).unwrap_or(&p).to_string_lossy().into_owned();
                if q.as_ref().is_none_or(|q| rel.to_ascii_lowercase().contains(q)) {
                    out.push(rel);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// A table from Serum's Tables folder, by reference or embedded.
pub fn table_from_library(rel: &str, embed: bool, morph: Option<Morph>) -> Result<(WavetableSource, usize), String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() || rel_path.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err("table path must be relative to Serum's Tables folder (see list_wavetables)".into());
    }
    let path = tables_dir().join(rel_path);
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (table, info) = wav::read_wavetable(&bytes, None).map_err(|e| e.to_string())?;
    let name = rel_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let frames = table.frames();
    if embed {
        let morph = morph.or(info.clm_morph).unwrap_or_default();
        return Ok((WavetableSource::Embedded { samples: table.samples, display_name: name, morph: morph as i64 }, frames));
    }
    Ok((
        WavetableSource::File {
            relative_path: rel.to_owned(),
            display_name: name,
            length: info.length as i64,
            channels: i64::from(info.channels),
            sample_rate: i64::from(info.sample_rate),
        },
        frames,
    ))
}

pub fn embedded(table: Wavetable, name: String, morph: Morph) -> (WavetableSource, usize) {
    let frames = table.frames();
    (WavetableSource::Embedded { samples: table.samples, display_name: name, morph: morph as i64 }, frames)
}

pub fn import_wav(path: &str, frame_size: Option<usize>) -> Result<Wavetable, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    wav::read_wavetable(&bytes, frame_size).map(|(t, _)| t).map_err(|e| e.to_string())
}

pub fn from_shapes(names: &[String], frames: usize) -> Result<Wavetable, String> {
    let shapes = names
        .iter()
        .map(|n| generate::Shape::from_name(n).ok_or_else(|| format!("unknown shape `{n}` (sine, saw, square, triangle, pulse:<duty>)")))
        .collect::<Result<Vec<_>, _>>()?;
    generate::morph_shapes(&shapes, frames).map_err(|e| e.to_string())
}

/// Export `preset_bytes` to a temp file and render it through the installed Serum 2 using
/// `tools/serum_host/render.py`. Returns the analysis JSON plus the spectrogram image.
pub async fn render(preset_bytes: Vec<u8>, args: &[(&str, String)]) -> Result<CallToolResult, String> {
    let tools = tools_dir();
    let python = tools.join("serum_host/.venv/bin/python");
    if !python.exists() {
        return Err(format!(
            "render needs the Serum host environment: run `{}` once",
            tools.join("serum_host/setup.sh").display()
        ));
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let dir = std::env::temp_dir().join("open-synth").join(format!("render-{stamp}"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let preset = dir.join("patch.SerumPreset");
    std::fs::write(&preset, preset_bytes).map_err(|e| e.to_string())?;

    let mut cmd = tokio::process::Command::new(&python);
    cmd.current_dir(&tools).arg("-m").arg("serum_host.render").arg(&preset).arg("--out").arg(&dir);
    for (flag, value) in args {
        cmd.arg(flag).arg(value);
    }
    cmd.kill_on_drop(true).stdin(std::process::Stdio::null());
    let output = tokio::time::timeout(RENDER_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "render timed out".to_string())?
        .map_err(|e| format!("failed to start renderer: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: Json = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str(l).ok())
        .ok_or_else(|| format!("renderer produced no JSON (exit {:?}): {}", output.status.code(), tail(&output.stderr)))?;
    if let Some(err) = report.get("error") {
        return Err(format!("render failed: {err}"));
    }

    let mut summary = report.clone();
    if let Some(env) = summary.get_mut("envelope").and_then(Json::as_array_mut) {
        // Keep the envelope readable: every 4th 50 ms window.
        *env = env.iter().step_by(4).cloned().collect();
    }
    summary["note"] = json!("envelope = RMS dB every 200 ms; wav/png paths are on local disk");
    let mut content = vec![ContentBlock::text(summary.to_string())];
    if let Some(png) = report.get("png").and_then(Json::as_str)
        && let Ok(bytes) = std::fs::read(png)
    {
        content.push(ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(bytes), "image/png"));
    }
    Ok(CallToolResult::success(content))
}

fn tail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(5)..].join(" | ")
}
