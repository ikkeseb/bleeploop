//! The listening sheet's source signal: a plucked string (Karplus-Strong, its loop tuned to the note by a
//! first-order allpass, the pluck a filtered noise burst with a pick-position comb), so a clip carries the
//! attacks, the sustain and the harmonics a clean electric guitar would. Deterministic: the noise is a
//! fixed LCG seeded per note. A phrase is rendered once into a buffer and indexed by frame; no DC
//! (a blocker runs over the whole phrase) and no sine.

use std::rc::Rc;

/// One note: its start and length in the phrase's units (an eighth note unless a clip says otherwise), its
/// MIDI number, and how long the string rings while held (T60, seconds; 0 = by the note's pitch). Past its
/// length the string is damped, as a fretting hand lets go.
#[derive(Clone, Copy)]
pub struct Note {
    pub at: f64,
    pub midi: u8,
    pub len: f64,
    pub vel: f32,
    pub ring: f64,
}

pub const fn n(at: f64, midi: u8, len: f64) -> Note {
    Note { at, midi, len, vel: 0.8, ring: 0.0 }
}

pub const fn accent(at: f64, midi: u8, len: f64) -> Note {
    Note { at, midi, len, vel: 1.0, ring: 0.0 }
}

pub const fn held(at: f64, midi: u8, len: f64, ring: f64) -> Note {
    Note { at, midi, len, vel: 0.9, ring }
}

/// E minor pentatonic, climbing, its last note high: asymmetric, so a reversed pass is obvious.
pub const RIFF_A: [Note; 6] = [accent(0.0, 52, 0.9), n(1.0, 55, 0.9), n(2.0, 57, 1.8), n(4.0, 59, 0.9), n(5.0, 62, 0.9), n(6.0, 64, 1.6)];
/// A second bar for RIFF_A, falling back down (TRIM's bar 2).
pub const RIFF_A2: [Note; 6] = [accent(8.0, 64, 0.9), n(9.0, 62, 0.9), n(10.0, 59, 0.9), n(11.0, 57, 0.9), n(12.0, 55, 1.8), n(14.0, 52, 1.6)];
/// Low strings, an answering line.
pub const RIFF_B: [Note; 5] = [accent(0.0, 40, 2.8), n(3.0, 43, 0.9), n(4.0, 45, 1.8), n(6.0, 47, 0.8), n(7.0, 50, 0.8)];
/// A second bar for RIFF_B (the multiply's two-bar take).
pub const RIFF_B2: [Note; 4] = [accent(8.0, 45, 2.8), n(11.0, 47, 0.9), n(12.0, 43, 1.8), n(14.0, 40, 1.6)];
/// High melody, its last note held to the bar line.
pub const RIFF_C: [Note; 4] = [accent(0.0, 71, 1.8), n(2.0, 69, 0.9), n(3.0, 67, 0.9), n(4.0, 64, 3.5)];
/// A layer whose last note rings across the bar line (an UNDO that switches there cuts it mid-note).
pub const LAYER: [Note; 4] = [n(1.0, 71, 0.9), n(2.0, 69, 0.9), n(3.0, 67, 0.9), held(5.0, 64, 4.5, 4.0)];
/// Two short stabs: room for a delay's repeats between them.
pub const STABS: [Note; 4] = [accent(0.0, 59, 0.4), accent(0.0, 64, 0.4), n(3.0, 57, 0.4), n(3.0, 62, 0.4)];
/// One pluck on the 1 (two strings).
pub const DOT: [Note; 2] = [accent(0.0, 59, 0.5), accent(0.0, 64, 0.5)];
/// 6/8, in eighths: a rolling arpeggio, bar 2 a step up.
pub const SIX_EIGHT: [Note; 12] = [
    accent(0.0, 52, 1.0),
    n(1.0, 59, 1.0),
    n(2.0, 64, 1.0),
    accent(3.0, 67, 1.0),
    n(4.0, 64, 1.0),
    n(5.0, 59, 0.8),
    accent(6.0, 57, 1.0),
    n(7.0, 64, 1.0),
    n(8.0, 69, 1.0),
    accent(9.0, 72, 1.0),
    n(10.0, 69, 1.0),
    n(11.0, 64, 0.8),
];

/// How long a released string takes to die away (T60, seconds).
const DAMPED: f64 = 0.07;

fn lcg(s: u64) -> u64 {
    s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407)
}

/// One plucked string into `out` from frame `start`, held `held` frames.
fn pluck(out: &mut [f32], start: usize, note: &Note, held: usize, sr: f64, seed: u64) {
    let freq = 440.0 * 2f64.powf((note.midi as f64 - 69.0) / 12.0);
    // The loop's delay: the line, half a frame of the averaging filter, the allpass's fraction.
    let l = sr / freq;
    let n = (l - 1.0).floor() as usize;
    let d = l - n as f64 - 0.5;
    let c = ((1.0 - d) / (1.0 + d)) as f32;
    // The pluck: low-passed noise, brighter for a harder pick, combed at the pick position, no DC.
    let mut rng = seed ^ 0x9E37_79B9_7F4A_7C15;
    let bright = 0.25 + 0.55 * note.vel;
    let (mut lp, mut noise) = (0.0f32, vec![0.0f32; n]);
    for x in noise.iter_mut() {
        rng = lcg(rng);
        let w = (rng >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0;
        lp += bright * (w - lp);
        *x = lp;
    }
    let p = ((n as f64 * 0.17).round() as usize).max(1);
    let mut line: Vec<f32> = (0..n).map(|i| noise[i] - noise[(i + p) % n]).collect();
    let mean = line.iter().sum::<f32>() / n as f32;
    let peak = line.iter().fold(0.0f32, |m, x| m.max((x - mean).abs())).max(1e-9);
    for x in line.iter_mut() {
        *x = (*x - mean) / peak * note.vel;
    }
    let ring = if note.ring > 0.0 { note.ring } else { (3.0 * (196.0 / freq).sqrt()).clamp(1.2, 5.0) };
    let rho_held = 10f64.powf(-3.0 / (ring * freq)) as f32;
    let rho_damped = 10f64.powf(-3.0 / (DAMPED * freq)) as f32;
    let total = held + (DAMPED * 3.0 * sr) as usize;
    let (mut idx, mut prev, mut ax, mut ay) = (0usize, 0.0f32, 0.0f32, 0.0f32);
    for k in 0..total {
        let Some(o) = out.get_mut(start + k) else { break };
        let x = line[idx];
        *o += x;
        let rho = if k < held { rho_held } else { rho_damped };
        let v = rho * 0.5 * (x + prev);
        prev = x;
        let y = c * v + ax - c * ay;
        (ax, ay) = (v, y);
        line[idx] = y;
        idx = (idx + 1) % n;
    }
}

/// `notes` repeated every `period` units (0: once), `unit` frames each, into `frames` frames, then a
/// DC blocker; scaled so its peak is `peak`.
fn render(notes: &[Note], unit: f64, sr: u32, frames: usize, period: f64, repeats: usize, peak: f32) -> Vec<f32> {
    let mut out = vec![0.0f32; frames];
    for r in 0..repeats {
        for (i, note) in notes.iter().enumerate() {
            let start = ((note.at + r as f64 * period) * unit).round() as usize;
            let held = (note.len * unit).round() as usize;
            pluck(&mut out, start, note, held, sr as f64, (i as u64 + 1) * 7919 + note.midi as u64);
        }
    }
    let (mut x1, mut y1) = (0.0f32, 0.0f32);
    for x in out.iter_mut() {
        let y = *x - x1 + 0.995 * y1;
        (x1, y1) = (*x, y);
        *x = y;
    }
    let top = out.iter().fold(0.0f32, |m, x| m.max(x.abs())).max(1e-9);
    out.iter_mut().for_each(|x| *x *= peak / top);
    out
}

/// A phrase played once from frame 0, `frames` long.
pub fn once(notes: &[Note], unit: f64, sr: u32, frames: usize, peak: f32) -> Rc<Vec<f32>> {
    Rc::new(render(notes, unit, sr, frames, 0.0, 1, peak))
}

/// A phrase as the player repeats it, one period `period` units long: its third repetition, so the
/// strings still ringing from the one before are in it and it loops without a seam of its own.
pub fn periodic(notes: &[Note], unit: f64, sr: u32, period: f64, peak: f32) -> Rc<Vec<f32>> {
    let len = (period * unit).round() as usize;
    let all = render(notes, unit, sr, 3 * len, period, 3, 1.0);
    let mut last = all[2 * len..].to_vec();
    let top = last.iter().fold(0.0f32, |m, x| m.max(x.abs())).max(1e-9);
    last.iter_mut().for_each(|x| *x *= peak / top);
    Rc::new(last)
}
