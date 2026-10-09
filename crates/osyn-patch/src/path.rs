//! Parameter addresses.
//!
//! `Oscillator0.kParamVolume`, `Oscillator0/WTOsc0.kParamTablePos`,
//! `FXRack0/FX[2]/FXDistortion.kParamDrive`: a `/`-separated module path (each segment a map
//! key, optionally followed by `[i]` to index into a list), then `.` and the param name, which
//! lives in that module's `plainParams` map.

use std::fmt;
use std::str::FromStr;

use crate::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Segment {
    pub key: String,
    pub index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModulePath(pub Vec<Segment>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParamPath {
    pub module: ModulePath,
    pub param: String,
}

impl ModulePath {
    /// Module type as Serum names it in mod-slot destinations: last key without trailing digits
    /// (`Oscillator0/WTOsc0` -> `WTOsc`).
    pub fn module_type(&self) -> &str {
        let last = self.0.last().map_or("", |s| s.key.as_str());
        last.trim_end_matches(|c: char| c.is_ascii_digit())
    }

    /// Trailing instance number of the last key (`VoiceFilter1` -> 1), 0 when absent.
    pub fn instance(&self) -> u32 {
        let last = self.0.last().map_or("", |s| s.key.as_str());
        let digits = &last[last.trim_end_matches(|c: char| c.is_ascii_digit()).len()..];
        digits.parse().unwrap_or(0)
    }
}

impl FromStr for ModulePath {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        let bad = || Error::Path(format!("invalid module path `{s}`"));
        if s.is_empty() {
            return Err(bad());
        }
        let segments = s
            .split('/')
            .map(|seg| {
                let (key, index) = match seg.split_once('[') {
                    Some((k, rest)) => {
                        let idx = rest.strip_suffix(']').and_then(|n| n.parse().ok()).ok_or_else(bad)?;
                        (k, Some(idx))
                    }
                    None => (seg, None),
                };
                if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Err(bad());
                }
                Ok(Segment { key: key.to_owned(), index })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ModulePath(segments))
    }
}

impl FromStr for ParamPath {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        let (module, param) = s
            .rsplit_once('.')
            .ok_or_else(|| Error::Path(format!("`{s}`: expected <module path>.<param>")))?;
        if param.is_empty() {
            return Err(Error::Path(format!("`{s}`: empty param name")));
        }
        Ok(ParamPath { module: module.parse()?, param: param.to_owned() })
    }
}

impl fmt::Display for ModulePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, seg) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            f.write_str(&seg.key)?;
            if let Some(idx) = seg.index {
                write!(f, "[{idx}]")?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for ParamPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.module, self.param)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays() {
        for s in ["Oscillator0.kParamVolume", "Oscillator0/WTOsc0.kParamTablePos", "FXRack0/FX[2]/FXDistortion.kParamDrive"] {
            let p: ParamPath = s.parse().unwrap();
            assert_eq!(p.to_string(), s);
        }
        let p: ParamPath = "Oscillator2/WTOsc2.kParamTablePos".parse().unwrap();
        assert_eq!(p.module.module_type(), "WTOsc");
        assert_eq!(p.module.instance(), 2);
    }

    #[test]
    fn rejects_garbage() {
        for s in ["", "noparam", "Osc0.", "Osc0/.kX", "FX[x].kY", "a b.kX"] {
            assert!(s.parse::<ParamPath>().is_err(), "{s}");
        }
    }
}
