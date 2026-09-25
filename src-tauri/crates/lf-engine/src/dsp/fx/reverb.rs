//! The reverb: each track's send gain into the shared bus ([`ReverbSendFx`], bypassed = gain 0), and
//! the bus itself ([`ReverbBus`]).
//!
//! A send at gain 0 contributes exactly nothing to the mix: the GainNode multiplies by 0 while its
//! gain is automated and outputs silence after, and the reverb of silence is silence.
//!
//! The bus is `makeReverbBus` (`src/audio/fx/fx.ts`): a Gain of 1 summing every track's send into
//! Tone's `Reverb` (`effect/Reverb.js` on `Effect.js`) at wet 1, whose output goes to the master. What
//! Tone builds: the input fans to the dry/wet CrossFade's `a` and, through `effectSend`, to a Blink
//! ConvolverNode ([`Convolver`], `normalize` on) whose stereo output returns through `effectReturn`
//! to `b`. Every Gain on the way is 1 (an exact copy), so the bus is the CrossFade over the input and
//! the convolver. At wet 1 the CrossFade still passes `a` at cos(π/2) ≈ 6e−17 (see the crossfade
//! module); the bus keeps that leak, as it keeps every other one.
//!
//! The IR is [`reverb_ir::generate`](crate::dsp::reverb_ir::generate) with [`REVERB_DECAY`] and
//! [`REVERB_PRE_DELAY`] and the bus's own `Math.random` draws (that module says which), built off the
//! audio thread and handed to [`ReverbBus::new`]. Tone generates the IR asynchronously and the live
//! bus is silent until it is set; the engine builds the bus with its IR, so it sounds from the start.

use super::{Ctl, FxState, RAMP};
use crate::dsp::convolver::Convolver;
use crate::dsp::crossfade::CrossFade;
use crate::dsp::param::{AudioParam, Rate, ToneParam, Units, QUANTUM};

/// The bus reverb's decay and pre-delay in seconds (`REVERB_DECAY_SECONDS`,
/// `REVERB_PRE_DELAY_SECONDS` in `src/audio/fx/metadata.ts`).
pub const REVERB_DECAY: f64 = 2.6;
pub const REVERB_PRE_DELAY: f64 = 0.02;

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

    /// The send outputs silence this quantum.
    pub fn is_silent(&self) -> bool {
        self.silent
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

/// The shared reverb bus: Tone's `Reverb` at wet 1 over the sum of the tracks' sends.
pub struct ReverbBus {
    convolver: Convolver,
    dry_wet: CrossFade,
    wet: [[f32; QUANTUM]; 2],
    /// The quantum whose CrossFade gains are computed.
    prepared: Option<u64>,
}

impl ReverbBus {
    /// `makeReverbBus` built while `frame` renders, its convolver holding `ir` (see the module doc).
    /// Allocates: build it off the audio thread.
    pub fn new(sample_rate: f32, ir: [&[f32]; 2], frame: u64) -> Self {
        // Effect: the CrossFade at Tone's default fade 0.5, then `wet.setValueAtTime(1, 0)`.
        let mut dry_wet = CrossFade::new(sample_rate, 0.5, frame);
        dry_wet.fade.set_value_at_time(1.0, 0.0, frame);
        ReverbBus { convolver: Convolver::new(ir, sample_rate, true), dry_wet, wet: [[0.0; QUANTUM]; 2], prepared: None }
    }

    /// Render frames `frame..frame + left.len()`: blocks follow each other, none crosses a quantum
    /// (the send's silence is per quantum). `input` is the sum of every track's send for the same
    /// frames, rendered first, or `None` when every chain's
    /// [`send_silent`](super::FxChain::send_silent) holds.
    pub fn process(&mut self, frame: u64, input: Option<&[f32]>, left: &mut [f32], right: &mut [f32]) {
        let n = left.len();
        debug_assert!(right.len() == n && input.is_none_or(|x| x.len() == n));
        let quantum_start = frame - frame % QUANTUM as u64;
        if self.prepared != Some(quantum_start) {
            self.dry_wet.begin_quantum(quantum_start);
            self.prepared = Some(quantum_start);
        }
        let at = (frame - quantum_start) as usize;
        let [wet_l, wet_r] = &mut self.wet;
        let silent = self.convolver.process(frame, input, &mut wet_l[..n], &mut wet_r[..n]);
        let (b_l, b_r) = if silent { (None, None) } else { (Some(&wet_l[..n]), Some(&wet_r[..n])) };
        self.dry_wet.process(at, input, b_l, left);
        self.dry_wet.process(at, input, b_r, right);
    }
}
