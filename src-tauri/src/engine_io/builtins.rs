//! The chain host's built-in factory ([`Builtins`]): ECHO, REVERB, RING MOD, OTT and SYNTH from
//! `lf_engine::units`, built at the engine's rate with their params applied, for `Chains::new`
//! (`chain_cmds.rs`). It runs on the chain host's command threads and a built-in owner's thread, never
//! on the callback (invariant 4): every constructor allocates.
//!
//! **Params apply as the engine applies a `SetUnitParam`** (`docs/unit-contract.md` § 2): in their
//! order, each through the unit's own `set_param`, so a value is clamped (ECHO's level sent as 2 reads
//! 1) or refused (a param of another kind, a value that is not finite, a SYNTH `instrument` outside
//! 0..=5) exactly where the engine would. A refused value is dropped, as the engine drops it. Then every
//! param of the kind the asked ones left unset is applied at its default (`InputSendParam::range`, a
//! SYNTH level 1, a SYNTH pick 0: a SYNTH added with no pick plays Lead, as the browser fake and the
//! plate name read it), so the answer is every param of the kind as the unit took it, in
//! `UnitParam::ALL`'s order: what the plate shows and what a rebuild at another rate builds again from.

use lf_engine::grid::Meter;
use lf_engine::units::{EchoUnit, OttUnit, ReverbUnit, RingUnit, SynthUnit};
use lf_engine::{InputSend, UnitCx, UnitKind, UnitParam};

use super::unit_host::{BuiltinFactory, Params, Terms, Unit};

/// The production [`BuiltinFactory`] (the module doc).
pub(crate) struct Builtins;

impl BuiltinFactory for Builtins {
    fn build(&self, kind: UnitKind, params: &Params, terms: Terms) -> Result<(Unit, Params), String> {
        let rate = terms.rate;
        let mut unit: Unit = match kind {
            UnitKind::Echo => Box::new(EchoUnit::new(rate)),
            UnitKind::Reverb => Box::new(ReverbUnit::new(rate)),
            UnitKind::Ring => Box::new(RingUnit::new(rate)),
            UnitKind::Ott => Box::new(OttUnit::new(rate)),
            UnitKind::Synth => Box::new(SynthUnit::new(rate)),
            UnitKind::Plugin => return Err("a plugin is not a built-in unit".to_string()),
        };
        // Before the unit's first frame: a ramp a value starts here has ended long before any engine
        // frame it renders, except in an engine's first 20 ms.
        let cx = UnitCx { frame: 0, dsp_frame: 0, bpm: 120.0, meter: Meter::FOUR_FOUR };
        let mut applied = [None; UnitParam::COUNT];
        for &(param, value) in params {
            if let Some(took) = unit.set_param(&cx, param, value) {
                applied[param.index()] = Some(took);
            }
        }
        let mut answer = Params::new();
        for param in UnitParam::ALL.into_iter().filter(|&p| belongs(kind, p)) {
            let took = match applied[param.index()] {
                Some(took) => took,
                None => unit
                    .set_param(&cx, param, default(param))
                    .ok_or_else(|| format!("{kind:?} refused its default {}", param.key()))?,
            };
            answer.push((param, took));
        }
        Ok((unit, answer))
    }
}

/// Whether `param` is one of `kind`'s.
fn belongs(kind: UnitKind, param: UnitParam) -> bool {
    match param {
        UnitParam::Send(p) => matches!(
            (kind, p.send()),
            (UnitKind::Echo, InputSend::Echo)
                | (UnitKind::Reverb, InputSend::Reverb)
                | (UnitKind::Ring, InputSend::Ring)
                | (UnitKind::Ott, InputSend::Ott)
        ),
        UnitParam::Instrument | UnitParam::Gain(_) => kind == UnitKind::Synth,
    }
}

/// A param's value when the add does not set it: an effect's as `InputSendParam::range` has it (the
/// browser fake's `PARAMS`), a SYNTH level 1, a SYNTH pick 0 (Lead).
fn default(param: UnitParam) -> f64 {
    match param {
        UnitParam::Send(p) => p.range().2,
        UnitParam::Instrument => 0.0,
        UnitParam::Gain(_) => 1.0,
    }
}
