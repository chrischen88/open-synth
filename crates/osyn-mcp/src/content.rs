//! Wavetable sources and Serum rendering for the MCP tools.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use osyn_patch::{SampleFile, SampleSource, WavetableSource};
use osyn_wt::{Morph, Wavetable, generate, wav};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value as Json, json};

const SERUM_CONTENT: &str = "/Library/Audio/Presets/Xfer Records/Serum 2 Presets";
const DEFAULT_TABLES_DIR: &str = "/Library/Audio/Presets/Xfer Records/Serum 2 Presets/Tables";
/// Longest audio `set_sample` will embed in a preset.
const MAX_EMBED_SECONDS: f64 = 120.0;
const RENDER_TIMEOUT: Duration = Duration::from_secs(150);

pub fn tables_dir() -> PathBuf {
    std::env::var_os("SERUM2_TABLES_DIR").map_or_else(|| PathBuf::from(DEFAULT_TABLES_DIR), PathBuf::from)
}

pub fn content_dir(kind: &str) -> PathBuf {
    let (var, sub) = match kind {
        "multisamples" => ("SERUM2_MULTISAMPLES_DIR", "Multisamples"),
        _ => ("SERUM2_SAMPLES_DIR", "Samples"),
    };
    std::env::var_os(var).map_or_else(|| Path::new(SERUM_CONTENT).join(sub), PathBuf::from)
}

fn tools_dir() -> PathBuf {
    std::env::var_os("OSYN_TOOLS_DIR")
        .map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools"), PathBuf::from)
}

/// Relative paths of every `.wav` under the Tables folder matching `query` (case-insensitive).
pub fn list_tables(query: Option<&str>) -> Result<Vec<String>, String> {
    list_files(&tables_dir(), &["wav"], query)
}

/// Relative paths of files with one of `exts` under `root`, filtered by `query`.
pub fn list_files(root: &Path, exts: &[&str], query: Option<&str>) -> Result<Vec<String>, String> {
    let root = root.to_path_buf();
    if !root.is_dir() {
        return Err(format!("Serum content folder not found at {}", root.display()));
    }
    let q = query.map(str::to_ascii_lowercase);
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x))) {
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

fn library_path(root: &Path, rel: &str, what: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() || rel_path.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("{what} path must be relative to its Serum content folder (see the list tool)"));
    }
    Ok(root.join(rel_path))
}

/// A table from Serum's Tables folder, by reference or embedded.
pub fn table_from_library(rel: &str, embed: bool, morph: Option<Morph>) -> Result<(WavetableSource, usize), String> {
    let rel_path = Path::new(rel);
    let path = library_path(&tables_dir(), rel, "table")?;
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

/// A sample from Serum's Samples folder (referenced), or any WAV on disk (embedded).
pub fn sample_source(file: Option<&str>, import: Option<&str>) -> Result<SampleSource, String> {
    match (file, import) {
        (Some(rel), None) => {
            let path = library_path(&content_dir("samples"), rel, "sample")?;
            let info = wav::audio_info(&std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?)
                .map_err(|e| e.to_string())?;
            Ok(SampleSource::File {
                relative_path: rel.to_owned(),
                info: SampleFile {
                    path: rel.to_owned(),
                    channels: i64::from(info.channels),
                    length: info.length as i64,
                    sample_rate: i64::from(info.sample_rate),
                },
            })
        }
        (None, Some(abs)) => {
            let bytes = std::fs::read(abs).map_err(|e| format!("{abs}: {e}"))?;
            let (channels, info) = wav::decode_all(&bytes).map_err(|e| format!("{abs}: {e} (WAV files only)"))?;
            if info.length as f64 / f64::from(info.sample_rate.max(1)) > MAX_EMBED_SECONDS {
                return Err(format!("{abs} is longer than {MAX_EMBED_SECONDS} s; trim it before embedding"));
            }
            let name = Path::new(abs).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            Ok(SampleSource::Embedded { channels, sample_rate: i64::from(info.sample_rate), name, original_path: abs.to_owned() })
        }
        _ => Err("give exactly one of file (Serum Samples library) or import (absolute WAV path)".into()),
    }
}

/// An SFZ from Serum's Multisamples folder: its text and header info for every sample it uses.
pub fn multisample(rel: &str) -> Result<(String, Vec<SampleFile>), String> {
    let sfz_path = library_path(&content_dir("multisamples"), rel, "multisample")?;
    let text = std::fs::read_to_string(&sfz_path).map_err(|e| format!("{}: {e}", sfz_path.display()))?;
    let dir = sfz_path.parent().unwrap_or(Path::new(""));
    let mut files: Vec<SampleFile> = Vec::new();
    for (sample, default_path) in sfz_samples(&text) {
        if files.iter().any(|f| f.path == sample) {
            continue;
        }
        let full = dir.join(default_path.replace('\\', "/")).join(sample.replace('\\', "/"));
        let info = wav::audio_info(&std::fs::read(&full).map_err(|e| format!("{rel}: {}: {e}", full.display()))?)
            .map_err(|e| format!("{rel}: {}: {e}", full.display()))?;
        files.push(SampleFile {
            path: sample,
            channels: i64::from(info.channels),
            length: info.length as i64,
            sample_rate: i64::from(info.sample_rate),
        });
    }
    if files.is_empty() {
        return Err(format!("{rel} references no samples"));
    }
    Ok((text, files))
}

/// `(sample=value, default_path in effect)` pairs. Values may contain spaces; they end where
/// the next `opcode=` starts or at end of line.
fn sfz_samples(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut default_path = String::new();
    for raw in text.lines() {
        // `//` starts a comment only at line start or after whitespace (paths may contain `//`).
        let cut = raw
            .match_indices("//")
            .find(|(i, _)| *i == 0 || raw[..*i].ends_with(char::is_whitespace))
            .map_or(raw.len(), |(i, _)| i);
        let line = &raw[..cut];
        for (key, value) in sfz_opcodes(line) {
            match key.as_str() {
                "default_path" => default_path = value,
                "sample" => out.push((value, default_path.clone())),
                _ => {}
            }
        }
    }
    out
}

fn sfz_opcodes(line: &str) -> Vec<(String, String)> {
    // Positions of `name=` where name is an opcode identifier preceded by start/space/`>`.
    let bytes = line.as_bytes();
    let mut starts = Vec::new();
    for (i, _) in line.match_indices('=') {
        let mut j = i;
        while j > 0 && (bytes[j - 1].is_ascii_alphanumeric() || bytes[j - 1] == b'_') {
            j -= 1;
        }
        if j < i && (j == 0 || bytes[j - 1] == b' ' || bytes[j - 1] == b'\t' || bytes[j - 1] == b'>') {
            starts.push((j, i));
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &(k0, eq))| {
            let end = starts.get(n + 1).map_or(line.len(), |&(next, _)| next);
            (line[k0..eq].to_owned(), line[eq + 1..end].trim().to_owned())
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sfz_sample_parsing_handles_spaces_and_default_path() {
        let sfz = "<control> default_path=Samples/\n<region> lokey=39 hikey=41 sample=Celli Tremolo/XF 02 E1.flac tune=10\n<region> sample=B.wav // comment\n<region> sample=Kick//909 BD.flac key=35";
        assert_eq!(
            sfz_samples(sfz),
            vec![
                ("Celli Tremolo/XF 02 E1.flac".to_owned(), "Samples/".to_owned()),
                ("B.wav".to_owned(), "Samples/".to_owned()),
                ("Kick//909 BD.flac".to_owned(), "Samples/".to_owned()),
            ]
        );
    }

    /// Every factory SFZ resolves all of its samples (skipped when not installed).
    #[test]
    fn factory_multisamples_resolve() {
        let root = content_dir("multisamples");
        let Ok(list) = list_files(&root, &["sfz"], None) else { return };
        let failed: Vec<String> = list.iter().filter_map(|rel| multisample(rel).err()).collect();
        eprintln!("{} sfz, {} failed", list.len(), failed.len());
        assert!(failed.is_empty(), "{failed:#?}");
    }
}
