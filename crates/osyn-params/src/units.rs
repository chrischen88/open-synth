//! Knob position ↔ stored value ↔ displayed value, per Serum 2 parameter.
//!
//! `data/serum2_units.json` is produced by `tools/serum_host/sweep.py`, which hosts Serum 2,
//! sets every automatable param to 9 knob positions and records what Serum stores in the
//! preset (`transform`) and what its UI shows (`display.samples`, plus a fitted law).
//!
//! Terms: *knob* = Serum's normalized 0..1 automation value; *plain* = the value stored in
//! `plainParams` (what `osyn-patch` holds); *display* = number + unit as shown in Serum.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value as Json;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFile {
    params: Vec<RawParam>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawParam {
    host: String,
    path: String,
    default_raw: f64,
    #[serde(default)]
    default_plain: Json,
    transform: Transform,
    display: RawDisplay,
}

#[derive(Debug, Clone, Deserialize)]
struct RawDisplay {
    #[serde(default)]
    unit: String,
    samples: Vec<(f64, String)>,
    #[serde(default)]
    fit: Option<Law>,
}

/// Knob → plain.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Transform {
    Affine { a: f64, b: f64 },
    Power { a: f64, b: f64, k: f64 },
    Floor { a: f64, n: f64, hi: f64 },
    Toggle { threshold: f64, low: f64, high: f64 },
    /// Steps of `[lo, hi, token, display]`; a `null` token means the key is omitted.
    Enum { steps: Vec<(f64, f64, Option<String>, String)> },
}

/// Knob → displayed number.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Law {
    Affine { a: f64, b: f64 },
    Power { a: f64, b: f64, k: f64 },
    Exp { a: f64, b: f64 },
    /// `c * log10(knob) + d`, fitted at load time from dB display samples.
    #[serde(skip)]
    Log { c: f64, d: f64 },
}

impl Law {
    fn eval(&self, r: f64) -> f64 {
        match *self {
            Law::Affine { a, b } => a + b * r,
            Law::Power { a, b, k } => a + b * r.powf(k),
            Law::Exp { a, b } => a * b.powf(r),
            Law::Log { c, d } => c * r.max(1e-9).log10() + d,
        }
    }

    fn invert(&self, y: f64) -> f64 {
        let r = match *self {
            Law::Affine { a, b } => (y - a) / b,
            Law::Power { a, b, k } => ((y - a) / b).max(0.0).powf(1.0 / k),
            Law::Exp { a, b } => (y / a).ln() / b.ln(),
            Law::Log { c, d } => 10f64.powf((y - d) / c),
        };
        if r.is_finite() { r } else { 0.0 }
    }
}

/// Displayed quantity, in canonical units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unit {
    Hz,
    Seconds,
    Decibels,
    Percent,
    Degrees,
    /// Unitless number as Serum shows it (semitones, voices, pan, ...).
    Number,
}

impl Unit {
    fn suffix(self) -> &'static str {
        match self {
            Unit::Hz => " Hz",
            Unit::Seconds => " s",
            Unit::Decibels => " dB",
            Unit::Percent => "%",
            Unit::Degrees => "°",
            Unit::Number => "",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParamUnits {
    pub host: String,
    pub path: String,
    pub default_knob: f64,
    pub default_plain: Json,
    pub transform: Transform,
    /// Display laws available for this param (e.g. a level knob has both % and dB).
    pub laws: Vec<(Unit, Law)>,
    pub samples: Vec<(f64, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Plain {
    Number(f64),
    /// Enum token; `None` = omit the key (Serum's "Off" for warp menus).
    Token(Option<String>),
}

impl ParamUnits {
    pub fn knob_to_plain(&self, r: f64) -> Plain {
        let r = r.clamp(0.0, 1.0);
        match &self.transform {
            Transform::Affine { a, b } => Plain::Number(a + b * r),
            Transform::Power { a, b, k } => Plain::Number(a + b * r.powf(*k)),
            Transform::Floor { a, n, hi } => Plain::Number((a + (n * r).floor()).min(*hi)),
            Transform::Toggle { threshold, low, high } => Plain::Number(if r >= *threshold { *high } else { *low }),
            Transform::Enum { steps } => {
                let step = steps.iter().find(|s| r < s.1).or(steps.last()).expect("enum has steps");
                Plain::Token(step.2.clone())
            }
        }
    }

    /// Inverse of [`knob_to_plain`]; integer/enum values map to the middle of their step.
    pub fn plain_to_knob(&self, plain: &Plain) -> Option<f64> {
        let r = match (&self.transform, plain) {
            (Transform::Affine { a, b }, Plain::Number(y)) => (y - a) / b,
            (Transform::Power { a, b, k }, Plain::Number(y)) => ((y - a) / b).max(0.0).powf(1.0 / k),
            (Transform::Floor { a, n, hi }, Plain::Number(y)) => {
                if (y - hi).abs() < 1e-9 { 1.0 } else { ((y - a).round() + 0.5) / n }
            }
            (Transform::Toggle { low, high, .. }, Plain::Number(y)) => {
                if (y - high).abs() <= (y - low).abs() { 1.0 } else { 0.0 }
            }
            (Transform::Enum { steps }, Plain::Token(t)) => {
                let s = steps.iter().find(|s| &s.2 == t)?;
                (s.0 + s.1) / 2.0
            }
            _ => return None,
        };
        r.is_finite().then(|| r.clamp(0.0, 1.0))
    }

    /// Enum token for a display name or token, case-insensitive ("MG Low 12" or "MgL12").
    pub fn enum_token(&self, name: &str) -> Option<Plain> {
        let Transform::Enum { steps } = &self.transform else { return None };
        let n = name.trim().to_ascii_lowercase();
        steps
            .iter()
            .find(|s| s.3.to_ascii_lowercase() == n || s.2.as_deref().is_some_and(|t| t.to_ascii_lowercase() == n))
            .map(|s| Plain::Token(s.2.clone()))
    }

    /// Knob position for a displayed value like "425 Hz", "50 ms", "-6 dB", "75%".
    pub fn display_to_knob(&self, text: &str) -> Result<f64, String> {
        let (value, unit) = parse_display(text).ok_or_else(|| format!("cannot parse `{text}` as a number with unit"))?;
        let law = self
            .laws
            .iter()
            .find(|(u, _)| *u == unit)
            .map(|(_, l)| l)
            .ok_or_else(|| format!("{} does not display in {:?}; available: {:?}", self.path, unit, self.units()))?;
        Ok(law.invert(value).clamp(0.0, 1.0))
    }

    pub fn units(&self) -> Vec<Unit> {
        self.laws.iter().map(|(u, _)| *u).collect()
    }

    /// Human-readable value at a knob position, e.g. "425 Hz", "-12.0 dB", "MG Low 12".
    pub fn display(&self, r: f64) -> String {
        if let Transform::Enum { steps } = &self.transform {
            return steps.iter().find(|s| r < s.1).or(steps.last()).map(|s| s.3.clone()).unwrap_or_default();
        }
        if let Transform::Toggle { threshold, .. } = self.transform {
            return if r >= threshold { "On".into() } else { "Off".into() };
        }
        if let Transform::Floor { .. } = self.transform
            && let Plain::Number(n) = self.knob_to_plain(r)
        {
            return format!("{n}");
        }
        match self.laws.as_slice() {
            [] => match self.knob_to_plain(r) {
                Plain::Number(n) => format_value(n, Unit::Number),
                Plain::Token(t) => t.unwrap_or_default(),
            },
            laws => laws.iter().map(|(unit, law)| format_value(law.eval(r), *unit)).collect::<Vec<_>>().join(" / "),
        }
    }
}

fn format_value(v: f64, unit: Unit) -> String {
    match unit {
        Unit::Seconds if v.abs() < 1.0 => format!("{:.1} ms", v * 1000.0),
        Unit::Seconds => format!("{v:.2} s"),
        Unit::Hz if v >= 1000.0 => format!("{:.2} kHz", v / 1000.0),
        Unit::Hz => format!("{v:.0} Hz"),
        Unit::Decibels if v < -150.0 => "-inf dB".into(),
        Unit::Decibels | Unit::Degrees | Unit::Percent => format!("{v:.1}{}", unit.suffix()),
        Unit::Number if (v - v.round()).abs() < 1e-6 => format!("{v:.0}"),
        Unit::Number => format!("{v:.2}"),
    }
}

/// "425 Hz" -> (425, Hz); "1.2 kHz" -> (1200, Hz); "50ms" -> (0.05, s); "-6 dB"; "75%"; "90°".
pub fn parse_display(text: &str) -> Option<(f64, Unit)> {
    let t = text.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let v: f64 = num.trim().parse().ok()?;
    Some(match unit.trim().to_ascii_lowercase().as_str() {
        "hz" => (v, Unit::Hz),
        "khz" => (v * 1000.0, Unit::Hz),
        "s" | "sec" => (v, Unit::Seconds),
        "ms" => (v / 1000.0, Unit::Seconds),
        "db" => (v, Unit::Decibels),
        "%" => (v, Unit::Percent),
        "°" | "deg" => (v, Unit::Degrees),
        "" => (v, Unit::Number),
        _ => return None,
    })
}

/// Numbers in a display sample, by unit: "13% [-24.1 dB]" -> [(13, %), (-24.1, dB)].
fn sample_numbers(s: &str) -> Vec<(f64, Unit)> {
    let mut out = Vec::new();
    let s = s.replace(['[', ']'], " ");
    let tokens: Vec<&str> = s.split_whitespace().collect();
    for (i, tok) in tokens.iter().enumerate() {
        let next = tokens.get(i + 1).copied().unwrap_or("");
        let joined = if parse_display(tok).is_some_and(|(_, u)| u == Unit::Number) { format!("{tok} {next}") } else { tok.to_string() };
        if let Some(p) = parse_display(&joined).or_else(|| parse_display(tok)) {
            out.push(p);
        }
    }
    out
}

/// Params whose displayed value depends on loaded content rather than the knob alone
/// (wavetable position is shown as a frame number of the current table).
const STATE_DEPENDENT_DISPLAY: [&str; 1] = ["kParamTablePos"];

/// Display fits come from rounded UI strings, so exact laws pick up tiny intercepts
/// (e.g. reso "0.22..100.22"); snap those to zero.
fn snap(law: &Law) -> Law {
    let tiny = |a: f64, b: f64| a.abs() <= 0.005 * b.abs();
    match *law {
        Law::Affine { a, b } if tiny(a, b) => Law::Affine { a: 0.0, b },
        Law::Power { a, b, k } if tiny(a, b) => Law::Power { a: 0.0, b, k },
        ref l => l.clone(),
    }
}

fn infer_laws(p: &RawParam) -> Vec<(Unit, Law)> {
    let mut laws = Vec::new();
    if STATE_DEPENDENT_DISPLAY.iter().any(|n| p.path.ends_with(&format!(".{n}"))) {
        return laws;
    }
    let has = |needle: &str| p.display.samples.iter().any(|(_, s)| s.contains(needle));
    if let Some(fit) = p.display.fit.as_ref().map(snap) {
        let unit = if p.host.ends_with("_hz") || matches!(fit, Law::Exp { .. }) {
            Unit::Hz
        } else if p.display.unit == "s" || has("ms") {
            Unit::Seconds
        } else if p.display.unit == "%" || has("%") {
            Unit::Percent
        } else if has("°") {
            Unit::Degrees
        } else {
            Unit::Number
        };
        laws.push((unit, fit));
    }
    if has("dB") {
        // Least-squares fit of dB = c*log10(r) + d over finite samples.
        let pts: Vec<(f64, f64)> = p
            .display
            .samples
            .iter()
            .filter(|(r, _)| *r > 0.0)
            .filter_map(|(r, s)| sample_numbers(s).into_iter().find(|(_, u)| *u == Unit::Decibels).map(|(v, _)| (r.log10(), v)))
            .collect();
        if pts.len() >= 3 {
            let n = pts.len() as f64;
            let (sx, sy) = pts.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
            let (mx, my) = (sx / n, sy / n);
            let (sxy, sxx) = pts.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + (x - mx) * (y - my), b + (x - mx).powi(2)));
            if sxx > 0.0 {
                let c = sxy / sxx;
                laws.push((Unit::Decibels, Law::Log { c, d: my - c * mx }));
            }
        }
    }
    laws
}

pub struct UnitTable {
    by_path: HashMap<String, ParamUnits>,
    /// (module type, param) -> a representative path, for per-type descriptions.
    by_type: HashMap<(String, String), String>,
}

impl UnitTable {
    pub fn get(&self, path: &str) -> Option<&ParamUnits> {
        self.by_path.get(path)
    }

    /// Units of `param` on any instance of `module_type` (instances share laws).
    pub fn for_type(&self, module_type: &str, param: &str) -> Option<&ParamUnits> {
        self.by_type.get(&(module_type.to_owned(), param.to_owned())).and_then(|p| self.by_path.get(p))
    }

    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }
}

static UNITS: OnceLock<UnitTable> = OnceLock::new();

/// Units for the params Serum exposes to hosts (455 of 541; FX and a few osc params are not).
pub fn units() -> &'static UnitTable {
    UNITS.get_or_init(|| {
        let raw: RawFile =
            serde_json::from_str(include_str!("../data/serum2_units.json")).expect("embedded serum2_units.json is valid");
        let by_path: HashMap<String, ParamUnits> = raw
            .params
            .into_iter()
            .map(|p| {
                let laws = infer_laws(&p);
                let units = ParamUnits {
                    host: p.host,
                    path: p.path.clone(),
                    default_knob: p.default_raw,
                    default_plain: p.default_plain,
                    transform: p.transform,
                    laws,
                    samples: p.display.samples,
                };
                (p.path, units)
            })
            .collect();
        let mut by_type = HashMap::new();
        let mut paths: Vec<&String> = by_path.keys().collect();
        paths.sort(); // deterministic representative: lowest instance
        for path in paths {
            if let Some((module, param)) = path.rsplit_once('.') {
                let last = module.rsplit('/').next().unwrap_or(module);
                let ty = last.trim_end_matches(|c: char| c.is_ascii_digit()).to_owned();
                by_type.entry((ty, param.to_owned())).or_insert_with(|| path.clone());
            }
        }
        UnitTable { by_path, by_type }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol * b.abs().max(1.0)
    }

    #[test]
    fn filter_cutoff_hz_roundtrip() {
        let p = units().get("VoiceFilter0.kParamFreq").unwrap();
        let r = p.display_to_knob("425 Hz").unwrap();
        assert!(close(r, 0.5, 0.01), "{r}");
        assert_eq!(p.knob_to_plain(r), Plain::Number(r));
        assert!(p.display(0.5).contains("Hz"));
    }

    #[test]
    fn envelope_attack_ms() {
        let p = units().get("Env0.kParamAttack").unwrap();
        let r = p.display_to_knob("1 s").unwrap();
        assert!(close(r, 0.5, 0.01), "{r}");
        let Plain::Number(secs) = p.knob_to_plain(p.display_to_knob("50 ms").unwrap()) else { panic!() };
        assert!(close(secs, 0.05, 0.02), "{secs}");
    }

    #[test]
    fn level_db_matches_serum_samples() {
        let p = units().get("Oscillator0.kParamVolume").unwrap();
        // Serum shows 50% knob as -12.0 dB.
        let r = p.display_to_knob("-12 dB").unwrap();
        assert!(close(r, 0.5, 0.01), "{r}");
        assert_eq!(p.knob_to_plain(0.5), Plain::Number(0.25));
    }

    #[test]
    fn enums_accept_display_names() {
        let p = units().get("VoiceFilter0.kParamType").unwrap();
        assert_eq!(p.enum_token("MG Low 12"), Some(Plain::Token(Some("MgL12".into()))));
        let r = p.plain_to_knob(&Plain::Token(Some("MgL12".into()))).unwrap();
        assert_eq!(p.display(r), "MG Low 12");
    }

    #[test]
    fn integer_params_round_trip() {
        let p = units().get("Oscillator0.kParamUnison").unwrap();
        for n in 1..=16 {
            let r = p.plain_to_knob(&Plain::Number(n as f64)).unwrap();
            assert_eq!(p.knob_to_plain(r), Plain::Number(n as f64), "unison {n}");
        }
    }

    #[test]
    fn every_param_plain_knob_round_trips_at_samples() {
        for p in units().by_path.values() {
            for i in 0..=8 {
                let r = i as f64 / 8.0;
                let plain = p.knob_to_plain(r);
                let back = p.plain_to_knob(&plain).unwrap_or_else(|| panic!("{} has no inverse", p.path));
                match (p.knob_to_plain(back), &plain) {
                    (Plain::Number(x), Plain::Number(y)) => assert!(close(x, *y, 1e-9), "{} at {r}: {x} vs {y}", p.path),
                    (got, want) => assert_eq!(&got, want, "{} at {r}", p.path),
                }
            }
        }
    }

    #[test]
    fn displays_match_serum_ui() {
        let reso = units().get("VoiceFilter0.kParamReso").unwrap();
        assert_eq!(reso.display(0.4), "40");
        let level = units().get("Oscillator0.kParamVolume").unwrap();
        assert_eq!(level.display(0.5), "50.0% / -12.0 dB");
        let attack = units().get("Env0.kParamAttack").unwrap();
        assert_eq!(attack.display(attack.default_knob), "1.0 ms");
        let pos = units().get("Oscillator0/WTOsc0.kParamTablePos").unwrap();
        assert_eq!(pos.display(0.0), "1"); // shown as stored value, not a frame number
    }

    #[test]
    fn parses_display_strings() {
        assert_eq!(parse_display("1.2 kHz"), Some((1200.0, Unit::Hz)));
        assert_eq!(parse_display("50ms"), Some((0.05, Unit::Seconds)));
        assert_eq!(parse_display("-6 dB"), Some((-6.0, Unit::Decibels)));
        assert_eq!(parse_display("loud"), None);
    }
}
