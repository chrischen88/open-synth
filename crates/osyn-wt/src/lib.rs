//! Wavetables in Serum's layout: frames of [`FRAME_SIZE`] samples, up to [`MAX_FRAMES`].
//!
//! - [`wav`]: read WAV files (PCM 16/24/32, float32, extensible; `clm ` chunk frame size).
//! - [`generate`]: basic shapes, additive (harmonic) spectra and formula-generated tables.
//! - [`expr`]: the small expression language used by formulas (`sin(x*pi) * (1-y)`).

pub mod expr;
pub mod generate;
pub mod wav;

pub const FRAME_SIZE: usize = 2048;
pub const MAX_FRAMES: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("wav: {0}")]
    Wav(String),
    #[error("formula: {0}")]
    Expr(String),
    #[error("{0}")]
    Invalid(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// How Serum builds in-between frames after loading (`interpolateAfterLoad`, first digit of
/// the `clm ` flags). Only the keyframes are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Morph {
    #[default]
    None = 0,
    Crossfade = 1,
    Spectral = 2,
    SpectralZeroFundamentalPhase = 3,
    SpectralZeroAllPhases = 4,
}

impl Morph {
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().replace([' ', '-', '_'], "").as_str() {
            "none" | "off" => Morph::None,
            "crossfade" | "xfade" => Morph::Crossfade,
            "spectral" => Morph::Spectral,
            "spectralzerofundamentalphase" | "spectralzerofund" => Morph::SpectralZeroFundamentalPhase,
            "spectralzeroallphases" | "spectralzeroall" => Morph::SpectralZeroAllPhases,
            _ => return None,
        })
    }
}

/// A wavetable: `frames.len() * FRAME_SIZE` samples, frame-major.
#[derive(Debug, Clone, PartialEq)]
pub struct Wavetable {
    pub samples: Vec<f32>,
}

impl Wavetable {
    pub fn new(samples: Vec<f32>) -> Result<Self, Error> {
        let frames = samples.len() / FRAME_SIZE;
        if samples.is_empty() || !samples.len().is_multiple_of(FRAME_SIZE) {
            return Err(Error::Invalid(format!("{} samples is not a whole number of {FRAME_SIZE}-sample frames", samples.len())));
        }
        if frames > MAX_FRAMES {
            return Err(Error::Invalid(format!("{frames} frames exceeds the maximum of {MAX_FRAMES}")));
        }
        Ok(Wavetable { samples })
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / FRAME_SIZE
    }

    pub fn frame(&self, i: usize) -> &[f32] {
        &self.samples[i * FRAME_SIZE..(i + 1) * FRAME_SIZE]
    }

    /// Remove each frame's DC offset, then scale the whole table so its peak is `peak`.
    pub fn normalize(&mut self, peak: f32) {
        for frame in self.samples.chunks_mut(FRAME_SIZE) {
            let mean = frame.iter().sum::<f32>() / FRAME_SIZE as f32;
            frame.iter_mut().for_each(|s| *s -= mean);
        }
        let max = self.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        if max > 0.0 {
            let k = peak / max;
            self.samples.iter_mut().for_each(|s| *s *= k);
        }
    }
}

/// Resample one cycle to `FRAME_SIZE` samples (linear interpolation, periodic).
pub(crate) fn resample_cycle(src: &[f32]) -> Vec<f32> {
    if src.len() == FRAME_SIZE {
        return src.to_vec();
    }
    let n = src.len();
    (0..FRAME_SIZE)
        .map(|i| {
            let pos = i as f64 * n as f64 / FRAME_SIZE as f64;
            let i0 = pos.floor() as usize % n;
            let frac = (pos - pos.floor()) as f32;
            src[i0] * (1.0 - frac) + src[(i0 + 1) % n] * frac
        })
        .collect()
}
