//! Tone's `Filter` (`component/filter/Filter.js`, Tone 15.1.22): 1 to 4 cascaded Blink biquads whose
//! frequency, Q, detune and gain follow four Tone Signals; and Tone's `Signal` (`signal/Signal.js` over
//! `signal/ToneConstantSource.js`), which the crossfade uses too.
//!
//! A Signal is a Blink ConstantSourceNode whose `offset` is a Tone [`ToneParam`]; ToneConstantSource
//! puts a gain envelope of exactly 1 after it from time 0, so its output is the offset's values. Tone
//! connects a Signal into a param with `connectSignal`: the param's own schedule is cancelled and set
//! to 0 at time 0, and the Signal's output sums in. So each biquad param here reads 0 plus its Signal,
//! and is always sample-accurate: Blink computes the biquad coefficients per frame whenever a Signal
//! moves inside a quantum (a ramp), once per quantum otherwise.
//!
//! Tone builds one BiquadFilterNode per cascade stage, each with the same four connections, so every
//! stage computes the same coefficients; the stages here do the same work Blink does. Tone's input and
//! output Gains are unity copies and are left out.

use super::biquad::{BiquadFilterNode, FilterType, ParamInputs};
use super::param::{AudioParam, Rate, ToneParam, Units, QUANTUM};

/// Tone's Signal: a ConstantSourceNode's offset param, rendered once per quantum.
pub struct Signal {
    pub param: ToneParam,
    values: [f32; QUANTUM],
}

impl Signal {
    /// `new Signal({ value, units, convert })`, built while `frame` renders.
    pub fn new(sample_rate: f64, units: Units, convert: bool, value: f64, frame: u64) -> Self {
        // ConstantSourceNode.offset: default 1, unbounded, a-rate.
        let native = AudioParam::new(sample_rate, 1.0, f32::MIN, f32::MAX, Rate::A);
        let mut param = ToneParam::new(native, units, None, frame);
        if !convert {
            param = param.without_conversion();
        }
        // Tone's Param constructor: a value other than the native default is set at time 0.
        param.reset(Some(value), frame);
        Signal { param, values: [0.0; QUANTUM] }
    }

    /// Render the quantum at `quantum_start` (ConstantSourceHandler::Process): the offset's a-rate
    /// values while it has sample-accurate ones, else its k-rate value. Call once per quantum.
    pub fn render(&mut self, quantum_start: u64) -> &[f32; QUANTUM] {
        self.param.native.fill(quantum_start, &mut self.values);
        &self.values
    }

    /// The quantum [`Signal::render`] last rendered.
    pub fn values(&self) -> &[f32; QUANTUM] {
        &self.values
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

/// Tone's rolloff choices: the number of cascaded biquads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rolloff {
    Db12 = 1,
    Db24 = 2,
    Db48 = 3,
    Db96 = 4,
}

const MAX_STAGES: usize = 4;

/// Tone's `Filter` on a mono signal.
pub struct Filter {
    pub frequency: Signal,
    pub q: Signal,
    pub detune: Signal,
    pub gain: Signal,
    stages: [BiquadFilterNode; MAX_STAGES],
    count: usize,
    scratch: [f32; QUANTUM],
}

impl Filter {
    /// `new Filter({ type, frequency, Q, rolloff })` with Tone's detune 0 and gain 0, built while
    /// `frame` renders.
    pub fn new(sample_rate: f32, kind: FilterType, frequency: f64, q: f64, rolloff: Rolloff, frame: u64) -> Self {
        let rate = sample_rate as f64;
        let q = Signal::new(rate, Units::Positive, true, q, frame);
        let frequency = Signal::new(rate, Units::Frequency, true, frequency, frame);
        let detune = Signal::new(rate, Units::Cents, true, 0.0, frame);
        let gain = Signal::new(rate, Units::Decibels, false, 0.0, frame);
        let mut stages = std::array::from_fn(|_| BiquadFilterNode::new(sample_rate, kind));
        for s in stages.iter_mut() {
            // Tone's BiquadFilter sets the node's params to its defaults (the native ones: nothing
            // is scheduled), then each Signal connects in.
            connect_signal(&mut s.frequency, frame);
            connect_signal(&mut s.detune, frame);
            connect_signal(&mut s.q, frame);
            connect_signal(&mut s.gain, frame);
        }
        Filter { frequency, q, detune, gain, stages, count: rolloff as usize, scratch: [0.0; QUANTUM] }
    }

    /// Render the Signals for the quantum at `quantum_start` and update every stage's coefficients.
    pub fn begin_quantum(&mut self, quantum_start: u64) {
        self.q.render(quantum_start);
        self.frequency.render(quantum_start);
        self.detune.render(quantum_start);
        self.gain.render(quantum_start);
        let inputs = ParamInputs {
            frequency: Some(self.frequency.values()),
            q: Some(self.q.values()),
            gain: Some(self.gain.values()),
            detune: Some(self.detune.values()),
        };
        for s in self.stages[..self.count].iter_mut() {
            s.begin_quantum(quantum_start, inputs);
        }
    }

    /// Filter frames `at..at + source.len()` of the current quantum through the cascade.
    pub fn process(&mut self, at: usize, source: &[f32], dest: &mut [f32]) {
        let n = source.len();
        self.stages[0].process(at, source, dest);
        for s in self.stages[1..self.count].iter_mut() {
            self.scratch[..n].copy_from_slice(dest);
            s.process(at, &self.scratch[..n], dest);
        }
    }
}
