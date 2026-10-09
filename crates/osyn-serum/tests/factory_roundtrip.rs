//! Byte-exact CBOR round-trip over the locally installed Serum 2 preset library.
//! Skipped (passes trivially) when the library is not installed, e.g. on CI.
//! Override the location with `SERUM2_PRESETS_DIR`.

use std::path::{Path, PathBuf};

use osyn_serum::{Preset, Value, cbor, split};

const DEFAULT_DIR: &str = "/Library/Audio/Presets/Xfer Records/Serum 2 Presets/Presets";

fn library_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("SERUM2_PRESETS_DIR").map_or_else(|| PathBuf::from(DEFAULT_DIR), PathBuf::from);
    dir.is_dir().then_some(dir)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|e| e == "SerumPreset") {
            out.push(p);
        }
    }
}

#[test]
fn factory_library_reencodes_byte_exact() {
    let Some(dir) = library_dir() else {
        eprintln!("Serum 2 preset library not found; skipping");
        return;
    };
    let mut files = Vec::new();
    collect(&dir, &mut files);

    let (mut exact, mut unreadable, mut mismatched) = (0, Vec::new(), Vec::new());
    for f in &files {
        let Ok(bytes) = std::fs::read(f) else { continue };
        let Ok((_, raw)) = split(&bytes) else {
            unreadable.push(f.clone());
            continue;
        };
        let decoded = cbor::decode(&raw).expect("decode");
        if cbor::encode(&decoded) == raw {
            exact += 1;
        } else {
            mismatched.push(f.clone());
        }
    }
    eprintln!("{exact} byte-exact, {} unreadable, {} mismatched", unreadable.len(), mismatched.len());
    assert!(mismatched.is_empty(), "re-encode mismatch: {mismatched:#?}");
    // A source file with a corrupt zstd frame is tolerated, but it must stay rare.
    assert!(unreadable.len() * 100 <= files.len(), "too many unreadable: {unreadable:#?}");
}

#[test]
fn written_file_reads_back_with_valid_hash() {
    let Some(dir) = library_dir() else { return };
    let mut files = Vec::new();
    collect(&dir, &mut files);
    let src = files.iter().find_map(|f| Preset::read(f).ok()).expect("a readable preset");

    let mut p = src.clone();
    p.data.insert("presetName".into(), Value::Text("osyn roundtrip".into()));
    let bytes = p.to_bytes().unwrap();
    let back = Preset::from_bytes(&bytes).unwrap();
    assert_eq!(back.data, p.data);
    assert_eq!(back.meta["presetName"], "osyn roundtrip");
    assert_eq!(back.tags(), src.tags());

    // hash = md5(zstd frame)
    use md5::{Digest, Md5};
    let json_len = u64::from_le_bytes(bytes[9..17].try_into().unwrap()) as usize;
    let frame = &bytes[17 + json_len + 8..];
    assert_eq!(back.meta["hash"], format!("{:x}", Md5::digest(frame)));
}
