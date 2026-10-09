//! Serum 2 `.SerumPreset` container.
//!
//! ```text
//! b"XferJson\0" | u64le json_len | metadata JSON | u32le cbor_len | u32le 2 | zstd(CBOR)
//! ```
//! `metadata.hash` is the MD5 of the zstd frame. The CBOR payload is a map of modules
//! (`Oscillator0`, `VoiceFilter0`, `ModSlot12`, ...). Byte-exact re-encoding is verified
//! against the factory library in `tests/factory_roundtrip.rs`.

pub mod cbor;

use std::path::Path;

use md5::{Digest, Md5};
use serde_json::{Map as JsonMap, Value as Json};

pub use cbor::{Map, Value};

const MAGIC: &[u8; 9] = b"XferJson\0";
const FORMAT_CONST: u32 = 2;
const ZSTD_LEVEL: i32 = 3;
/// Metadata keys in the order Serum writes them (sorted).
const META_KEYS: [&str; 11] = [
    "fileType",
    "hash",
    "presetAuthor",
    "presetDescription",
    "presetName",
    "product",
    "productVersion",
    "tags",
    "url",
    "vendor",
    "version",
];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a SerumPreset: {0}")]
    Format(String),
    #[error("cbor: {0}")]
    Cbor(String),
    #[error("metadata json: {0}")]
    Json(#[from] serde_json::Error),
}

/// A decoded preset: the JSON header plus the CBOR module tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    pub meta: JsonMap<String, Json>,
    pub data: Map,
}

impl Preset {
    pub fn name(&self) -> Option<&str> {
        self.data.get("presetName").and_then(Value::as_str)
    }

    pub fn tags(&self) -> Vec<String> {
        self.meta
            .get("tags")
            .and_then(Json::as_array)
            .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let (meta, cbor) = split(bytes)?;
        let data = match cbor::decode(&cbor)? {
            Value::Map(m) => m,
            _ => return Err(Error::Format("payload is not a map".into())),
        };
        Ok(Preset { meta, data })
    }

    pub fn read(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    /// Serialize. Metadata is rebuilt from the module tree's top-level fields plus `tags`,
    /// and the hash is recomputed, so edits to `data` are always reflected in the header.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        let cbor = cbor::encode(&Value::Map(self.data.clone()));
        let frame = zstd::bulk::compress(&cbor, ZSTD_LEVEL)?;
        let hash = format!("{:x}", Md5::digest(&frame));

        let mut meta = JsonMap::new();
        for key in META_KEYS {
            let v = match key {
                "hash" => Json::String(hash.clone()),
                "tags" => Json::Array(self.tags().into_iter().map(Json::String).collect()),
                _ => match self.data.get(key) {
                    Some(Value::Text(s)) => Json::String(s.clone()),
                    Some(Value::Float(f)) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
                    Some(Value::Int(i)) => Json::from(*i),
                    _ => Json::String(String::new()),
                },
            };
            meta.insert(key.to_owned(), v);
        }
        let json = serde_json::to_vec(&meta)?;

        let mut out = Vec::with_capacity(MAGIC.len() + 16 + json.len() + frame.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(json.len() as u64).to_le_bytes());
        out.extend_from_slice(&json);
        out.extend_from_slice(&(cbor.len() as u32).to_le_bytes());
        out.extend_from_slice(&FORMAT_CONST.to_le_bytes());
        out.extend_from_slice(&frame);
        Ok(out)
    }

    pub fn write(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        Ok(std::fs::write(path, self.to_bytes()?)?)
    }
}

/// Split a file into (metadata, raw CBOR bytes), verifying the declared size.
pub fn split(bytes: &[u8]) -> Result<(JsonMap<String, Json>, Vec<u8>), Error> {
    let fmt = |m: &str| Error::Format(m.to_owned());
    if !bytes.starts_with(MAGIC) {
        return Err(fmt("bad magic"));
    }
    let mut pos = MAGIC.len();
    let json_len = read_u64(bytes, pos).ok_or_else(|| fmt("truncated header"))?;
    pos += 8;
    let json_end = usize::try_from(json_len)
        .ok()
        .and_then(|n| pos.checked_add(n))
        .filter(|&e| e <= bytes.len())
        .ok_or_else(|| fmt("metadata length out of range"))?;
    let meta: JsonMap<String, Json> = serde_json::from_slice(&bytes[pos..json_end])?;
    pos = json_end;
    let cbor_len = read_u32(bytes, pos).ok_or_else(|| fmt("truncated payload header"))? as usize;
    pos += 8; // cbor_len + format constant
    let frame = bytes.get(pos..).ok_or_else(|| fmt("missing payload"))?;
    let cbor = zstd::bulk::decompress(frame, cbor_len)?;
    if cbor.len() != cbor_len {
        return Err(fmt(&format!("payload size {} != declared {cbor_len}", cbor.len())));
    }
    Ok((meta, cbor))
}

fn read_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn read_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
