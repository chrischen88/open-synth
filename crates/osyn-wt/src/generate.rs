//! Wavetable generators. Shapes and harmonic spectra are band-limited by construction
//! (additive synthesis up to the Nyquist of a 2048-sample cycle).

use std::f64::consts::TAU;

use crate::expr::Expr;
use crate::{Error, FRAME_SIZE, MAX_FRAMES, Wavetable};

const MAX_HARMONIC: usize = FRAME_SIZE / 2 - 1;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    Sine,
    Saw,
    Square,
    Triangle,
    /// Pulse with duty cycle 0..1.
    Pulse(f64),
}

impl Shape {
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "sine" | "sin" => Shape::Sine,
            "saw" | "sawtooth" => Shape::Saw,
            "square" => Shape::Square,
            "triangle" | "tri" => Shape::Triangle,
            other => {
                let duty = other.strip_prefix("pulse")?.trim_start_matches([':', ' ']);
                Shape::Pulse(if duty.is_empty() { 0.25 } else { duty.parse::<f64>().ok()?.clamp(0.01, 0.99) })
            }
        })
    }

    /// Harmonic amplitudes and phases (sine-phase) for harmonics 1..=MAX_HARMONIC.
    fn partials(self) -> Vec<(f64, f64)> {
        (1..=MAX_HARMONIC)
            .map(|k| {
                let kf = k as f64;
                match self {
                    Shape::Sine => (if k == 1 { 1.0 } else { 0.0 }, 0.0),
                    Shape::Saw => (1.0 / kf, 0.0),
                    Shape::Square => (if k % 2 == 1 { 1.0 / kf } else { 0.0 }, 0.0),
                    Shape::Triangle => {
                        let a = if k % 2 == 1 { 1.0 / (kf * kf) } else { 0.0 };
                        // alternate sign via phase so the peaks line up
                        (a, if (k / 2) % 2 == 1 { std::f64::consts::PI } else { 0.0 })
                    }
                    Shape::Pulse(d) => {
                        // Fourier series of a pulse: (2/kπ) sin(kπd), cosine-phase
                        ((kf * std::f64::consts::PI * d).sin() / kf, std::f64::consts::FRAC_PI_2)
                    }
                }
            })
            .collect()
    }
}

/// One normalized frame of a basic shape.
pub fn shape(s: Shape) -> Vec<f32> {
    let mut t = Wavetable { samples: additive(&s.partials()) };
    t.normalize(1.0);
    t.samples
}

fn additive(partials: &[(f64, f64)]) -> Vec<f32> {
    let mut out = vec![0f64; FRAME_SIZE];
    for (k, &(amp, phase)) in partials.iter().enumerate().take(MAX_HARMONIC) {
        if amp == 0.0 {
            continue;
        }
        let h = (k + 1) as f64;
        for (i, o) in out.iter_mut().enumerate() {
            *o += amp * (TAU * h * i as f64 / FRAME_SIZE as f64 + phase).sin();
        }
    }
    out.into_iter().map(|v| v as f32).collect()
}

fn check_frames(frames: usize) -> Result<(), Error> {
    if (1..=MAX_FRAMES).contains(&frames) {
        Ok(())
    } else {
        Err(Error::Invalid(format!("frames must be 1..={MAX_FRAMES}, got {frames}")))
    }
}

/// Crossfade between keyframe shapes, spreading `frames` frames evenly across them.
pub fn morph_shapes(keys: &[Shape], frames: usize) -> Result<Wavetable, Error> {
    check_frames(frames)?;
    if keys.is_empty() {
        return Err(Error::Invalid("need at least one shape".into()));
    }
    let partials: Vec<Vec<(f64, f64)>> = keys.iter().map(|s| s.partials()).collect();
    // Interpolating in the waveform domain keeps it simple and phase-coherent.
    let waves: Vec<Vec<f32>> = partials.iter().map(|p| additive(p)).collect();
    let mut samples = Vec::with_capacity(frames * FRAME_SIZE);
    for f in 0..frames {
        let (a, b, t) = key_position(f, frames, waves.len());
        samples.extend(waves[a].iter().zip(&waves[b]).map(|(x, y)| x * (1.0 - t) + y * t));
    }
    let mut t = Wavetable::new(samples)?;
    t.normalize(1.0);
    Ok(t)
}

/// Additive table from keyframe spectra: `keys[k][h]` = amplitude of harmonic h+1 (sine phase).
/// Amplitudes are interpolated linearly between keyframes.
pub fn harmonics(keys: &[Vec<f64>], frames: usize) -> Result<Wavetable, Error> {
    check_frames(frames)?;
    if keys.is_empty() || keys.iter().all(|k| k.iter().all(|a| *a == 0.0)) {
        return Err(Error::Invalid("need at least one non-silent harmonic keyframe".into()));
    }
    let width = keys.iter().map(Vec::len).max().unwrap_or(0).min(MAX_HARMONIC);
    let mut samples = Vec::with_capacity(frames * FRAME_SIZE);
    for f in 0..frames {
        let (a, b, t) = key_position(f, frames, keys.len());
        let partials: Vec<(f64, f64)> = (0..width)
            .map(|h| {
                let get = |k: &Vec<f64>| k.get(h).copied().unwrap_or(0.0);
                (get(&keys[a]) * (1.0 - t as f64) + get(&keys[b]) * t as f64, 0.0)
            })
            .collect();
        samples.extend(additive(&partials));
    }
    let mut t = Wavetable::new(samples)?;
    t.normalize(1.0);
    Ok(t)
}

/// Evaluate a formula per sample: `x` = -1..1 phase, `y` = 0..1 table position.
/// The result is DC-corrected and normalized. Formulas can alias (they are not band-limited).
pub fn formula(src: &str, frames: usize) -> Result<Wavetable, Error> {
    check_frames(frames)?;
    let expr = Expr::parse(src)?;
    let mut samples = Vec::with_capacity(frames * FRAME_SIZE);
    for f in 0..frames {
        let y = if frames == 1 { 0.0 } else { f as f64 / (frames - 1) as f64 };
        for i in 0..FRAME_SIZE {
            let w = i as f64 / FRAME_SIZE as f64;
            let v = expr.eval(&[w * 2.0 - 1.0, y, w, f as f64]);
            samples.push(if v.is_finite() { v as f32 } else { 0.0 });
        }
    }
    if samples.iter().all(|s| *s == 0.0) {
        return Err(Error::Invalid("formula produced silence everywhere".into()));
    }
    let mut t = Wavetable::new(samples)?;
    t.normalize(1.0);
    Ok(t)
}

/// For frame `f` of `frames`, the surrounding keyframes and the blend between them.
fn key_position(f: usize, frames: usize, keys: usize) -> (usize, usize, f32) {
    if keys == 1 || frames == 1 {
        return (0, 0, 0.0);
    }
    let pos = f as f64 / (frames - 1) as f64 * (keys - 1) as f64;
    let a = (pos.floor() as usize).min(keys - 2);
    (a, a + 1, (pos - a as f64) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn shapes_are_normalized_and_distinct() {
        let sine = shape(Shape::Sine);
        let square = shape(Shape::Square);
        assert!((sine.iter().fold(0f32, |m, s| m.max(s.abs())) - 1.0).abs() < 1e-6);
        assert!((rms(&sine) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
        assert!(rms(&square) > 0.8, "band-limited square (with Gibbs overshoot) is mostly full-scale");
        assert_eq!(Shape::from_name("pulse:0.1"), Some(Shape::Pulse(0.1)));
    }

    #[test]
    fn morph_endpoints_match_shapes() {
        let t = morph_shapes(&[Shape::Sine, Shape::Saw], 16).unwrap();
        assert_eq!(t.frames(), 16);
        let first = t.frame(0);
        let sine = shape(Shape::Sine);
        // same shape up to the table-wide normalization
        let corr: f32 = first.iter().zip(&sine).map(|(a, b)| a * b).sum::<f32>() / (rms(first) * rms(&sine) * FRAME_SIZE as f32);
        assert!(corr > 0.999, "{corr}");
    }

    #[test]
    fn formula_and_harmonics() {
        let t = formula("sin(x*pi) * (1-y) + saw(w) * y", 8).unwrap();
        assert_eq!(t.frames(), 8);
        assert!(formula("0", 1).is_err());
        assert!(formula("sin(", 1).is_err());
        let h = harmonics(&[vec![1.0], vec![1.0, 0.5, 0.33]], 4).unwrap();
        assert_eq!(h.frames(), 4);
        assert!(harmonics(&[vec![0.0]], 4).is_err());
        assert!(harmonics(&[vec![1.0]], 0).is_err());
    }
}
