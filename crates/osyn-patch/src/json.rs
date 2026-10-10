//! JSON views of patch values for tools and UIs.
//!
//! Byte blobs (embedded audio, curve data) are summarized as `{"$bytes": <len>}` and cannot be
//! set through JSON.

use osyn_serum::{Map, Value};
use serde_json::{Number, Value as Json};

pub fn to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        Value::Float(f) => Number::from_f64(*f).map_or(Json::Null, Json::Number),
        Value::Text(s) => Json::String(s.clone()),
        Value::Bytes(b) => serde_json::json!({ "$bytes": b.len() }),
        Value::Array(items) => Json::Array(items.iter().map(to_json).collect()),
        Value::Map(m) => Json::Object(m.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
    }
}

/// Scalar params only. Numbers become floats, matching how Serum stores plain params
/// (integers in JSON like `7` are stored as `7.0`).
pub fn value_from_json(j: &Json) -> Option<Value> {
    Some(match j {
        Json::Bool(b) => Value::Float(if *b { 1.0 } else { 0.0 }), // Serum stores toggles as 0.0/1.0
        Json::Number(n) => Value::Float(n.as_f64()?),
        Json::String(s) => Value::Text(s.clone()),
        Json::Object(o) => Value::Map(o.iter().map(|(k, v)| Some((k.clone(), value_from_json(v)?))).collect::<Option<Map>>()?),
        Json::Array(a) => Value::Array(a.iter().map(value_from_json).collect::<Option<Vec<_>>>()?),
        Json::Null => return None,
    })
}
