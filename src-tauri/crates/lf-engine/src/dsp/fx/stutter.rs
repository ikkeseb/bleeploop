//! `StutterFx`: a tempo-synced amplitude gate under a CrossFade bypass (fade 0 = dry).
//!
//! The gate is a native GainNode whose gain (0) sums a looped control buffer: one second at the
//! context rate, ones for its first half, played by an AudioBufferSourceNode at `1 / period` so one
//! pass lasts one division, started at an offset that puts it on the grid. A rate or grid change
//! (`scheduleGate`) starts a new source and stops the old one at the same time; fx.ts reads that time
//! from the native `currentTime`, here the control's context time ([`Ctl::context_time`]).
//! standardized-audio-context's `value` setter adds a `setValueAtTime` at the context time to
//! Blink's own; both are kept.
//!
//! Tone makes a source per change and drops the old one when it ends. The old one's stop time is the
//! context time, so from the next quantum on it renders nothing; two sources in a pool are enough.

use std::sync::Arc;

use super::{clamp_index, division_beats, Ctl, FxState, FxTiming, DIVISIONS, RAMP};
use crate::dsp::buffer_source::{AudioBuffer, BufferSource, PlaybackState};
use crate::dsp::crossfade::CrossFade;
use crate::dsp::param::{AudioParam, Rate, QUANTUM};

/// `gateBuffer`: one second of control signal, ones for the first `floor(rate / 2)` frames.
pub fn gate_buffer(sample_rate: f32) -> Arc<AudioBuffer> {
    let length = sample_rate as usize;
    let mut data = vec![0.0f32; length];
    data[..(sample_rate / 2.0).floor() as usize].fill(1.0);
    Arc::new(AudioBuffer::new(sample_rate, vec![data]))
}

pub struct StutterFx {
    bypassed: bool,
    /// Index into [`DIVISIONS`].
    rate: usize,
    timing: Option<FxTiming>,
    sample_rate: f32,
    /// The gate GainNode's gain.
    gate: AudioParam,
    sources: [BufferSource; 2],
    /// The pool slot fx.ts calls `control`.
    control: Option<usize>,
    buffer: Arc<AudioBuffer>,
    xfade: CrossFade,
    control_sum: [f32; QUANTUM],
    gate_values: [f32; QUANTUM],
    gated: [f32; QUANTUM],
}

impl StutterFx {
    pub fn new(sample_rate: f32, ctl: Ctl) -> Self {
        let mut gate = AudioParam::new(sample_rate as f64, 1.0, f32::MIN, f32::MAX, Rate::A);
        set_native_value(&mut gate, 0.0, sample_rate, ctl);
        StutterFx {
            bypassed: true,
            rate: 1,
            timing: None,
            sample_rate,
            gate,
            sources: std::array::from_fn(|_| BufferSource::new(sample_rate, ctl.frame)),
            control: None,
            buffer: gate_buffer(sample_rate),
            xfade: CrossFade::new(sample_rate, 0.0, ctl.frame),
            control_sum: [0.0; QUANTUM],
            gate_values: [0.0; QUANTUM],
            gated: [0.0; QUANTUM],
        }
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypassed
    }

    pub fn set_bypass(&mut self, bypassed: bool, ctl: Ctl) {
        self.bypassed = bypassed;
        self.xfade.fade.ramp_to(if bypassed { 0.0 } else { 1.0 }, RAMP, ctl.now, ctl.frame);
    }

    pub fn get_param(&self) -> f64 {
        self.rate as f64
    }

    pub fn set_param(&mut self, value: f64, ctl: Ctl) {
        let next = clamp_index(value, DIVISIONS.len());
        if next == self.rate {
            return;
        }
        self.rate = next;
        self.schedule_gate(ctl);
    }

    pub fn get_state(&self) -> FxState {
        FxState { bypassed: self.bypassed, params: [self.rate as f64, 0.0, 0.0] }
    }

    pub fn set_timing(&mut self, timing: FxTiming, ctl: Ctl) {
        if self.timing == Some(timing) {
            return;
        }
        self.timing = Some(timing);
        self.schedule_gate(ctl);
    }

    fn schedule_gate(&mut self, ctl: Ctl) {
        let Some(timing) = self.timing else { return };
        let period = timing.beat_period * division_beats(self.rate);
        let when = ctl.context_time(self.sample_rate);
        let offset = (((when - timing.anchor) % period) + period) % period / period;
        let slot = self.control.map_or(0, |c| 1 - c);
        let next = &mut self.sources[slot];
        next.reset(ctl.frame);
        next.set_buffer(Arc::clone(&self.buffer));
        next.set_loop(true);
        set_native_value(&mut next.playback_rate.native, 1.0 / period, self.sample_rate, ctl);
        self.gate.set_connected(true);
        next.start_grain(when, offset, None, ctl.frame);
        if let Some(previous) = self.control {
            self.sources[previous].stop(when);
        }
        self.control = Some(slot);
    }

    pub(super) fn begin_quantum(&mut self, quantum_start: u64) {
        self.xfade.begin_quantum(quantum_start);
        // The param's summing junction: every connected source's non-silent output.
        self.control_sum.fill(0.0);
        let mut any = false;
        for s in self.sources.iter_mut() {
            if matches!(s.state(), PlaybackState::Scheduled | PlaybackState::Playing) {
                s.process(quantum_start);
                if !s.silent() {
                    any = true;
                    for (sum, &v) in self.control_sum.iter_mut().zip(&s.output()[0]) {
                        *sum += v;
                    }
                }
            }
        }
        let input = any.then_some(&self.control_sum);
        self.gate.calculate_sample_accurate_values(quantum_start, &mut self.gate_values, input);
    }

    pub(super) fn process(&mut self, at: usize, input: &[f32], out: &mut [f32]) {
        let gated = &mut self.gated[..input.len()];
        for ((g, &x), &v) in gated.iter_mut().zip(input).zip(&self.gate_values[at..]) {
            *g = x * v;
        }
        self.xfade.process(at, Some(input), Some(gated), out);
    }
}

/// A native param's `value` setter through standardized-audio-context: Blink's setter (the intrinsic
/// value and a `setValueAtTime` at the context time), then sac's own `setValueAtTime` there.
fn set_native_value(param: &mut AudioParam, value: f64, sample_rate: f32, ctl: Ctl) {
    param.set_value(value as f32, ctl.frame);
    param.set_value_at_time(value as f32, ctl.context_time(sample_rate), ctl.frame);
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48000.0;

    fn gate(fx: &mut StutterFx, quantum_start: u64) -> [f32; QUANTUM] {
        fx.begin_quantum(quantum_start);
        fx.gate_values
    }

    #[test]
    fn the_gate_is_open_for_the_first_half_of_each_division() {
        let start = Ctl { now: 0.0, frame: 0 };
        let mut fx = StutterFx::new(RATE, start);
        fx.set_timing(FxTiming { anchor: 0.0, beat_period: 0.5 }, start);
        // 1/8 at 120 BPM: 6000 frames open, 6000 shut.
        let values: Vec<f32> = (0..96u64).flat_map(|q| gate(&mut fx, q * 128)).collect();
        for (n, &v) in values.iter().enumerate() {
            assert_eq!(v, if n % 12000 < 6000 { 1.0 } else { 0.0 }, "frame {n}");
        }
    }

    #[test]
    fn a_live_rate_change_restarts_on_the_grid() {
        let timing = FxTiming { anchor: -0.087, beat_period: 0.43795833333333334 };
        let start = Ctl { now: 0.0, frame: 0 };
        // `steady` runs 1/8. from frame 0; `live` runs 1/16 and switches mid-quantum at `f`.
        let mut steady = StutterFx::new(RATE, start);
        steady.set_param(2.0, start);
        steady.set_timing(timing, start);
        let mut live = StutterFx::new(RATE, start);
        live.set_param(3.0, start);
        live.set_timing(timing, start);
        let f: u64 = 24064 + 37;
        let switch = f.next_multiple_of(128);
        for q in 0..375u64 {
            let quantum_start = q * 128;
            let a = gate(&mut steady, quantum_start);
            let b = gate(&mut live, quantum_start);
            if (quantum_start..quantum_start + 128).contains(&f) {
                live.set_param(2.0, Ctl::at(f, RATE));
            }
            if quantum_start >= switch {
                // The same grid phase. The steady gate's float playback rate (1/period rounded to
                // f32) has drifted a few thousandths of a buffer frame off the grid by now, which
                // shows only on the interpolated samples of an edge.
                let edges = a.iter().zip(&b).filter(|(x, y)| x != y).count();
                assert!(a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 0.01) && edges <= 1, "quantum {q}");
            }
        }
    }
}
