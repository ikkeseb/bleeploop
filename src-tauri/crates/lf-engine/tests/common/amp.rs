//! `ToyAmp`: a stand-in for an amp-sim plugin, with memory past the latency it reports. It reports
//! [`ToyAmp::LATENCY`] frames and is a delay of exactly that, then `tanh(6x)`, a two-pole low-pass near
//! 3 kHz (RBJ, Q 1/√2) and a 15 ms one-pole "cab" ring added under it. A dropout in its input reaches
//! its output `LATENCY` frames late, as a pure delay of the reported latency would, and then rings on
//! for as long as the low-pass and the cab remember it: the part no routing mark covers. Deterministic,
//! allocation-free in `process`, its states flushed to zero below 1e-20 (a plugin's FTZ), so it falls
//! silent exactly once its input does. Not a model of any amp: the listening render
//! (`tests/dropout_ab.rs`) is its user.

use lf_engine::grid::Frame;
use lf_engine::{SlotEvent, SlotKind, SlotProcessor};

pub struct ToyAmp {
    line: Vec<f32>,
    at: usize,
    /// The low-pass's coefficients, normalised by `a0`, and its direct-form-I history.
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
    /// The cab's pole and state.
    pole: f32,
    cab: f32,
}

impl ToyAmp {
    pub const LATENCY: Frame = 256;

    pub fn new(sr: u32) -> Self {
        let w = 2.0 * std::f64::consts::PI * 3000.0 / sr as f64;
        let alpha = w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let c = w.cos();
        ToyAmp {
            line: vec![0.0; Self::LATENCY as usize],
            at: 0,
            b: [((1.0 - c) / 2.0 / a0) as f32, ((1.0 - c) / a0) as f32, ((1.0 - c) / 2.0 / a0) as f32],
            a: [(-2.0 * c / a0) as f32, ((1.0 - alpha) / a0) as f32],
            x: [0.0; 2],
            y: [0.0; 2],
            pole: (-1.0 / (0.015 * sr as f64)).exp() as f32,
            cab: 0.0,
        }
    }
}

fn flush(v: f32) -> f32 {
    if v.abs() < 1e-20 {
        0.0
    } else {
        v
    }
}

impl SlotProcessor for ToyAmp {
    fn kind(&self) -> SlotKind {
        SlotKind::Effect
    }

    fn latency(&self) -> Frame {
        Self::LATENCY
    }

    fn process(&mut self, _frame: Frame, input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
        for (w, &x) in out.iter_mut().zip(input) {
            let late = std::mem::replace(&mut self.line[self.at], x);
            self.at = (self.at + 1) % self.line.len();
            let s = (6.0 * late).tanh();
            let lp = flush(self.b[0] * s + self.b[1] * self.x[0] + self.b[2] * self.x[1] - self.a[0] * self.y[0] - self.a[1] * self.y[1]);
            self.x = [s, self.x[0]];
            self.y = [lp, self.y[0]];
            self.cab = flush(self.pole * self.cab + (1.0 - self.pole) * lp);
            *w = 0.5 * (lp + self.cab);
        }
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}
