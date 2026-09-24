//! The graph pieces Tone's synths are wired from, one quantum per call: Tone's `Signal` on Blink's
//! ConstantSourceNode ([`Signal`]), Blink's WaveShaperNode with Tone's mapped curves ([`WaveShaper`]),
//! and a GainNode whose gain has a signal connected into it ([`ParamGain`]).
//!
//! [`Signal`] ports `signal/Signal.js` (a ToneConstantSource started at construction, its offset a
//! Tone [`ToneParam`]) over `modules/webaudio/constant_source_handler.cc`. [`WaveShaper`] ports
//! `signal/WaveShaper.js`'s `setMap` and the x86 path of `WaveShaperCurveValues` in
//! `modules/webaudio/wave_shaper_handler.cc` (no oversampling). standardized-audio-context connects a
//! looping silent buffer into every WaveShaperNode whose curve is not zero at its centre (its bug
//! #119 workaround, `native-wave-shaper-node-factory.js`), so such a shaper never propagates
//! silence: this one always maps its input, zero included. [`ParamGain`] ports
//! `modules/webaudio/gain_handler.cc` for a gain that is a-rate because a signal is connected.
//!
//! Every Tone source also gets a keep-alive gain of 0 into the destination from
//! standardized-audio-context (`add-silent-connection.js`). Its only effect on the output is that every
//! source renders every quantum, which is what these nodes do; the zeros it sums in are left out.
//!
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.

use crate::dsp::param::{AudioParam, Rate, ToneParam, Units, QUANTUM};

const Q: usize = QUANTUM;

/// Tone's `Signal`: a constant source whose offset Tone schedules. Started when built, never stopped.
pub struct Signal {
    pub param: ToneParam,
    out: [f32; Q],
}

impl Signal {
    /// `new Signal({ value, units })`; `frame` is the frame being rendered when it is built.
    pub fn new(sample_rate: f32, units: Units, value: f64, frame: u64) -> Self {
        // ConstantSourceNode.offset: a-rate, default 1, the whole float range.
        let native = AudioParam::new(sample_rate as f64, 1.0, f32::MIN, f32::MAX, Rate::A);
        Signal { param: ToneParam::new(native, units, Some(value), frame), out: [0.0; Q] }
    }

    /// Back to a freshly built signal with `value`.
    pub fn reset(&mut self, value: f64, frame: u64) {
        self.param.reset(Some(value), frame);
    }

    /// Render the quantum at `q`; `input` sums into the offset once something is connected to it.
    pub fn process(&mut self, q: u64, input: Option<&[f32; Q]>) -> &[f32; Q] {
        let p = &mut self.param.native;
        let sample_accurate = p.has_sample_accurate_values(q);
        if sample_accurate && p.rate() == Rate::A {
            p.calculate_sample_accurate_values(q, &mut self.out, input);
        } else {
            let value = if sample_accurate { p.final_value(q, input.map(|x| x[0])) } else { p.value(q) };
            self.out.fill(value);
        }
        &self.out
    }

    /// The last rendered quantum.
    pub fn output(&self) -> &[f32; Q] {
        &self.out
    }
}

/// Blink's WaveShaperNode with a Tone mapping (`setMap`, 1024 points by default).
pub struct WaveShaper {
    curve: Vec<f32>,
}

impl WaveShaper {
    /// `new WaveShaper({ mapping, length })`: `mapping(x)` at `length` points over -1..1, stored as
    /// float.
    pub fn new(length: usize, mapping: impl Fn(f64) -> f64) -> Self {
        assert!(length >= 2, "a curve of at least two points");
        let last = (length - 1) as f64;
        WaveShaper { curve: (0..length).map(|i| mapping((i as f64 / last) * 2.0 - 1.0) as f32).collect() }
    }

    /// Tone's `AudioToGain`: -1..1 to 0..1.
    pub fn audio_to_gain() -> Self {
        WaveShaper::new(1024, |x| (x + 1.0) / 2.0)
    }

    /// Tone's `GainToAudio`: 0..1 to -1..1.
    pub fn gain_to_audio() -> Self {
        WaveShaper::new(1024, |x| x.abs() * 2.0 - 1.0)
    }

    /// `WaveShaperCurveValues`, the SSE path: float index arithmetic, then `v1 + f * (v2 - v1)`.
    pub fn process(&self, input: &[f32; Q], out: &mut [f32; Q]) {
        let curve = &self.curve[..];
        let max_index = (curve.len() - 1) as i32;
        let high = max_index as f32;
        let scale = (0.5 * (curve.len() - 1) as f64) as f32;
        for (o, &x) in out.iter_mut().zip(input) {
            let v = (x + 1.0) * scale;
            // Vclip: max(0, min(high, v)) with SSE's operand order.
            let v = if high < v { high } else { v };
            let v = if 0.0 > v { 0.0 } else { v };
            let index1 = v as i32;
            let v1 = curve[index1.clamp(0, max_index) as usize];
            let v2 = curve[(index1.wrapping_add(1)).clamp(0, max_index) as usize];
            let f = v - index1 as f32;
            *o = f * (v2 - v1) + v1;
        }
    }
}

/// A GainNode whose gain param has a signal connected: sample-accurate every quantum, the timeline
/// plus the signal.
pub struct ParamGain {
    pub gain: ToneParam,
    values: [f32; Q],
}

impl ParamGain {
    /// Tone's `new Gain({ gain })` with a signal connected to its gain (`connectSignal` when
    /// `zeroed`, which cancels the schedule and sets 0 at 0; a plain connect otherwise).
    pub fn new(sample_rate: f32, gain: f64, zeroed: bool, frame: u64) -> Self {
        let native = AudioParam::new(sample_rate as f64, 1.0, f32::MIN, f32::MAX, Rate::A);
        let mut p = ParamGain { gain: ToneParam::new(native, Units::Gain, Some(gain), frame), values: [0.0; Q] };
        p.connect(zeroed, frame);
        p
    }

    pub fn reset(&mut self, gain: f64, zeroed: bool, frame: u64) {
        self.gain.reset(Some(gain), frame);
        self.connect(zeroed, frame);
    }

    fn connect(&mut self, zeroed: bool, frame: u64) {
        if zeroed {
            self.gain.connect_signal(false, frame);
        } else {
            self.gain.native.set_connected(true);
        }
    }

    /// Render the quantum at `q`: `input` times the gain values. The automation runs whether or not
    /// the input is silent (`None`).
    pub fn process(&mut self, q: u64, input: Option<&[f32; Q]>, gain_input: Option<&[f32; Q]>, out: &mut [f32; Q]) -> bool {
        self.gain.native.calculate_sample_accurate_values(q, &mut self.values, gain_input);
        match input {
            Some(x) => {
                for ((o, &x), &g) in out.iter_mut().zip(x).zip(&self.values) {
                    *o = x * g;
                }
                false
            }
            None => {
                out.fill(0.0);
                true
            }
        }
    }
}
