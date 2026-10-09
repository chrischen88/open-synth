//! Minimal CBOR codec matching the byte layout Serum 2 writes.
//!
//! Serum's encoder: definite lengths only, no tags, map keys are always text and keep
//! insertion order, floats are float32 when lossless and float64 otherwise (never float16).
//! Decoding is slightly more permissive (accepts float16) so hand-made files still load.

use indexmap::IndexMap;

use crate::Error;

pub type Map = IndexMap<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    Map(Map),
}

impl Value {
    pub fn as_map(&self) -> Option<&Map> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_map_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Numeric view of Int or Float.
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Value::Int(i) => Some(i as f64),
            Value::Float(f) => Some(f),
            _ => None,
        }
    }
}

// --- encoding ------------------------------------------------------------------------------

pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 * 1024);
    encode_into(value, &mut out);
    out
}

fn head(major: u8, n: u64, out: &mut Vec<u8>) {
    let m = major << 5;
    match n {
        0..=23 => out.push(m | n as u8),
        24..=0xFF => out.extend_from_slice(&[m | 24, n as u8]),
        0x100..=0xFFFF => {
            out.push(m | 25);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        0x1_0000..=0xFFFF_FFFF => {
            out.push(m | 26);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
        _ => {
            out.push(m | 27);
            out.extend_from_slice(&n.to_be_bytes());
        }
    }
}

fn encode_into(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(0xF6),
        Value::Bool(false) => out.push(0xF4),
        Value::Bool(true) => out.push(0xF5),
        Value::Int(i) if *i >= 0 => head(0, *i as u64, out),
        Value::Int(i) => head(1, !(*i) as u64, out),
        Value::Float(f) => {
            let single = *f as f32;
            if single as f64 == *f || f.is_nan() {
                out.push(0xFA);
                out.extend_from_slice(&single.to_be_bytes());
            } else {
                out.push(0xFB);
                out.extend_from_slice(&f.to_be_bytes());
            }
        }
        Value::Text(s) => {
            head(3, s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            head(2, b.len() as u64, out);
            out.extend_from_slice(b);
        }
        Value::Array(items) => {
            head(4, items.len() as u64, out);
            items.iter().for_each(|v| encode_into(v, out));
        }
        Value::Map(map) => {
            head(5, map.len() as u64, out);
            for (k, v) in map {
                head(3, k.len() as u64, out);
                out.extend_from_slice(k.as_bytes());
                encode_into(v, out);
            }
        }
    }
}

// --- decoding ------------------------------------------------------------------------------

pub fn decode(bytes: &[u8]) -> Result<Value, Error> {
    let mut r = Reader { buf: bytes, pos: 0 };
    let v = r.value(0)?;
    if r.pos != bytes.len() {
        return Err(Error::Cbor(format!("{} trailing bytes", bytes.len() - r.pos)));
    }
    Ok(v)
}

const MAX_DEPTH: usize = 64;

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or_else(|| Error::Cbor(format!("unexpected end at offset {}", self.pos)))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn arg(&mut self, info: u8) -> Result<u64, Error> {
        Ok(match info {
            0..=23 => info as u64,
            24 => self.take(1)?[0] as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().unwrap()),
            _ => return Err(Error::Cbor(format!("unsupported additional info {info} at {}", self.pos))),
        })
    }

    fn len(&mut self, info: u8) -> Result<usize, Error> {
        let n = self.arg(info)?;
        usize::try_from(n)
            .ok()
            .filter(|&n| n <= self.buf.len())
            .ok_or_else(|| Error::Cbor(format!("length {n} out of range")))
    }

    fn text(&mut self, info: u8) -> Result<String, Error> {
        let n = self.len(info)?;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|e| Error::Cbor(e.to_string()))
    }

    fn value(&mut self, depth: usize) -> Result<Value, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::Cbor("nesting too deep".into()));
        }
        let ib = self.take(1)?[0];
        let (major, info) = (ib >> 5, ib & 0x1F);
        Ok(match major {
            0 => Value::Int(i64::try_from(self.arg(info)?).map_err(|_| Error::Cbor("uint overflow".into()))?),
            1 => {
                let n = i64::try_from(self.arg(info)?).map_err(|_| Error::Cbor("nint overflow".into()))?;
                Value::Int(-1 - n)
            }
            2 => {
                let n = self.len(info)?;
                Value::Bytes(self.take(n)?.to_vec())
            }
            3 => Value::Text(self.text(info)?),
            4 => {
                let n = self.len(info)?;
                let mut items = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    items.push(self.value(depth + 1)?);
                }
                Value::Array(items)
            }
            5 => {
                let n = self.len(info)?;
                let mut map = Map::with_capacity(n.min(4096));
                for _ in 0..n {
                    let kb = self.take(1)?[0];
                    if kb >> 5 != 3 {
                        return Err(Error::Cbor(format!("non-text map key at {}", self.pos - 1)));
                    }
                    let k = self.text(kb & 0x1F)?;
                    let v = self.value(depth + 1)?;
                    map.insert(k, v);
                }
                Value::Map(map)
            }
            7 => match info {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                22 => Value::Null,
                25 => Value::Float(f16_to_f64(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))),
                26 => Value::Float(f32::from_be_bytes(self.take(4)?.try_into().unwrap()) as f64),
                27 => Value::Float(f64::from_be_bytes(self.take(8)?.try_into().unwrap())),
                _ => return Err(Error::Cbor(format!("unsupported simple value {info}"))),
            },
            _ => return Err(Error::Cbor(format!("unsupported major type {major} (tags/indefinite)"))),
        })
    }
}

fn f16_to_f64(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1F) as i32;
    let frac = (h & 0x3FF) as f64;
    sign * match exp {
        0 => frac * 2f64.powi(-24),
        31 if frac == 0.0 => f64::INFINITY,
        31 => f64::NAN,
        _ => (1.0 + frac / 1024.0) * 2f64.powi(exp - 15),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_use_shortest_of_f32_f64() {
        assert_eq!(encode(&Value::Float(1.0)), [0xFA, 0x3F, 0x80, 0, 0]);
        assert_eq!(encode(&Value::Float(0.1))[0], 0xFB);
    }

    #[test]
    fn roundtrip_preserves_key_order() {
        let mut m = Map::new();
        m.insert("z".into(), Value::Int(-500));
        m.insert("a".into(), Value::Array(vec![Value::Text("x".into()), Value::Bytes(vec![1, 2])]));
        m.insert("b".into(), Value::Map(Map::new()));
        let v = Value::Map(m);
        assert_eq!(decode(&encode(&v)).unwrap(), v);
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(decode(&[0x82, 0x01]).is_err());
    }
}
