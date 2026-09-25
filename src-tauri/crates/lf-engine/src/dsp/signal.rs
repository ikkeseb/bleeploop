//! Tone's signal plumbing, one quantum per call: Tone's `Signal` on Blink's ConstantSourceNode
//! ([`Signal`]), Blink's WaveShaperNode with Tone's mapped curves ([`WaveShaper`]), Tone's `Scale`
//! ([`Scale`]), and Tone's `connectSignal` into a native param ([`connect_signal`]; into a Tone param
//! it is [`ToneParam::connect_signal`]).
//!
//! [`Signal`] ports `signal/Signal.js` over `signal/ToneConstantSource.js` and
//! `modules/webaudio/constant_source_handler.cc`: a ConstantSourceNode started at construction, its
//! offset a Tone [`ToneParam`]. ToneConstantSource puts a gain envelope of exactly 1 after it from
//! time 0, so the output is the offset's values. [`WaveShaper`] ports `signal/WaveShaper.js`'s
//! `setMap` and the x86 path of `WaveShaperCurveValues` in `modules/webaudio/wave_shaper_handler.cc`
//! (no oversampling). standardized-audio-context connects a looping silent buffer into every
//! WaveShaperNode whose curve is not zero at its centre (its bug #119 workaround,
//! `native-wave-shaper-node-factory.js`), so such a shaper never propagates silence: this one always
//! maps its input, zero included. [`Scale`] ports `signal/Scale.js` over `Multiply.js` and `Add.js`:
//! a Gain whose gain is `max - min`, then a unity Gain summing that with a constant source of `min`.
//! [`connect_signal`] ports `connectSignal` from `signal/Signal.js`.
//!
//! Every Tone source also gets a keep-alive gain of 0 into the destination from
//! standardized-audio-context (`add-silent-connection.js`). Its only effect on the output is that every
//! source renders every quantum, which is what these nodes do; the zeros it sums in are left out.
//!
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.

use super::gain::GainNode;
use super::param::{AudioParam, Rate, ToneParam, Units, QUANTUM};

const Q: usize = QUANTUM;

/// Tone's `Signal`: a constant source whose offset Tone schedules. Started when built, never stopped.
pub struct Signal {
    pub param: ToneParam,
    out: [f32; Q],
}

impl Signal {
    /// `new Signal({ value, units })`; `frame` is the frame being rendered when it is built.
    pub fn new(sample_rate: f32, units: Units, value: f64, frame: u64) -> Self {
        Signal { param: ToneParam::new(Self::offset(sample_rate), units, Some(value), frame), out: [0.0; Q] }
    }

    /// `new Signal({ value, units, convert: false })`.
    pub fn unconverted(sample_rate: f32, units: Units, value: f64, frame: u64) -> Self {
        let mut param = ToneParam::new(Self::offset(sample_rate), units, None, frame).without_conversion();
        param.reset(Some(value), frame);
        Signal { param, out: [0.0; Q] }
    }

    /// ConstantSourceNode.offset: a-rate, default 1, the whole float range.
    fn offset(sample_rate: f32) -> AudioParam {
        AudioParam::new(sample_rate as f64, 1.0, f32::MIN, f32::MAX, Rate::A)
    }

    /// Back to a freshly built signal with `value`.
    pub fn reset(&mut self, value: f64, frame: u64) {
        self.param.reset(Some(value), frame);
    }

    /// Render the quantum at `q` (once per quantum, in order); `input` sums into the offset once
    /// something is connected to it.
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

    /// `Signal.rampTo(value, rampTime)` at Tone's `now`.
    pub fn ramp_to(&mut self, value: f64, ramp_time: f64, now: f64, frame: u64) {
        self.param.ramp_to(value, ramp_time, now, frame);
    }

    /// `Signal.setValueAtTime(value, time)`.
    pub fn set_value_at_time(&mut self, value: f64, time: f64, frame: u64) {
        self.param.set_value_at_time(value, time, frame);
    }
}

/// `connectSignal(signal, param)` on a native param: cancel its schedule, set 0 at time 0, sum in.
pub fn connect_signal(param: &mut AudioParam, frame: u64) {
    param.cancel_scheduled_values(0.0, frame);
    param.set_value_at_time(0.0, 0.0, frame);
    param.set_connected(true);
}

/// Blink's WaveShaperNode with a Tone mapping (`setMap`, 1024 points by default).
pub struct WaveShaper {
    curve: Vec<f32>,
}

impl WaveShaper {
    /// `new WaveShaper({ mapping, length })`: `mapping(x)` at `length` points over -1..1, evaluated in
    /// double and stored as float.
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

    /// `WaveShaperCurveValues` on a quantum.
    pub fn process(&self, input: &[f32; Q], out: &mut [f32; Q]) {
        for (o, &x) in out.iter_mut().zip(input) {
            *o = self.shape(x);
        }
    }

    /// `WaveShaperCurveValues` on one value, the SSE path: float index arithmetic, then
    /// `v1 + f * (v2 - v1)`.
    pub fn shape(&self, x: f32) -> f32 {
        let curve = &self.curve[..];
        let max_index = (curve.len() - 1) as i32;
        let high = max_index as f32;
        let scale = (0.5 * (curve.len() - 1) as f64) as f32;
        let v = (x + 1.0) * scale;
        // Vclip: max(0, min(high, v)) with SSE's operand order.
        let v = if high < v { high } else { v };
        let v = if 0.0 > v { 0.0 } else { v };
        let index1 = v as i32;
        let v1 = curve[index1.clamp(0, max_index) as usize];
        let v2 = curve[(index1.wrapping_add(1)).clamp(0, max_index) as usize];
        let f = v - index1 as f32;
        f * (v2 - v1) + v1
    }
}

/// Tone's `Scale`: `input * (max - min) + min`, in Blink's float arithmetic (the Multiply's gain, then
/// the Add's sum; its unity Gain copies).
pub struct Scale {
    /// The Multiply: a Gain at `max - min`.
    mult: GainNode,
    /// The Add's addend: a Signal of `min`.
    add: Signal,
    scaled: [[f32; Q]; 1],
    out: [f32; Q],
}

impl Scale {
    /// `new Scale({ min, max })`, built while `frame` renders.
    pub fn new(sample_rate: f32, min: f64, max: f64, frame: u64) -> Self {
        let mut mult = GainNode::new(sample_rate as f64, 1.0, Units::Gain, frame);
        mult.gain.set_value_at_time(max - min, 0.0, frame);
        Scale { mult, add: Signal::new(sample_rate, Units::Number, min, frame), scaled: [[0.0; Q]], out: [0.0; Q] }
    }

    /// The `min`/`max` setters (`_setRange`) at Tone's `now`: the Add's value, then the Multiply's.
    pub fn set_range(&mut self, min: f64, max: f64, now: f64, frame: u64) {
        self.add.param.set_value(min, now, frame);
        self.mult.gain.set_value(max - min, now, frame);
    }

    /// Render the quantum at `q` from `input` (`None` when silent).
    pub fn process(&mut self, q: u64, input: Option<&[f32; Q]>) -> &[f32; Q] {
        self.mult.process(q, input.map(std::slice::from_ref), &mut self.scaled);
        let add = self.add.process(q, None);
        for ((o, &x), &a) in self.out.iter_mut().zip(&self.scaled[0]).zip(add) {
            *o = x + a;
        }
        &self.out
    }
}
