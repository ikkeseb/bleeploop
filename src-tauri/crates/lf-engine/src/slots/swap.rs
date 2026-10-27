//! A slot's hot swap ([`crate::SlotPort::replace`]): its state and the slot's methods that run it. The
//! rules: [`super`]'s module doc.

use super::{crossfade, slot_gain, Seat, Slot};
use crate::api::{HandbackReason, Rejection, SlotKind, SlotProcessor, SlotReply};
use crate::grid::Frame;

/// The candidate's processing before it can commit, past its latency.
pub(super) const STAGE_SECONDS: f64 = 0.010;
/// The crossfade from the replaced unit to its replacement.
pub(super) const SWAP_SECONDS: f64 = 0.010;

/// What the unit in a slot's `side` seat is doing. Set before a unit takes the seat (one that panics as
/// it arrives is staging already) and `Idle` again once it has left; what a seat holds is the seat's
/// alone to say, so a hand-back reads the seat, never this.
pub(super) enum Swap {
    /// No swap: the slot plays its `seat` alone.
    Idle,
    /// The candidate in `side` has processed `done` frames, unheard ([`Slot::commit_when_ready`] says
    /// how many it needs), then plays at `gain`.
    Staging { done: Frame, gain: f32 },
    /// The unit in `side` fades out under the one the slot plays, `step` frames into the swap fade, at
    /// the slot's `gain` as it stood at the commit.
    Fading { step: u32, gain: f64 },
}

impl Swap {
    /// This frame's step into the swap fade (of `n`) and the outgoing unit's gain, the step then
    /// advanced; `None` outside the fade, its end included.
    pub(super) fn step(&mut self, n: u32) -> Option<(u32, f64)> {
        match self {
            Swap::Fading { step, gain } if *step < n => {
                let at = *step;
                *step += 1;
                Some((at, *gain))
            }
            _ => None,
        }
    }
}

impl Seat {
    /// The unit's output at frame `k` at gain `g`, on its destination: (wet, bus).
    fn term(&self, k: usize, g: f64) -> (f32, f32) {
        let y = (g * self.out[k] as f64) as f32;
        if self.kind == SlotKind::Instrument {
            (0.0, y)
        } else {
            (y, 0.0)
        }
    }

    /// The latency the deferred commit compares: an empty seat's is 0.
    fn playing_latency(&self) -> Frame {
        if self.unit.is_some() {
            self.latency
        } else {
            0
        }
    }
}

impl Slot {
    /// A replace: the candidate takes the side seat and stages there, for its latency plus `stage`
    /// frames beside a playing unit; into an empty slot it is ready at once, an install whose gain glides
    /// to `gain` (held, as any candidate, while a take is `capturing` and its latency is not the slot's).
    /// A slot already swapping, or whose unit is being removed, sends it straight back `Busy`, counted,
    /// never started or stopped.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn replace(&mut self, tag: u64, unit: Box<dyn SlotProcessor>, gain: f32, fade: u32, stage: Frame, capturing: bool, dropped: &mut u64, errors: &mut u64) {
        if self.removing || self.side.unit.is_some() {
            *errors += 1;
            self.end.hand_back(tag, HandbackReason::Rejected(Rejection::Busy), unit, errors);
            return;
        }
        self.swap = Swap::Staging { done: 0, gain };
        self.side.take(tag, unit);
        if self.seat.unit.is_none() {
            // An install: it changes no live flag.
            self.commit_when_ready(stage, capturing, fade, None, dropped, errors);
        }
    }

    /// A withdraw for `tag`: the candidate staging under it goes back `Cancelled`. Any other tag (a
    /// committed unit's, an outgoing one's, an unknown one) gets no answer and changes nothing.
    pub(super) fn withdraw(&mut self, tag: u64, errors: &mut u64) {
        if matches!(self.swap, Swap::Staging { .. }) && self.side.tag == tag {
            self.cancel(errors);
        }
    }

    /// A staging candidate, if any, goes back `Cancelled`: the swap is off.
    pub(super) fn cancel(&mut self, errors: &mut u64) {
        if matches!(self.swap, Swap::Staging { .. }) {
            self.retire_side(HandbackReason::Rejected(Rejection::Cancelled), errors);
        }
    }

    /// Whether `tag` names the unit fading out after a swap: leaving already, it gets no answer.
    pub(super) fn fading_out(&self, tag: u64) -> bool {
        matches!(self.swap, Swap::Fading { .. }) && self.side.tag == tag
    }

    /// A staging candidate commits once it has processed its latency plus `stage` frames (into an empty
    /// slot, none: nothing plays to hide its start behind), unless a take is `capturing` and its latency
    /// differs from the playing unit's. An install into an empty slot starts `fade` frames into its own;
    /// a live-off the commit makes ramps as `live` says ([`Slot::commit`]).
    pub(super) fn commit_when_ready(&mut self, stage: Frame, capturing: bool, fade: u32, live: Option<(Frame, Frame)>, dropped: &mut u64, errors: &mut u64) {
        let Swap::Staging { done, .. } = self.swap else { return };
        let need = if self.seat.unit.is_some() { self.side.latency + stage } else { 0 };
        if done >= need && !(capturing && self.side.playing_latency() != self.seat.playing_latency()) {
            self.commit(fade, live, dropped, errors);
        }
    }

    /// The candidate becomes the unit the slot plays, `Committed`. Into an empty slot that is an install
    /// (in from bypass, `fade` frames done) whose gain glides to the replace's; beside a playing unit, the
    /// old one's notes are released, the seats trade places, the old one fades out at the gain the slot
    /// had, and the slot's gain is the replace's at once. A live slot whose unit changes kind goes off
    /// live there, as a GO LIVE off would: ramped over `live`'s frames from its frame, settled without,
    /// and owed its answer (`live_off`).
    fn commit(&mut self, fade: u32, live: Option<(Frame, Frame)>, dropped: &mut u64, errors: &mut u64) {
        let Swap::Staging { gain, .. } = self.swap else { return };
        let empty = self.seat.unit.is_none();
        self.seat.release_all(0, dropped);
        std::mem::swap(&mut self.seat, &mut self.side);
        self.gain_target = slot_gain(gain);
        let live_off = !empty && self.live && self.seat.kind != self.side.kind;
        if empty {
            self.swap = Swap::Idle;
            self.engage(fade);
        } else {
            self.swap = Swap::Fading { step: 0, gain: self.gain };
            self.gain = self.gain_target;
        }
        if live_off {
            self.set_live(false, live);
            self.live_off = true;
        }
        self.end.send(SlotReply::Committed { tag: self.seat.tag }, errors);
    }

    /// No block will render the swap (idle servicing, an eviction): a staging candidate commits (an
    /// install into an empty slot `fade` frames in), and whatever the side seat then holds, the unit it
    /// replaced, goes back `ReplacedOut`, its released notes carried by one silent frame from `frame`.
    /// A kind change's live-off is settled.
    pub(super) fn settle_swap(&mut self, frame: Frame, zeros: &[f32], fade: u32, dropped: &mut u64, errors: &mut u64) {
        self.commit(fade, None, dropped, errors);
        self.side.flush(frame, zeros);
        self.retire_side(HandbackReason::ReplacedOut, errors);
    }

    /// The side seat's unit, if any, goes back, stopped, with `reason`, and the swap is over.
    pub(super) fn retire_side(&mut self, reason: HandbackReason, errors: &mut u64) {
        self.side.hand_back(&mut self.end, reason, errors);
        self.swap = Swap::Idle;
    }

    /// Frame `k` of the swap fade, `at` its step and the outgoing unit's gain: the outgoing unit
    /// crossfades into the one the slot plays (at the slot's gain) over `n` frames, as (wet, bus).
    pub(super) fn crossing(&self, k: usize, at: (u32, f64), n: u32) -> (f32, f32) {
        let (step, gain) = at;
        let (old_wet, old_bus) = self.side.term(k, gain);
        let (wet, bus) = self.seat.term(k, self.gain);
        (crossfade(old_wet, wet, step, n), crossfade(old_bus, bus, step, n))
    }

    /// After a render call of `m` frames: a staging candidate counts them, and goes back `StartRefused` if
    /// it refused to start; a swap fade that reached its end (of `n`) hands the old unit back.
    pub(super) fn swap_after_call(&mut self, m: usize, n: u32, errors: &mut u64) {
        match &mut self.swap {
            Swap::Staging { done, .. } => {
                *done += m as Frame;
                if self.side.unit.as_ref().is_some_and(|unit| unit.start_refused()) {
                    self.retire_side(HandbackReason::Rejected(Rejection::StartRefused), errors);
                }
            }
            Swap::Fading { step, .. } if *step >= n => self.retire_side(HandbackReason::ReplacedOut, errors),
            _ => {}
        }
    }
}
