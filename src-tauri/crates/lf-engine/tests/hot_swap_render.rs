//! A rendering tool, not a gate (`#[ignore]`): the hot swap and today's cold swap, rendered offline through
//! the real engine with two distinct amp-sim stand-ins, so the owner's ear can judge the crossfade. Writes
//! four mono 48 kHz WAVs (`note-cold`, `note-hot`, `riff-cold`, `riff-hot`) to `src-tauri/target/hot_swap_render/`
//! and prints the numbers; a script turns them into clips.
//!
//! `cold`: the old unit is removed (the 10 ms fade to bypass), the slot passes the dry input for `GAP`
//! (a stand-in: a real load takes seconds), then the new unit is installed and fades in from bypass.
//! `hot`: `SlotPort::replace`, committing at the swap frame, a 10 ms crossfade with no bypass term.
//!
//!     cargo test -p lf-engine --no-default-features --test hot_swap_render -- --ignored --nocapture

mod common;

use common::{Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{HandbackReason, SlotEvent, SlotKind, SlotProcessor, SlotReply};

const SR: f64 = 48_000.0;
const BLOCK: usize = 128;
const TOTAL: usize = 6 * 48_000;
const START: Frame = 48_000;
/// The swap fade and the stage time past latency: 10 ms each. The replace is sent `STAGE_BLOCKS` before the
/// swap frame so that the commit lands on it (staging 480 frames, whole blocks of 128 = 512).
const FADE: usize = 480;
const STAGE_BLOCKS: usize = 512;
/// The cold swap's stand-in for a plugin's load time.
const GAP: usize = 48_000;
/// Half the jump window: 5 ms.
const HALF: usize = 240;

/// One of the two amp-sim stand-ins. No allocation in `process`.
#[derive(Clone)]
struct Amp {
    bright: bool,
    makeup: f32,
    /// A: one-pole low-pass state. B: one-pole high-pass state.
    y: f32,
    x1: f32,
    coef: f32,
}

impl Amp {
    /// A: warm, `tanh(4x)` then a one-pole low-pass at 3 kHz.
    fn warm() -> Amp {
        Amp { bright: false, makeup: 1.0, y: 0.0, x1: 0.0, coef: (1.0 - (-2.0 * std::f64::consts::PI * 3000.0 / SR).exp()) as f32 }
    }

    /// B: a one-pole high-pass at 200 Hz, gain 8, a hard clip at +-0.8.
    fn bright(makeup: f32) -> Amp {
        Amp { bright: true, makeup, y: 0.0, x1: 0.0, coef: (-2.0 * std::f64::consts::PI * 200.0 / SR).exp() as f32 }
    }

    fn step(&mut self, x: f32) -> f32 {
        if self.bright {
            self.y = self.coef * (self.y + x - self.x1);
            self.x1 = x;
            (8.0 * self.y).clamp(-0.8, 0.8) * self.makeup
        } else {
            self.y += self.coef * ((4.0 * x).tanh() - self.y);
            self.y * self.makeup
        }
    }
}

impl SlotProcessor for Amp {
    fn kind(&self) -> SlotKind {
        SlotKind::Effect
    }

    fn latency(&self) -> Frame {
        0
    }

    fn process(&mut self, _frame: Frame, input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
        for (y, &x) in out.iter_mut().zip(input) {
            *y = self.step(x);
        }
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

fn run(mut amp: Amp, x: &[f32]) -> Vec<f32> {
    x.iter().map(|&s| amp.step(s)).collect()
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, s| m.max(s.abs()))
}

/// A plucked string added into `buf` from sample `at`: harmonics `1..=h`, the higher ones dying faster, a
/// short noise burst for the pick. Deterministic.
fn pluck(buf: &mut [f32], at: usize, f0: f64, amp: f64, tau: f64, h: usize, seed: u32) {
    let mut s = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    for n in at..buf.len() {
        let t = (n - at) as f64 / SR;
        if t > tau * 12.0 {
            break;
        }
        let mut v = 0.0;
        for k in 1..=h {
            let kf = k as f64;
            v += (1.0 / kf.powf(1.1)) * (-t / (tau / (1.0 + 0.5 * (kf - 1.0)))).exp() * (2.0 * std::f64::consts::PI * f0 * kf * t).sin();
        }
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (s >> 8) as f64 / (1u64 << 24) as f64 * 2.0 - 1.0;
        v += 0.25 * noise * (-t / 0.004).exp();
        buf[n] += (amp * v * (1.0 - (-t / 0.0007).exp())) as f32;
    }
}

fn scale_to(buf: &mut [f32], target: f32) {
    let k = target / peak(buf);
    buf.iter_mut().for_each(|s| *s *= k);
}

/// A sustained pluck near G3 (196 Hz), still ringing at 3 s.
fn note_input() -> Vec<f32> {
    let mut b = vec![0.0; TOTAL];
    pluck(&mut b, (0.2 * SR) as usize, 196.0, 1.0, 2.5, 6, 1);
    scale_to(&mut b, 0.5);
    b
}

/// A palm-muted eighth-note riff at 120 bpm on E2, G2 and A2.
fn riff_input() -> Vec<f32> {
    let mut b = vec![0.0; TOTAL];
    let pattern = [(82.41, 1.0), (82.41, 0.7), (82.41, 0.8), (98.0, 1.0), (82.41, 0.7), (82.41, 0.8), (110.0, 1.0), (98.0, 0.9)];
    for i in 0..24 {
        let (f0, a) = pattern[i % 8];
        pluck(&mut b, (i as f64 * 0.25 * SR) as usize, f0, a, 0.14, 4, 10 + i as u32);
    }
    scale_to(&mut b, 0.5);
    b
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Cold,
    Hot,
}

struct Rendered {
    out: Vec<f32>,
    /// Frames from the render's start where something changes: the swap (hot) or the fade to bypass and
    /// the new unit's install (cold).
    events: Vec<usize>,
}

fn render(input: &[f32], swap: usize, mode: Mode, makeup: f32) -> Rendered {
    let mut rig = Rig::with(Opts { block: BLOCK, start: START, ..Default::default() });
    let sig = input.to_vec();
    rig.set_input(move |f| sig.get((f - START) as usize).copied().unwrap_or(0.0));
    rig.install_tagged(0, 1, Box::new(Amp::warm()));
    rig.keep_output();
    let at = |n: usize| START + n as Frame;
    let mut replies = Vec::new();
    let mut events = vec![swap];
    match mode {
        Mode::Hot => {
            rig.advance_to(at(swap - STAGE_BLOCKS));
            rig.replace(0, 2, Box::new(Amp::bright(makeup)), 1.0);
            rig.advance_to(at(swap + FADE + 2 * BLOCK));
        }
        Mode::Cold => {
            rig.advance_to(at(swap));
            rig.remove_tagged(0, 1);
            rig.advance_to(at(swap + GAP));
            while let Some(r) = rig.reply(0) {
                replies.push(r);
            }
            assert!(replies.iter().any(|r| matches!(r, SlotReply::Handback(b) if b.tag == 1)), "the old unit came back before the install");
            rig.install_tagged(0, 2, Box::new(Amp::bright(makeup)));
            events.push(swap + GAP);
        }
    }
    rig.advance_to(at(TOTAL));
    while let Some(r) = rig.reply(0) {
        replies.push(r);
    }
    if mode == Mode::Hot {
        assert!(replies.iter().any(|r| matches!(r, SlotReply::Committed { tag: 2 })), "committed");
        assert!(replies.iter().any(|r| matches!(r, SlotReply::Handback(b) if b.tag == 1 && b.reason == HandbackReason::ReplacedOut)), "the old unit back ReplacedOut");
    }
    assert_eq!(rig.monitor.len(), TOTAL);
    Rendered { out: rig.monitor.clone(), events }
}

/// The hot render around the commit is `(1-t)·A + t·B` of the two units run alone on the same input, B
/// from the replace on: no bypass or dry term in it. Returns the largest deviation.
fn crossfade_deviation(input: &[f32], swap: usize, makeup: f32, got: &[f32]) -> f32 {
    let a = run(Amp::warm(), input);
    let mut b = vec![0.0; TOTAL];
    let from = swap - STAGE_BLOCKS;
    let mut amp = Amp::bright(makeup);
    for n in from..TOTAL {
        b[n] = amp.step(input[n]);
    }
    let mut worst = 0.0f32;
    for n in swap - 300..swap + FADE + 300 {
        let k = n as i64 - swap as i64;
        let want = if k <= 0 {
            a[n]
        } else if k >= FADE as i64 {
            b[n]
        } else {
            a[n] + (k as f32 / FADE as f32) * (b[n] - a[n])
        };
        worst = worst.max((got[n] - want).abs());
    }
    worst
}

fn max_jump(x: &[f32], lo: usize, hi: usize) -> f32 {
    (lo.max(1)..hi.min(x.len())).map(|n| (x[n] - x[n - 1]).abs()).fold(0.0, f32::max)
}

fn db(a: f64, b: f64) -> f64 {
    20.0 * (b / a).log10()
}

/// The numbers for one render (raw engine output, before normalisation).
fn report(name: &str, r: &Rendered) {
    let x = &r.out;
    let w = 24_000;
    // Windows of 10 ms across the render, none touching an event's fade, the silent ones left out.
    let mut elsewhere = Vec::new();
    let mut lo = 0;
    while lo + 2 * HALF <= x.len() {
        let hi = lo + 2 * HALF;
        let near = r.events.iter().any(|&e| lo < e + FADE + HALF && hi > e - HALF);
        let j = max_jump(x, lo, hi);
        if !near && j > 0.0 {
            elsewhere.push(j);
        }
        lo = hi;
    }
    elsewhere.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = elsewhere[elsewhere.len() / 2];
    println!("== {name}");
    for (i, &e) in r.events.iter().enumerate() {
        let label = if r.events.len() == 1 { "swap" } else if i == 0 { "fade to bypass" } else { "new unit in" };
        let around = max_jump(x, e - HALF, e + HALF);
        let span = max_jump(x, e - HALF, e + FADE + HALF);
        println!("  {label} at frame {e} ({:.4} s): max jump +-5 ms {around:.5}, over the fade {span:.5}; elsewhere median-of-max {median:.5} ({} windows); ratio {:.2} / {:.2}", e as f64 / SR, elsewhere.len(), around / median, span / median);
    }
    let first = r.events[0];
    let last = *r.events.last().unwrap();
    let before = &x[first - w..first];
    let after = &x[last + FADE..last + FADE + w];
    println!("  peak before {:.4} after {:.4}; rms before {:.4} after {:.4}: level jump {:+.2} dB", peak(before), peak(after), rms(before), rms(after), db(rms(before), rms(after)));
    if r.events.len() == 2 {
        let gap = &x[first + FADE..last];
        println!("  gap (dry guitar): peak {:.4}, rms {:.4}", peak(gap), rms(gap));
    }
}

fn write_wav(path: &std::path::Path, x: &[f32]) {
    let mut b = Vec::with_capacity(44 + 2 * x.len());
    let data = (2 * x.len()) as u32;
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&48_000u32.to_le_bytes());
    b.extend_from_slice(&96_000u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for &s in x {
        b.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

#[test]
#[ignore = "a rendering tool, not a gate"]
fn render_hot_swap_clips() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/hot_swap_render");
    std::fs::create_dir_all(&dir).unwrap();
    // (name, input, swap frame): the note swaps at 3.000 s; the riff 80 ms into the eighth at 3.0 s.
    let inputs = [("note", note_input(), 144_000usize), ("riff", riff_input(), 147_840usize)];
    for (name, input, swap) in inputs {
        // B's make-up gain: its RMS over the whole input equals A's, so the switch is no level jump.
        let makeup = (rms(&run(Amp::warm(), &input)) / rms(&run(Amp::bright(1.0), &input))) as f32;
        println!("#### {name}: B make-up {makeup:.3} ({:+.1} dB)", 20.0 * makeup.log10());
        let cold = render(&input, swap, Mode::Cold, makeup);
        let hot = render(&input, swap, Mode::Hot, makeup);
        let dev = crossfade_deviation(&input, swap, makeup, &hot.out);
        println!("  hot render vs (1-t)A + tB of the units alone, around the commit: max deviation {dev:e}");
        assert!(dev < 1e-4, "the hot swap holds a bypass/dry term or other: {dev}");
        report(&format!("{name}-cold"), &cold);
        report(&format!("{name}-hot"), &hot);
        // One common factor per pair, to -3 dBFS, then a 5 ms fade at each end.
        let k = 10f32.powf(-3.0 / 20.0) / peak(&cold.out).max(peak(&hot.out));
        for (suffix, r) in [("cold", &cold), ("hot", &hot)] {
            let mut x: Vec<f32> = r.out.iter().map(|s| s * k).collect();
            for n in 0..HALF {
                let g = n as f32 / HALF as f32;
                x[n] *= g;
                let m = x.len() - 1 - n;
                x[m] *= g;
            }
            write_wav(&dir.join(format!("{name}-{suffix}.wav")), &x);
            let marks: Vec<String> = r.events.iter().map(|&e| format!("{:.4}", e as f64 / SR)).collect();
            std::fs::write(dir.join(format!("{name}-{suffix}.marks")), marks.join(" ")).unwrap();
        }
        println!("  pair factor {k:.3}, peak after normalising {:.3}", peak(&cold.out).max(peak(&hot.out)) * k);
    }
}
