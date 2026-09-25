//! Tone's `Filter` (`component/filter/Filter.js`, Tone 15.1.22): 1 to 4 cascaded Blink biquads whose
//! frequency, Q, detune and gain follow four Tone [`Signal`]s.
//!
//! Tone connects a Signal into a param with `connectSignal`: the param's own schedule is cancelled and
//! set to 0 at time 0, and the Signal's output sums in. So each biquad param here reads 0 plus its
//! Signal, and is always sample-accurate: Blink computes the biquad coefficients per frame whenever a
//! Signal moves inside a quantum (a ramp), once per quantum otherwise.
//!
//! Tone builds one BiquadFilterNode per cascade stage, each with the same four connections, so every
//! stage computes the same coefficients; the stages here do the same work Blink does. Tone's input and
//! output Gains are unity copies and are left out.

use super::biquad::{BiquadFilterNode, FilterType, ParamInputs};
use super::param::{Units, QUANTUM};
use super::signal::{connect_signal, Signal};

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
        let q = Signal::new(sample_rate, Units::Positive, q, frame);
        let frequency = Signal::new(sample_rate, Units::Frequency, frequency, frame);
        let detune = Signal::new(sample_rate, Units::Cents, 0.0, frame);
        let gain = Signal::unconverted(sample_rate, Units::Decibels, 0.0, frame);
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
        self.begin_quantum_driven(quantum_start, None);
    }

    /// [`Filter::begin_quantum`] with a signal connected into `frequency` (an envelope driving the
    /// cutoff: `connectSignal` overrides the Signal, see [`ToneParam::connect_signal`]).
    ///
    /// [`ToneParam::connect_signal`]: super::param::ToneParam::connect_signal
    pub fn begin_quantum_driven(&mut self, quantum_start: u64, frequency: Option<&[f32; QUANTUM]>) {
        self.q.process(quantum_start, None);
        self.frequency.process(quantum_start, frequency);
        self.detune.process(quantum_start, None);
        self.gain.process(quantum_start, None);
        let inputs = ParamInputs {
            frequency: Some(self.frequency.output()),
            q: Some(self.q.output()),
            gain: Some(self.gain.output()),
            detune: Some(self.detune.output()),
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
