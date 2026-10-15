//! A 16-bit PCM mono WAV writer for the ignored listening renders, so no dependency: `samples` times
//! `gain`, clamped to ±1, rounded to the nearest of ±32767.

use std::path::Path;

/// Write `samples * gain` to `path` at `sr`. Returns how many samples the clamp touched.
pub fn write(path: &Path, sr: u32, samples: &[f32], gain: f32) -> std::io::Result<usize> {
    let data = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&sr.to_le_bytes());
    bytes.extend_from_slice(&(sr * 2).to_le_bytes()); // bytes per second
    bytes.extend_from_slice(&2u16.to_le_bytes()); // bytes per frame
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    let mut clipped = 0;
    for &s in samples {
        let v = s * gain;
        clipped += usize::from(v.abs() > 1.0);
        bytes.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
    }
    std::fs::write(path, bytes)?;
    Ok(clipped)
}
