//! WAV reading with Serum's `clm ` chunk (`"<!>2048 01000000 ..."`: frame size, then flags
//! whose first digit is the morph mode).

use crate::{Error, FRAME_SIZE, MAX_FRAMES, Morph, Wavetable, resample_cycle};

#[derive(Debug, Clone)]
pub struct WavInfo {
    pub channels: u16,
    pub sample_rate: u32,
    /// Sample frames in the file (per channel), as Serum records in `numFrames`.
    pub length: usize,
    /// Cycle length from the `clm ` chunk, if present.
    pub clm_frame_size: Option<usize>,
    pub clm_morph: Option<Morph>,
}

/// Decode the first channel of a WAV file to f32, plus header info.
pub fn decode(bytes: &[u8]) -> Result<(Vec<f32>, WavInfo), Error> {
    let (mut channels, info) = decode_all(bytes)?;
    Ok((channels.swap_remove(0), info))
}

/// Decode every channel of a WAV file to f32 (channel-major), plus header info.
pub fn decode_all(bytes: &[u8]) -> Result<(Vec<Vec<f32>>, WavInfo), Error> {
    let bad = |m: &str| Error::Wav(m.to_owned());
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("not a RIFF/WAVE file"));
    }
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (format, channels, rate, bits)
    let mut data: Option<&[u8]> = None;
    let (mut clm_frame_size, mut clm_morph) = (None, None);
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = bytes.get(pos + 8..pos + 8 + len).unwrap_or(&bytes[pos + 8..]);
        match id {
            b"fmt " if body.len() >= 16 => {
                let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
                let mut format = u16_at(0);
                if format == 0xFFFE && body.len() >= 26 {
                    format = u16_at(24); // WAVE_FORMAT_EXTENSIBLE: sub-format GUID starts with the real tag
                }
                fmt = Some((format, u16_at(2), u32::from_le_bytes(body[4..8].try_into().unwrap()), u16_at(14)));
            }
            b"data" => data = Some(body),
            b"clm " => {
                let text = String::from_utf8_lossy(body);
                let mut parts = text.trim_start_matches("<!>").split_whitespace();
                clm_frame_size = parts.next().and_then(|n| n.parse().ok());
                clm_morph = parts.next().and_then(|flags| flags.chars().next()).and_then(|c| match c {
                    '0' => Some(Morph::None),
                    '1' => Some(Morph::Crossfade),
                    '2' => Some(Morph::Spectral),
                    '3' => Some(Morph::SpectralZeroFundamentalPhase),
                    '4' => Some(Morph::SpectralZeroAllPhases),
                    _ => None,
                });
            }
            _ => {}
        }
        pos += 8 + len + (len & 1);
    }
    let (format, channels, sample_rate, bits) = fmt.ok_or_else(|| bad("missing fmt chunk"))?;
    let data = data.ok_or_else(|| bad("missing data chunk"))?;
    if channels == 0 {
        return Err(bad("zero channels"));
    }
    let width = (bits as usize).div_ceil(8);
    let stride = width * channels as usize;
    type Reader = Box<dyn Fn(&[u8]) -> f32>;
    let read: Reader = match (format, bits) {
        (3, 32) => Box::new(|b| f32::from_le_bytes(b[..4].try_into().unwrap())),
        (1, 16) => Box::new(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0),
        (1, 24) => Box::new(|b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0),
        (1, 32) => Box::new(|b| i32::from_le_bytes(b[..4].try_into().unwrap()) as f32 / 2_147_483_648.0),
        (1, 8) => Box::new(|b| (b[0] as f32 - 128.0) / 128.0),
        _ => return Err(bad(&format!("unsupported sample format {format}/{bits}-bit"))),
    };
    let length = data.len() / stride;
    let out: Vec<Vec<f32>> = (0..channels as usize)
        .map(|c| data.chunks_exact(stride).map(|frame| read(&frame[c * width..(c + 1) * width])).collect())
        .collect();
    Ok((out, WavInfo { channels, sample_rate, length, clm_frame_size, clm_morph }))
}

/// Header facts Serum records for a referenced audio file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioInfo {
    pub channels: u16,
    pub sample_rate: u32,
    /// Sample frames per channel.
    pub length: u64,
}

/// Read channels / rate / length from a WAV or FLAC file's header without decoding audio.
pub fn audio_info(bytes: &[u8]) -> Result<AudioInfo, Error> {
    if bytes.starts_with(b"fLaC") {
        // First metadata block must be STREAMINFO (type 0, 34 bytes).
        let si = bytes.get(8..8 + 34).filter(|_| bytes[4] & 0x7F == 0).ok_or_else(|| Error::Wav("bad FLAC STREAMINFO".into()))?;
        let rate = (u32::from(si[10]) << 12) | (u32::from(si[11]) << 4) | (u32::from(si[12]) >> 4);
        let channels = u16::from((si[12] >> 1) & 0x7) + 1;
        let total = (u64::from(si[13] & 0x0F) << 32) | u64::from(u32::from_be_bytes(si[14..18].try_into().unwrap()));
        return Ok(AudioInfo { channels, sample_rate: rate, length: total });
    }
    let (_, info) = decode(bytes)?;
    Ok(AudioInfo { channels: info.channels, sample_rate: info.sample_rate, length: info.length as u64 })
}

/// Read a wavetable. The cycle length comes from `frame_size`, else the `clm ` chunk, else
/// 2048 when the length divides evenly. Cycles of other lengths are resampled to 2048.
pub fn read_wavetable(bytes: &[u8], frame_size: Option<usize>) -> Result<(Wavetable, WavInfo), Error> {
    let (samples, info) = decode(bytes)?;
    let cycle = frame_size.or(info.clm_frame_size).unwrap_or(FRAME_SIZE);
    if !(32..=9999).contains(&cycle) {
        return Err(Error::Wav(format!("cycle length {cycle} outside 32..9999")));
    }
    if samples.len() < cycle {
        return Err(Error::Wav(format!("{} samples is shorter than one {cycle}-sample cycle", samples.len())));
    }
    if frame_size.is_none() && info.clm_frame_size.is_none() && samples.len() % FRAME_SIZE != 0 {
        return Err(Error::Wav(format!(
            "no clm chunk and {} samples is not a multiple of 2048; pass the cycle length (frame_size)",
            samples.len()
        )));
    }
    let frames = (samples.len() / cycle).min(MAX_FRAMES);
    let out: Vec<f32> = (0..frames).flat_map(|f| resample_cycle(&samples[f * cycle..(f + 1) * cycle])).collect();
    Ok((Wavetable::new(out)?, info))
}

/// Write a 32-bit float mono WAV with a `clm ` chunk Serum recognizes.
pub fn encode(table: &Wavetable, morph: Morph) -> Vec<u8> {
    let clm = format!("<!>{FRAME_SIZE} {}0000000 wavetable (open-synth)", morph as u8);
    let mut clm_bytes = clm.into_bytes();
    if clm_bytes.len() % 2 == 1 {
        clm_bytes.push(0);
    }
    let data_len = table.samples.len() * 4;
    let mut out = Vec::with_capacity(44 + clm_bytes.len() + 8 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((4 + 24 + 8 + clm_bytes.len() + 8 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&44_100u32.to_le_bytes());
    out.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(b"clm ");
    out.extend_from_slice(&(clm_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&clm_bytes);
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    table.samples.iter().for_each(|s| out.extend_from_slice(&s.to_le_bytes()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::{Shape, shape};

    #[test]
    fn encode_decode_roundtrip() {
        let t = Wavetable::new(shape(Shape::Saw).into_iter().chain(shape(Shape::Square)).collect()).unwrap();
        let bytes = encode(&t, Morph::Spectral);
        let (back, info) = read_wavetable(&bytes, None).unwrap();
        assert_eq!(back, t);
        assert_eq!(info.clm_frame_size, Some(2048));
        assert_eq!(info.clm_morph, Some(Morph::Spectral));
    }

    #[test]
    fn requires_frame_size_without_clm() {
        // 16-bit PCM, 1000 samples, no clm chunk.
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36u32 + 2000).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&[1, 0, 1, 0]);
        b.extend_from_slice(&44_100u32.to_le_bytes());
        b.extend_from_slice(&88_200u32.to_le_bytes());
        b.extend_from_slice(&[2, 0, 16, 0]);
        b.extend_from_slice(b"data");
        b.extend_from_slice(&2000u32.to_le_bytes());
        b.extend(std::iter::repeat_n(0u8, 2000));
        assert!(read_wavetable(&b, None).is_err());
        let (t, _) = read_wavetable(&b, Some(500)).unwrap();
        assert_eq!(t.frames(), 2);
    }

    #[test]
    fn decodes_all_channels_and_flac_headers() {
        // Stereo 16-bit: L = +0.5, R = -0.5 for 4 frames.
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36u32 + 16).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&[1, 0, 2, 0]);
        b.extend_from_slice(&48_000u32.to_le_bytes());
        b.extend_from_slice(&192_000u32.to_le_bytes());
        b.extend_from_slice(&[4, 0, 16, 0]);
        b.extend_from_slice(b"data");
        b.extend_from_slice(&16u32.to_le_bytes());
        for _ in 0..4 {
            b.extend_from_slice(&16384i16.to_le_bytes());
            b.extend_from_slice(&(-16384i16).to_le_bytes());
        }
        let (ch, info) = decode_all(&b).unwrap();
        assert_eq!((ch.len(), info.length, info.sample_rate), (2, 4, 48_000));
        assert_eq!((ch[0][0], ch[1][3]), (0.5, -0.5));
        assert_eq!(audio_info(&b).unwrap(), AudioInfo { channels: 2, sample_rate: 48_000, length: 4 });

        let dir = std::path::Path::new("/Library/Audio/Presets/Xfer Records/Serum 2 Presets/Multisamples");
        if let Some(flac) = find_ext(dir, "flac") {
            let info = audio_info(&std::fs::read(&flac).unwrap()).unwrap();
            assert!((1..=2).contains(&info.channels) && [44_100, 48_000, 96_000].contains(&info.sample_rate) && info.length > 0, "{info:?}");
        }
    }

    fn find_ext(dir: &std::path::Path, ext: &str) -> Option<std::path::PathBuf> {
        for e in std::fs::read_dir(dir).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(f) = find_ext(&p, ext) {
                    return Some(f);
                }
            } else if p.extension().is_some_and(|x| x == ext) {
                return Some(p);
            }
        }
        None
    }

    /// Every table in the local Serum library parses (skipped when not installed).
    #[test]
    fn reads_factory_tables() {
        let dir = std::path::Path::new("/Library/Audio/Presets/Xfer Records/Serum 2 Presets/Tables");
        if !dir.is_dir() {
            return;
        }
        let mut stack = vec![dir.to_path_buf()];
        let (mut ok, mut failed) = (0, Vec::new());
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav")) {
                    match read_wavetable(&std::fs::read(&p).unwrap(), None) {
                        Ok(_) => ok += 1,
                        Err(e) => failed.push(format!("{}: {e}", p.display())),
                    }
                }
            }
        }
        eprintln!("{ok} tables read, {} failed", failed.len());
        // Tables without a clm chunk and a non-2048 length need an explicit frame size.
        assert!(failed.iter().all(|f| f.contains("pass the cycle length")), "{failed:#?}");
    }
}
