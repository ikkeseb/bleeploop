//! OWNS: how a plugin owner's unit enters its slot ([`enter`]): an install into the empty slot, or a
//! hot swap's replace beside the unit another owner plays (the engine stages it muted, commits and
//! crossfades: `lf_engine`'s slots doc), with the swap's own withdraw and the report each refusal gives.
//!
//! A replace's answer is read until [`SwapBounds`]' deadlines (a take recording at another latency
//! holds the candidate staged); called off (past `commit`, or by the caller's `cancel`), it is
//! withdrawn, and a commit that wins the race against the withdraw stands.

use std::sync::atomic::{AtomicBool, Ordering::Acquire};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lf_engine::{Rejection, SlotProcessor};

use super::REMOVE_TIMEOUT;
use crate::engine_io::slot_host::Answer;
use crate::engine_io::SlotHost;

/// How long a replace waits for its commit after the claim: the candidate's latency and the engine's
/// staging, with room for a slow device. (Tests wait 8 s, inside their 10 s `REMOVE_TIMEOUT`: a
/// `TestDevice`'s render thread can starve for 2 s on a loaded machine.)
const COMMIT_WAIT: Duration = Duration::from_millis(if cfg!(test) { 8_000 } else { 1_500 });

/// How long one read of a replace's answer waits.
const ANSWER_POLL: Duration = Duration::from_millis(10);

/// How an owner's unit enters its slot.
pub(crate) enum Entry {
    /// Into the empty slot.
    Install,
    /// Beside the unit another owner plays, crossfaded to at `gain`. `cancel` calls the swap off while it
    /// stages; set before the claim, nothing enters the engine.
    Replace { gain: f32, cancel: Arc<AtomicBool>, bounds: SwapBounds },
}

impl Entry {
    /// A swap its caller has called off.
    pub(super) fn cancelled(&self) -> bool {
        matches!(self, Entry::Replace { cancel, .. } if cancel.load(Acquire))
    }
}

/// A replace's deadlines, both counted from the claim, so its answer is settled inside the load's
/// post-claim `REMOVE_TIMEOUT` (`spawn_within`).
pub(crate) struct SwapBounds {
    /// Uncommitted by then, the owner withdraws the candidate.
    pub(crate) commit: Duration,
    /// Unanswered by then, the swap is unresolved.
    pub(crate) settle: Duration,
}

impl Default for SwapBounds {
    fn default() -> SwapBounds {
        SwapBounds { commit: COMMIT_WAIT, settle: REMOVE_TIMEOUT - Duration::from_millis(100) }
    }
}

/// Where a unit stands once [`enter`] returns.
pub(super) enum Entered {
    /// The slot's: installed, or committed (a commit after a withdraw included).
    Committed,
    /// Never the slot's: back with the load's error (stopped if it processed).
    Refused(Box<dyn SlotProcessor>, String),
    /// A replace the engine never answered: the unit may still be in it.
    Unresolved,
}

/// Put plugin `name`'s unit, activated at `rate`, into the slot as `entry` says, the load claimed at
/// `claimed_at`.
pub(super) fn enter(slot: &SlotHost, unit: Box<dyn SlotProcessor>, rate: u32, entry: &Entry, name: &str, claimed_at: Instant) -> Entered {
    let Entry::Replace { gain, cancel, bounds } = entry else {
        return match slot.install(unit, rate) {
            Ok(()) => Entered::Committed,
            Err((unit, e)) => Entered::Refused(unit, e),
        };
    };
    if let Err((unit, e)) = slot.replace(unit, rate, *gain) {
        return Entered::Refused(unit, e);
    }
    let (commit_by, settle_by) = (claimed_at + bounds.commit, claimed_at + bounds.settle);
    // Once the swap is called off: whether the withdraw went out (false: no engine, or the port is full).
    let mut withdrawn: Option<bool> = None;
    loop {
        match slot.answer(ANSWER_POLL.min(settle_by.saturating_duration_since(Instant::now()))) {
            Answer::Committed => return Entered::Committed,
            Answer::Refused(unit, why) => return Entered::Refused(unit, refusal(why, name, cancel.load(Acquire), withdrawn == Some(true))),
            Answer::Pending => {}
        }
        let now = Instant::now();
        if withdrawn.is_none() && (cancel.load(Acquire) || now >= commit_by) {
            withdrawn = Some(slot.withdraw());
        }
        if now >= settle_by {
            return Entered::Unresolved;
        }
    }
}

/// The load's error for a replace the engine refused, by why and by who called it off: the caller
/// (`cancelled`, whether or not the withdraw went out), this owner at its commit deadline (`withdrew`:
/// its withdraw went out), or neither (the playing unit was removed: its owner restarted or tore down).
/// A transient refusal starts `busy:`.
fn refusal(why: Rejection, name: &str, cancelled: bool, withdrew: bool) -> String {
    match why {
        Rejection::StartRefused => format!("{name} refused to start processing; the playing plugin stays"),
        Rejection::Cancelled if cancelled => "plugin load cancelled".to_string(),
        Rejection::Cancelled if withdrew => "busy: a take is recording at another latency; swap after it".to_string(),
        Rejection::Cancelled => "busy: the playing plugin restarted".to_string(),
        // `SlotHost::answer` logs it.
        Rejection::Busy => "busy: the slot is swapping or its plugin is being removed".to_string(),
        why => format!("{name} could not enter the slot ({why:?})"),
    }
}
