//! A gain stage: Tone's `Gain` (`core/context/Gain.js`, a [`ToneParam`] over the native gain) on
//! Blink's GainNode (`modules/webaudio/gain_handler.cc`, with the silence propagation of
//! `AudioHandler::ProcessIfNecessary` in `audio_handler.cc` and the gain copies of
//! `platform/audio/audio_bus.cc`), at Chromium 153.0.8010.12. One call renders one quantum.
//!
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.

use super::param::{AudioParam, Rate, ToneParam, Units, QUANTUM};

pub struct GainNode {
    pub gain: ToneParam,
    sample_rate: f64,
    values: [f32; QUANTUM],
    /// Blink's `last_non_silent_time_`: the end of the last quantum with a non-silent input.
    last_non_silent_time: f64,
}

impl GainNode {
    /// Tone's `new Gain({ gain, units })`; `frame` is the frame being rendered when it is built.
    pub fn new(sample_rate: f64, gain: f64, units: Units, frame: u64) -> Self {
        let native = AudioParam::new(sample_rate, 1.0, f32::MIN, f32::MAX, Rate::A);
        GainNode { gain: ToneParam::new(native, units, Some(gain), frame), sample_rate, values: [0.0; QUANTUM], last_non_silent_time: 0.0 }
    }

    /// Back to a freshly built node (Tone builds a new one; the engine reuses this one).
    pub fn reset(&mut self, gain: f64, frame: u64) {
        self.gain.reset(Some(gain), frame);
        self.last_non_silent_time = 0.0;
    }

    /// Render the quantum at `quantum_start` from `input` (`None` when every input is silent) into
    /// `out`, one slot per input channel. Returns whether the output is silent.
    pub fn process(&mut self, quantum_start: u64, input: Option<&[[f32; QUANTUM]]>, out: &mut [[f32; QUANTUM]]) -> bool {
        let current_time = quantum_start as f64 / self.sample_rate;
        let silent = match input {
            // Every input silent and the (tail-less) node past its last sound: only the automation runs.
            None if self.last_non_silent_time < current_time => {
                self.gain.native.calculate_sample_accurate_values(quantum_start, &mut self.values, None);
                zero(out);
                true
            }
            _ => self.render(quantum_start, input, out),
        };
        if input.is_some() {
            self.last_non_silent_time = (quantum_start + QUANTUM as u64) as f64 / self.sample_rate;
        }
        silent
    }

    fn render(&mut self, quantum_start: u64, input: Option<&[[f32; QUANTUM]]>, out: &mut [[f32; QUANTUM]]) -> bool {
        let param = &mut self.gain.native;
        let sample_accurate = param.has_sample_accurate_values(quantum_start);
        if sample_accurate && param.rate() == Rate::A {
            param.calculate_sample_accurate_values(quantum_start, &mut self.values, None);
            let Some(input) = input else {
                zero(out);
                return true;
            };
            for (o, i) in out.iter_mut().zip(input) {
                for ((o, &x), &g) in o.iter_mut().zip(i).zip(&self.values) {
                    *o = x * g;
                }
            }
            return false;
        }
        let gain = if sample_accurate { param.final_value(quantum_start, None) } else { param.value(quantum_start) };
        match input {
            Some(input) if gain != 0.0 => {
                for (o, i) in out.iter_mut().zip(input) {
                    if gain == 1.0 {
                        o.copy_from_slice(i);
                    } else {
                        for (o, &x) in o.iter_mut().zip(i) {
                            *o = x * gain;
                        }
                    }
                }
                false
            }
            _ => {
                zero(out);
                true
            }
        }
    }
}

fn zero(out: &mut [[f32; QUANTUM]]) {
    for c in out.iter_mut() {
        c.fill(0.0);
    }
}
