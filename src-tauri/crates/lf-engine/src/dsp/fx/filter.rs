//! `FilterFx`: Tone's 24 dB lowpass `Filter` under a CrossFade bypass (fade 0 = dry).

use super::{Ctl, FxParam, FxState, RAMP};
use crate::dsp::biquad::FilterType;
use crate::dsp::crossfade::CrossFade;
use crate::dsp::filter::{Filter, Rolloff};
use crate::dsp::param::QUANTUM;

pub struct FilterFx {
    bypassed: bool,
    cutoff: f64,
    q: f64,
    filter: Filter,
    xfade: CrossFade,
    wet: [f32; QUANTUM],
}

impl FilterFx {
    pub fn new(sample_rate: f32, ctl: Ctl) -> Self {
        let (cutoff, q) = (1200.0, 2.0);
        FilterFx {
            bypassed: true,
            cutoff,
            q,
            filter: Filter::new(sample_rate, FilterType::Lowpass, cutoff, q, Rolloff::Db24, ctl.frame),
            xfade: CrossFade::new(sample_rate, 0.0, ctl.frame),
            wet: [0.0; QUANTUM],
        }
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypassed
    }

    pub fn set_bypass(&mut self, bypassed: bool, ctl: Ctl) {
        self.bypassed = bypassed;
        self.xfade.fade.ramp_to(if bypassed { 0.0 } else { 1.0 }, RAMP, ctl.now, ctl.frame);
    }

    pub fn get_param(&self, param: FxParam) -> f64 {
        if param == FxParam::Cutoff {
            self.cutoff
        } else {
            self.q
        }
    }

    /// Unclamped, as fx.ts sets them: the UI and session import keep them in range.
    pub fn set_param(&mut self, param: FxParam, value: f64, ctl: Ctl) {
        if param == FxParam::Cutoff {
            self.cutoff = value;
            self.filter.frequency.ramp_to(value, RAMP, ctl.now, ctl.frame);
        } else {
            self.q = value;
            self.filter.q.ramp_to(value, RAMP, ctl.now, ctl.frame);
        }
    }

    pub fn get_state(&self) -> FxState {
        FxState { bypassed: self.bypassed, params: [self.cutoff, self.q, 0.0] }
    }

    pub(super) fn begin_quantum(&mut self, quantum_start: u64) {
        self.filter.begin_quantum(quantum_start);
        self.xfade.begin_quantum(quantum_start);
    }

    pub(super) fn process(&mut self, at: usize, input: &[f32], out: &mut [f32]) {
        let wet = &mut self.wet[..input.len()];
        self.filter.process(at, input, wet);
        self.xfade.process(at, Some(input), Some(wet), out);
    }
}
