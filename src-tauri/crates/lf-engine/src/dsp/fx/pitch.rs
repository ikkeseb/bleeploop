//! `PitchFx`: a CrossFade whose dry input is always connected and whose wet input is a PitchShift
//! that Tone builds on the first enable and keeps from then on.
//!
//! The PitchShift port (two modulated delay lines, plan Stage 3 "hard") is a later wave's; until it
//! lands the wet input stays unconnected, which is exactly what a PitchFx that has never been enabled
//! renders: `b`'s GainNode has no input, so the CrossFade outputs `a` alone (the dry signal at
//! 0.9999988 while bypassed). Enabling it still ramps the fade as Tone does, toward a wet path that is
//! silent here.

use super::{Ctl, FxState, RAMP};
use crate::dsp::crossfade::CrossFade;

pub struct PitchFx {
    bypassed: bool,
    semitones: f64,
    xfade: CrossFade,
}

impl PitchFx {
    pub fn new(sample_rate: f32, ctl: Ctl) -> Self {
        PitchFx { bypassed: true, semitones: 0.0, xfade: CrossFade::new(sample_rate, 0.0, ctl.frame) }
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypassed
    }

    pub fn set_bypass(&mut self, bypassed: bool, ctl: Ctl) {
        self.bypassed = bypassed;
        self.xfade.fade.ramp_to(if bypassed { 0.0 } else { 1.0 }, RAMP, ctl.now, ctl.frame);
    }

    pub fn get_param(&self) -> f64 {
        self.semitones
    }

    pub fn set_param(&mut self, value: f64) {
        self.semitones = value;
    }

    pub fn get_state(&self) -> FxState {
        FxState { bypassed: self.bypassed, params: [self.semitones, 0.0, 0.0] }
    }

    pub(super) fn begin_quantum(&mut self, quantum_start: u64) {
        self.xfade.begin_quantum(quantum_start);
    }

    pub(super) fn process(&mut self, at: usize, input: &[f32], out: &mut [f32]) {
        self.xfade.process(at, Some(input), None, out);
    }
}
