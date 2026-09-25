//! The per-track FX chain, ported from `src/audio/fx/fx.ts` (`FxChain` and its five nodes) and its
//! pure metadata `src/audio/fx/metadata.ts`, as Tone 15.1.22 builds it on Blink:
//!
//! ```text
//! input → Filter → Pitch → Stutter → Delay → out
//!                                         └→ reverb send ─┐
//! every track's send ─────────────────────────────────────┴→ ReverbBus (stereo) → master
//! ```
//!
//! The chain renders its output and its send; the caller sums the sends into the one shared
//! [`ReverbBus`] (see the reverb module).
//!
//! Every node is built at construction and always renders, as in Tone: a bypassed Filter, Pitch or
//! Stutter is a [`CrossFade`](super::crossfade::CrossFade) at fade 0, which still mixes its wet path in
//! at −56 dB (see the crossfade module), and a bypassed Delay is its dry/wet CrossFade at wet 0, which
//! does the same with its echoes. Only an effect Tone has not built yet stays out: the PitchShift, which
//! fx.ts builds on the pitch's first enable and renders from then on, bypassed or not.
//!
//! # Control timing
//!
//! A control call ([`FxChain::set_param`], [`FxChain::set_bypass`], [`FxChain::set_state`],
//! [`FxChain::set_timing`]) takes a [`Ctl`]: Tone's `now` in seconds, and the frame being rendered,
//! which fixes Blink's context time (`param::context_frame`: the next quantum not yet computed). Param
//! ramps start at `now` (Blink moves an event that falls before the quantum it next renders to that
//! quantum's start); the stutter restarts its gate at the context time, where fx.ts reads the native
//! `currentTime`. The engine calls with [`Ctl::at`] on the frame a control lands on, between the two
//! halves of a split block; Tone's live lookahead and the stutter's 256-frame live lead exist to cover
//! main-thread jitter, which the engine does not have, so neither is kept. An offline replay can
//! schedule ahead instead, as Tone's offline context does: every tick's calls run before rendering
//! starts, at `now` = the tick's time and frame 0.
//!
//! [`FxChain::process`] renders any block split: every node computes its control values for a quantum
//! when the quantum's first frame renders and then runs frame by frame, so a block size changes
//! nothing but where the work happens. It never allocates.

mod delay;
mod filter;
mod pitch;
mod reverb;
mod stutter;

pub use delay::DelayFx;
pub use filter::FilterFx;
pub use pitch::PitchFx;
pub use reverb::{ReverbBus, ReverbSendFx, REVERB_DECAY, REVERB_PRE_DELAY};
pub use stutter::StutterFx;

use super::param::{self, QUANTUM};

/// Every bypass and param transition ramps over 20 ms (`RAMP`).
pub const RAMP: f64 = 0.02;
/// The delay's self-oscillation guard (`MAX_FEEDBACK`).
pub const MAX_FEEDBACK: f64 = 0.95;
/// The note divisions of the tempo-synced params (`FX_DIVISIONS`).
pub const DIVISIONS: [&str; 4] = ["4n", "8n", "8n.", "16n"];
const DIVISION_LABELS: [&str; 4] = ["1/4", "1/8", "1/8.", "1/16"];

/// A division's length in beats (`divisionBeats`: 4 over the note value, times 1.5 when dotted).
pub fn division_beats(index: usize) -> f64 {
    const NOTE: [f64; 4] = [4.0, 8.0, 8.0, 16.0];
    let dotted = DIVISIONS[index].ends_with('.');
    (4.0 / NOTE[index]) * if dotted { 1.5 } else { 1.0 }
}

/// `clampIndex`: JS `Math.round`, then clamped into `0..length`.
pub fn clamp_index(value: f64, length: usize) -> usize {
    ((value + 0.5).floor()).clamp(0.0, (length - 1) as f64) as usize
}

/// The five effects, in chain (and serialized) order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FxKind {
    Filter,
    Pitch,
    Stutter,
    Delay,
    Reverb,
}

impl FxKind {
    pub const ALL: [FxKind; 5] = [FxKind::Filter, FxKind::Pitch, FxKind::Stutter, FxKind::Delay, FxKind::Reverb];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        ["Filter", "Pitch", "Stutter", "Delay", "Reverb"][self.index()]
    }

    /// `FX_PARAM_DEFS[kind]`, in serialization order.
    pub fn params(self) -> &'static [FxParamDef] {
        FX_PARAM_DEFS[self.index()]
    }
}

/// One FX parameter, by kind and key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FxParam {
    Cutoff,
    Q,
    Semitones,
    Rate,
    Time,
    Feedback,
    Mix,
    Amount,
}

impl FxParam {
    pub fn kind(self) -> FxKind {
        match self {
            FxParam::Cutoff | FxParam::Q => FxKind::Filter,
            FxParam::Semitones => FxKind::Pitch,
            FxParam::Rate => FxKind::Stutter,
            FxParam::Time | FxParam::Feedback | FxParam::Mix => FxKind::Delay,
            FxParam::Amount => FxKind::Reverb,
        }
    }

    /// Its position in its kind's defs (and in [`FxState::params`]).
    pub fn index(self) -> usize {
        match self {
            FxParam::Q | FxParam::Feedback => 1,
            FxParam::Mix => 2,
            _ => 0,
        }
    }

    pub fn def(self) -> &'static FxParamDef {
        &self.kind().params()[self.index()]
    }

    /// The param a kind's `key` names (`setParam`'s string keys).
    pub fn from_key(kind: FxKind, key: &str) -> Option<FxParam> {
        let i = kind.params().iter().position(|d| d.key == key)?;
        Some(Self::of(kind, i))
    }

    fn of(kind: FxKind, index: usize) -> FxParam {
        match (kind, index) {
            (FxKind::Filter, 0) => FxParam::Cutoff,
            (FxKind::Filter, _) => FxParam::Q,
            (FxKind::Pitch, _) => FxParam::Semitones,
            (FxKind::Stutter, _) => FxParam::Rate,
            (FxKind::Delay, 0) => FxParam::Time,
            (FxKind::Delay, 1) => FxParam::Feedback,
            (FxKind::Delay, _) => FxParam::Mix,
            (FxKind::Reverb, _) => FxParam::Amount,
        }
    }
}

/// `FxParamDef`: the range, key and choice contract for UI, audio and import.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FxParamDef {
    pub key: &'static str,
    pub label: &'static str,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub default: f64,
    pub unit: Option<&'static str>,
    pub integer: bool,
    /// For index-style params: the value is an index into this list.
    pub choices: Option<&'static [&'static str]>,
}

const fn def(key: &'static str, label: &'static str, min: f64, max: f64, step: f64, default: f64) -> FxParamDef {
    FxParamDef { key, label, min, max, step, default, unit: None, integer: false, choices: None }
}

const FILTER_PARAMS: [FxParamDef; 2] =
    [FxParamDef { unit: Some("Hz"), ..def("cutoff", "Cutoff", 120.0, 14000.0, 1.0, 1200.0) }, def("q", "Reso", 0.1, 14.0, 0.1, 2.0)];
const PITCH_PARAMS: [FxParamDef; 1] =
    [FxParamDef { unit: Some("st"), integer: true, ..def("semitones", "Pitch", -12.0, 12.0, 1.0, 0.0) }];
const STUTTER_PARAMS: [FxParamDef; 1] =
    [FxParamDef { integer: true, choices: Some(&DIVISION_LABELS), ..def("rate", "Rate", 0.0, 3.0, 1.0, 1.0) }];
const DELAY_PARAMS: [FxParamDef; 3] = [
    FxParamDef { integer: true, choices: Some(&DIVISION_LABELS), ..def("time", "Time", 0.0, 3.0, 1.0, 1.0) },
    def("feedback", "Fbk", 0.0, 0.95, 0.01, 0.4),
    def("mix", "Mix", 0.0, 1.0, 0.01, 0.3),
];
const REVERB_PARAMS: [FxParamDef; 1] = [def("amount", "Send", 0.0, 1.0, 0.01, 0.3)];

/// `FX_PARAM_DEFS`, indexed by [`FxKind::index`].
pub const FX_PARAM_DEFS: [&[FxParamDef]; 5] = [&FILTER_PARAMS, &PITCH_PARAMS, &STUTTER_PARAMS, &DELAY_PARAMS, &REVERB_PARAMS];

/// Params one effect has at most.
pub const MAX_PARAMS: usize = 3;

/// `FxState`: bypass and params in the kind's def order (unused slots stay 0).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FxState {
    pub bypassed: bool,
    pub params: [f64; MAX_PARAMS],
}

impl FxState {
    /// `defaultStateFor(kind)`: bypassed, params at their defaults.
    pub fn default_for(kind: FxKind) -> FxState {
        let mut params = [0.0; MAX_PARAMS];
        for (p, d) in params.iter_mut().zip(kind.params()) {
            *p = d.default;
        }
        FxState { bypassed: true, params }
    }
}

/// `defaultFxStates()`: five bypassed effects in chain order.
pub fn default_fx_states() -> [FxState; 5] {
    FxKind::ALL.map(FxState::default_for)
}

/// `FxTiming`: the looper's context-time grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FxTiming {
    pub anchor: f64,
    pub beat_period: f64,
}

/// `setTiming`'s refusal: the anchor must be finite and the beat period positive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidTiming;

/// When a control call lands: Tone's `now` (seconds) and the frame being rendered (Blink's context
/// time is that frame rounded up to a quantum). See the module doc.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ctl {
    pub now: f64,
    pub frame: u64,
}

impl Ctl {
    /// A live control landing on `frame`.
    pub fn at(frame: u64, sample_rate: f32) -> Ctl {
        Ctl { now: frame as f64 / sample_rate as f64, frame }
    }

    /// Blink's context time for this call: what fx.ts reads as the native `currentTime`.
    pub fn context_time(&self, sample_rate: f32) -> f64 {
        param::context_time(self.frame, sample_rate as f64)
    }
}

/// The per-track chain on a mono signal.
pub struct FxChain {
    pub filter: FilterFx,
    pub pitch: PitchFx,
    pub stutter: StutterFx,
    pub delay: DelayFx,
    pub reverb: ReverbSendFx,
    /// The quantum whose control values are computed.
    prepared: Option<u64>,
    a: [f32; QUANTUM],
    b: [f32; QUANTUM],
}

impl FxChain {
    /// `new FxChain(initial)`: every node built (Tone's constructor order), then `setState(initial)`.
    pub fn new(sample_rate: f32, initial: Option<&[FxState; 5]>, ctl: Ctl) -> Self {
        let mut chain = FxChain {
            filter: FilterFx::new(sample_rate, ctl),
            pitch: PitchFx::new(sample_rate, ctl),
            stutter: StutterFx::new(sample_rate, ctl),
            delay: DelayFx::new(sample_rate, ctl),
            reverb: ReverbSendFx::new(sample_rate, ctl),
            prepared: None,
            a: [0.0; QUANTUM],
            b: [0.0; QUANTUM],
        };
        if let Some(states) = initial {
            chain.set_state(states, ctl);
        }
        chain
    }

    /// `getState()`: the five states in chain order.
    pub fn get_state(&self) -> [FxState; 5] {
        [self.filter.get_state(), self.pitch.get_state(), self.stutter.get_state(), self.delay.get_state(), self.reverb.get_state()]
    }

    /// `setState(states)`: per node, every param in def order, then the bypass.
    pub fn set_state(&mut self, states: &[FxState; 5], ctl: Ctl) {
        for kind in FxKind::ALL {
            let s = &states[kind.index()];
            for i in 0..kind.params().len() {
                self.set_param(FxParam::of(kind, i), s.params[i], ctl);
            }
            self.set_bypass(kind, s.bypassed, ctl);
        }
    }

    pub fn set_param(&mut self, param: FxParam, value: f64, ctl: Ctl) {
        match param.kind() {
            FxKind::Filter => self.filter.set_param(param, value, ctl),
            FxKind::Pitch => self.pitch.set_param(value, ctl),
            FxKind::Stutter => self.stutter.set_param(value, ctl),
            FxKind::Delay => self.delay.set_param(param, value, ctl),
            FxKind::Reverb => self.reverb.set_param(value, ctl),
        }
    }

    pub fn param(&self, param: FxParam) -> f64 {
        match param.kind() {
            FxKind::Filter => self.filter.get_param(param),
            FxKind::Pitch => self.pitch.get_param(),
            FxKind::Stutter => self.stutter.get_param(),
            FxKind::Delay => self.delay.get_param(param),
            FxKind::Reverb => self.reverb.get_param(),
        }
    }

    pub fn set_bypass(&mut self, kind: FxKind, bypassed: bool, ctl: Ctl) {
        match kind {
            FxKind::Filter => self.filter.set_bypass(bypassed, ctl),
            FxKind::Pitch => self.pitch.set_bypass(bypassed, ctl),
            FxKind::Stutter => self.stutter.set_bypass(bypassed, ctl),
            FxKind::Delay => self.delay.set_bypass(bypassed, ctl),
            FxKind::Reverb => self.reverb.set_bypass(bypassed, ctl),
        }
    }

    pub fn is_bypassed(&self, kind: FxKind) -> bool {
        match kind {
            FxKind::Filter => self.filter.is_bypassed(),
            FxKind::Pitch => self.pitch.is_bypassed(),
            FxKind::Stutter => self.stutter.is_bypassed(),
            FxKind::Delay => self.delay.is_bypassed(),
            FxKind::Reverb => self.reverb.is_bypassed(),
        }
    }

    /// `setTiming(timing)`: the stutter's grid and the delay's beat.
    pub fn set_timing(&mut self, timing: FxTiming, ctl: Ctl) -> Result<(), InvalidTiming> {
        if !timing.anchor.is_finite() || !timing.beat_period.is_finite() || timing.beat_period <= 0.0 {
            return Err(InvalidTiming);
        }
        self.stutter.set_timing(timing, ctl);
        self.delay.set_timing(timing, ctl);
        Ok(())
    }

    /// The send is silent in the quantum last rendered (its gain is 0). A quantum in which every
    /// chain's send is silent reaches the [`ReverbBus`] as `None`.
    pub fn send_silent(&self) -> bool {
        self.reverb.is_silent()
    }

    fn begin_quantum(&mut self, quantum_start: u64) {
        self.filter.begin_quantum(quantum_start);
        self.pitch.begin_quantum(quantum_start);
        self.stutter.begin_quantum(quantum_start);
        self.delay.begin_quantum(quantum_start);
        self.reverb.begin_quantum(quantum_start);
        self.prepared = Some(quantum_start);
    }

    /// Render `input` (frames `frame..frame + input.len()`) into `out` (the chain's output, the dry
    /// path to the master) and `send` (the reverb bus's input from this track).
    pub fn process(&mut self, frame: u64, input: &[f32], out: &mut [f32], send: &mut [f32]) {
        debug_assert!(input.len() == out.len() && out.len() == send.len());
        let q = QUANTUM as u64;
        let mut done = 0;
        while done < input.len() {
            let f = frame + done as u64;
            let quantum_start = f - f % q;
            if self.prepared != Some(quantum_start) {
                self.begin_quantum(quantum_start);
            }
            let at = (f - quantum_start) as usize;
            let n = (QUANTUM - at).min(input.len() - done);
            let span = done..done + n;
            let (a, b) = (&mut self.a[..n], &mut self.b[..n]);
            self.filter.process(at, &input[span.clone()], a);
            self.pitch.process(at, a, b);
            self.stutter.process(at, b, a);
            self.delay.process(at, a, &mut out[span.clone()]);
            self.reverb.process(at, &out[span.clone()], &mut send[span]);
            done += n;
        }
    }
}
