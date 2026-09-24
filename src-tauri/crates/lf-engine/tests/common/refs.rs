//! The Tone reference fixtures (`tests/fixtures/tone`, written by `verify/probes/tone-refs.mjs --write`)
//! and the tolerance classes a Stage 3 port is judged by (docs/plans/native-engine.md § Stage 3).
//!
//! A port test replays a scenario's setup and events from the manifest, renders the same number of
//! frames and channels, and calls [`assert_class`] with the tightest class it passes. A failure writes
//! the Rust render to `tests/fixtures/tone/out/<id>.rust.wav` (untracked) for an ear A/B.
//!
//! Seeded inputs are regenerated here bit for bit (their sha256 is in the manifest); the noise tables
//! are the port's own job (`tables` in the manifest; check them with [`assert_sha256`]).

use std::path::PathBuf;
use std::sync::OnceLock;

pub use lf_engine::dsp::rng::Mulberry32;
use rustfft::{num_complex::Complex, FftPlanner};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub capture: Capture,
    pub tables: Tables,
    pub scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
pub struct Capture {
    pub commit: String,
    pub tone: String,
    pub chromium: String,
}

#[derive(Debug, Deserialize)]
pub struct Tables {
    pub prng: String,
    pub seed: u32,
    /// Generation order: each table draws `channels × length` values, then its Noise's start offset
    /// draws one more before the next table begins.
    pub order: Vec<String>,
    pub length: usize,
    pub channels: usize,
    /// The sample rate Tone stamped on the tables (the rate of the context that generated them).
    pub rate: u32,
    /// sha256 of the tables' f32 little-endian bytes, channel 0 then channel 1.
    pub sha256: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scenario {
    pub id: String,
    /// "synth" | "fx" | "limiter" | "ir"
    pub group: String,
    pub rate: u32,
    pub frames: usize,
    /// Stored channels: 1 when the render's two channels were identical.
    pub channels: usize,
    /// Group-specific: synth `{synth}`; fx `{states, timing, input: {active}, reverb?}`; limiter
    /// `{input: {ramp}}`; ir `{decay, preDelay, normalize}`.
    pub setup: serde_json::Value,
    pub events: Vec<ScriptEvent>,
    /// Every `Math.random` draw the scenario made, in order (noise start offsets, IR generation).
    pub random: Vec<f64>,
    pub input_sha256: Option<String>,
    pub file: String,
    pub peak: f64,
}

/// A frame-stamped event. Synth: `on` (note, velocity 0..1), `off` (note), `bend` (value, semitones),
/// `mod` (value, 0..1). FX: `param` (fx index, key, value), `bypass` (fx index, value as bool).
/// Control changes (bend, mod, param, bypass) sit on 128-frame boundaries: Tone's offline clock ticks.
#[derive(Debug, Clone, Deserialize)]
pub struct ScriptEvent {
    pub frame: u64,
    pub kind: String,
    pub note: Option<u8>,
    pub velocity: Option<f64>,
    pub value: Option<serde_json::Value>,
    pub fx: Option<usize>,
    pub key: Option<String>,
}

pub fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone")
}

pub fn manifest() -> &'static Manifest {
    static M: OnceLock<Manifest> = OnceLock::new();
    M.get_or_init(|| {
        let text = std::fs::read_to_string(dir().join("manifest.json")).expect("read the fixture manifest");
        serde_json::from_str(&text).expect("parse the fixture manifest")
    })
}

pub fn scenario(id: &str) -> &'static Scenario {
    manifest().scenarios.iter().find(|s| s.id == id).unwrap_or_else(|| panic!("no scenario {id}"))
}

/// The reference render, one Vec per stored channel.
pub fn reference(id: &str) -> Vec<Vec<f32>> {
    let s = scenario(id);
    let bytes = std::fs::read(dir().join(&s.file)).expect("read the fixture");
    // encodeWav's float32 layout: a 58-byte header (fmt with cbSize 0, fact), then interleaved f32 LE.
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(u16::from_le_bytes([bytes[20], bytes[21]]), 3, "{id}: not IEEE float");
    let channels = u16::from_le_bytes([bytes[22], bytes[23]]) as usize;
    assert_eq!(channels, s.channels);
    assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), s.rate);
    let data = &bytes[58..];
    let frames = data.len() / 4 / channels;
    assert_eq!(frames, s.frames, "{id}: frame count");
    let mut out = vec![Vec::with_capacity(frames); channels];
    for (k, chunk) in data.chunks_exact(4).enumerate() {
        out[k % channels].push(f32::from_le_bytes(chunk.try_into().unwrap()));
    }
    out
}

pub fn sha256_f32(channels: &[&[f32]]) -> String {
    let mut h = Sha256::new();
    for c in channels {
        for v in c.iter() {
            h.update(v.to_le_bytes());
        }
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn assert_sha256(what: &str, channels: &[&[f32]], expected: &str) {
    assert_eq!(sha256_f32(channels), expected, "{what} is not the probe's bit-exact data");
}

/// The FX scenarios' input (tone-refs.mjs `fxInput`): a 110→440 Hz sawtooth chirp plus a seeded
/// noise burst every quarter second, silent after `active` seconds. Checked against the manifest.
pub fn fx_input(s: &Scenario) -> Vec<f32> {
    let rate = s.rate as f64;
    let frames = s.frames;
    let active = s.setup["input"]["active"].as_f64().expect("fx input.active");
    let mut noise = Mulberry32::new(7);
    let quarter = (rate / 4.0).round() as usize;
    let on = (active * rate).round() as usize;
    let mut phase = 0.0f64;
    let mut x = vec![0.0f32; frames];
    for (n, out) in x.iter_mut().enumerate() {
        phase += (110.0 * (1.0 + (3.0 * n as f64) / frames as f64)) / rate;
        phase -= phase.floor();
        let w = noise.next_f64() * 2.0 - 1.0;
        let burst = if ((n % quarter) as f64) < quarter as f64 / 10.0 { 1.0 } else { 0.0 };
        *out = if n < on { (0.4 * (2.0 * phase - 1.0) + 0.3 * w * burst) as f32 } else { 0.0 };
    }
    assert_sha256(&format!("{} input", s.id), &[&x], s.input_sha256.as_deref().expect("input sha"));
    x
}

/// The limiter scenarios' stereo input (tone-refs.mjs `limiterInput`): 220 / 330 Hz triangles, an
/// amplitude ramp from 0.1 to 4.0 over `ramp` seconds, then 50 ms bursts at 3.0 with 150 ms gaps.
pub fn limiter_input(s: &Scenario) -> [Vec<f32>; 2] {
    let rate = s.rate as f64;
    let frames = s.frames;
    let ramp = (s.setup["input"]["ramp"].as_f64().expect("limiter input.ramp") * rate).round() as usize;
    let burst = (0.05 * rate).round() as usize;
    let gap = (0.15 * rate).round() as usize;
    let (mut pl, mut pr) = (0.0f64, 0.0f64);
    let mut l = vec![0.0f32; frames];
    let mut r = vec![0.0f32; frames];
    for n in 0..frames {
        pl += 220.0 / rate;
        pl -= pl.floor();
        pr += 330.0 / rate;
        pr -= pr.floor();
        let a = if n < ramp {
            0.1 + (3.9 * n as f64) / ramp as f64
        } else if (n - ramp) % (burst + gap) < burst {
            3.0
        } else {
            0.05
        };
        l[n] = (a * (1.0 - 4.0 * (pl - 0.5).abs())) as f32;
        r[n] = (0.5 * a * (1.0 - 4.0 * (pr - 0.5).abs())) as f32;
    }
    assert_sha256(&format!("{} input", s.id), &[&l, &r], s.input_sha256.as_deref().expect("input sha"));
    [l, r]
}

/// Tolerance classes (plan § Stage 3). N: the null residual is at or below −60 dB of the reference
/// RMS. S: long-term STFT band energies within ±1 dB and the 10 ms RMS envelope within ±0.5 dB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    N,
    S,
}

#[derive(Debug)]
pub struct Score {
    /// Null residual in dB relative to the reference RMS (all channels together).
    pub residual_db: f64,
    /// Largest band deviation (dB) over bands within 60 dB of the loudest band.
    pub band_db: f64,
    /// Largest envelope deviation (dB) over windows within 50 dB of the loudest window.
    pub envelope_db: f64,
}

pub fn score(reference: &[Vec<f32>], render: &[Vec<f32>], rate: u32) -> Score {
    assert_eq!(reference.len(), render.len(), "channel count");
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (a, b) in reference.iter().zip(render) {
        assert_eq!(a.len(), b.len(), "frame count");
        for (&x, &y) in a.iter().zip(b) {
            sig += (x as f64).powi(2);
            err += (x as f64 - y as f64).powi(2);
        }
    }
    let residual_db = 10.0 * (err.max(1e-300) / sig.max(1e-300)).log10();
    let mut band_db = 0.0f64;
    let mut envelope_db = 0.0f64;
    for (a, b) in reference.iter().zip(render) {
        let (ba, bb) = (bands(a), bands(b));
        let loudest = ba.iter().cloned().fold(0.0, f64::max);
        for (x, y) in ba.iter().zip(&bb) {
            if *x > loudest * 1e-6 {
                band_db = band_db.max((10.0 * (y.max(1e-300) / x).log10()).abs());
            }
        }
        let (ea, eb) = (envelope(a, rate), envelope(b, rate));
        let loudest = ea.iter().cloned().fold(0.0, f64::max);
        for (x, y) in ea.iter().zip(&eb) {
            if *x > loudest * 1e-5 {
                envelope_db = envelope_db.max((10.0 * (y.max(1e-300) / x).log10()).abs());
            }
        }
    }
    Score { residual_db, band_db, envelope_db }
}

/// Octave-band energies (bands 31.25 Hz · 2^k in bin terms) of a 2048-point Hann STFT, hop 1024,
/// summed over the whole signal.
fn bands(x: &[f32]) -> Vec<f64> {
    const N: usize = 2048;
    let fft = FftPlanner::<f64>::new().plan_fft_forward(N);
    let window: Vec<f64> = (0..N).map(|k| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / N as f64).cos()).collect();
    let mut power = vec![0.0f64; N / 2 + 1];
    let mut start = 0;
    while start < x.len() {
        let mut buf: Vec<Complex<f64>> =
            (0..N).map(|k| Complex::new(x.get(start + k).copied().unwrap_or(0.0) as f64 * window[k], 0.0)).collect();
        fft.process(&mut buf);
        for (p, c) in power.iter_mut().zip(&buf) {
            *p += c.norm_sqr();
        }
        start += N / 2;
    }
    // Bin edges 1, 2, 4, … : octave bands above DC.
    let mut out = Vec::new();
    let mut lo = 1;
    while lo < power.len() {
        let hi = (lo * 2).min(power.len());
        out.push(power[lo..hi].iter().sum());
        lo = hi;
    }
    out
}

/// Mean-square energy per 10 ms window.
fn envelope(x: &[f32], rate: u32) -> Vec<f64> {
    let window = rate as usize / 100;
    x.chunks(window).map(|c| c.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / c.len() as f64).collect()
}

/// Assert `render` passes `class` against the scenario's reference; on failure write the render beside
/// it for an ear A/B. Prints the score either way, so a test log shows how much headroom a class has.
pub fn assert_class(id: &str, render: &[Vec<f32>], class: Class) {
    let reference = reference(id);
    let s = score(&reference, render, scenario(id).rate);
    println!("[refs] {id}: residual {:.1} dB, bands {:.2} dB, envelope {:.2} dB", s.residual_db, s.band_db, s.envelope_db);
    let pass = match class {
        Class::N => s.residual_db <= -60.0,
        Class::S => s.band_db <= 1.0 && s.envelope_db <= 0.5,
    };
    if !pass {
        let out = dir().join("out");
        std::fs::create_dir_all(&out).ok();
        let path = out.join(format!("{id}.rust.wav"));
        write_wav(&path, scenario(id).rate, render);
        panic!("{id} fails class {class:?}: {s:?}; render written to {}", path.display());
    }
}

/// A float32 WAV in encodeWav's layout, so the reference and the render open side by side.
pub fn write_wav(path: &std::path::Path, rate: u32, channels: &[Vec<f32>]) {
    let n = channels.len() as u16;
    let frames = channels.first().map_or(0, |c| c.len());
    let data = frames * n as usize * 4;
    let mut b = Vec::with_capacity(58 + data);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&((58 + data - 8) as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&18u32.to_le_bytes());
    b.extend_from_slice(&3u16.to_le_bytes());
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * n as u32 * 4).to_le_bytes());
    b.extend_from_slice(&(n * 4).to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(b"fact");
    b.extend_from_slice(&4u32.to_le_bytes());
    b.extend_from_slice(&(frames as u32).to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(data as u32).to_le_bytes());
    for f in 0..frames {
        for c in channels {
            b.extend_from_slice(&c[f].to_le_bytes());
        }
    }
    std::fs::write(path, b).expect("write the render");
}
