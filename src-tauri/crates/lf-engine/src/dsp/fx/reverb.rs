//! `ReverbSendFx`: the track's send gain into the shared reverb bus (bypassed = gain 0). The bus and
//! its convolution reverb are a later wave's; the send renders the bus's input from this track.
//!
//! A send at gain 0 contributes exactly nothing to the mix: the GainNode multiplies by 0 while its
//! gain is automated and outputs silence after, and the reverb of silence is silence.

use super::{Ctl, FxState, RAMP};
use crate::dsp::param::{AudioParam, Rate, ToneParam, Units, QUANTUM};

pub struct ReverbSendFx {
    bypassed: bool,
    amount: f64,
    send: ToneParam,
    values: [f32; QUANTUM],
    /// The k-rate gain is 0: Blink's GainNode outputs silence.
    silent: bool,
}

impl ReverbSendFx {
    pub fn new(sample_rate: f32, ctl: Ctl) -> Self {
        let native = AudioParam::new(sample_rate as f64, 1.0, f32::MIN, f32::MAX, Rate::A);
        ReverbSendFx {
            bypassed: true,
            amount: 0.3,
            send: ToneParam::new(native, Units::Gain, Some(0.0), ctl.frame),
            values: [0.0; QUANTUM],
            silent: true,
        }
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypassed
    }

    pub fn set_bypass(&mut self, bypassed: bool, ctl: Ctl) {
        self.bypassed = bypassed;
        self.send.ramp_to(if bypassed { 0.0 } else { self.amount }, RAMP, ctl.now, ctl.frame);
    }

    pub fn get_param(&self) -> f64 {
        self.amount
    }

    pub fn set_param(&mut self, value: f64, ctl: Ctl) {
        self.amount = value.clamp(0.0, 1.0);
        if !self.bypassed {
            self.send.ramp_to(self.amount, RAMP, ctl.now, ctl.frame);
        }
    }

    pub fn get_state(&self) -> FxState {
        FxState { bypassed: self.bypassed, params: [self.amount, 0.0, 0.0] }
    }

    pub(super) fn begin_quantum(&mut self, quantum_start: u64) {
        let gain = &mut self.send.native;
        if gain.has_sample_accurate_values(quantum_start) {
            gain.calculate_sample_accurate_values(quantum_start, &mut self.values, None);
            self.silent = false;
        } else {
            let g = gain.value(quantum_start);
            self.values.fill(g);
            self.silent = g == 0.0;
        }
    }

    pub(super) fn process(&mut self, at: usize, input: &[f32], send: &mut [f32]) {
        if self.silent {
            send.fill(0.0);
            return;
        }
        for ((s, &x), &g) in send.iter_mut().zip(input).zip(&self.values[at..]) {
            *s = x * g;
        }
    }
}
