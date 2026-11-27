//! The rings used by host threads and the observations handed to the shared feed.

use std::sync::Arc;
use lf_engine::{Event, Overview, SessionPort, TimedCommand, UnitMeter};
use lf_engine::{scope::ScopeBin, tuner::TunerFrame};
use rtrb::{Consumer, Producer};
use super::chain;

/// The engine's non-RT ends, replaced with the engine (a sample-rate change builds a new one).
pub(crate) struct Ends {
    pub(crate) commands: Producer<TimedCommand>,
    /// The commands `commands` took, in all: the engine's overview counts the ones it has taken
    /// ([`EngineHost::silence_slot`] waits on a block after its own).
    pub(crate) pushed: u64,
    pub(crate) events: Consumer<Event>,
    pub(crate) overview: Arc<Overview>,
    /// The session port (`session.rs`), out while a snapshot or load uses it.
    pub(crate) session: Option<SessionPort>,
    /// The scope taps' columns (`lf_engine::scope`), drained by the feed every tick, and the frames one
    /// column of this engine covers.
    pub(crate) scope: Consumer<ScopeBin>,
    pub(crate) scope_bin: u32,
    /// The tuner's tap (`lf_engine::tuner`): both slots' raw input while the tuner is on, drained by the
    /// feed every tick with the scope; and this engine's sample rate, which the feed's pitch analysis
    /// is built for.
    pub(crate) tuner: Consumer<TunerFrame>,
    pub(crate) rate: u32,
    /// The unit meter ring (`EngineHandle::unit_meters`): each seated unit's meter, folded by the engine
    /// at most every 4 ms; drained by the feed every tick with the scope, off the audio thread, and
    /// folded per unit until a frame carries it (`feed.rs`).
    pub(crate) unit_meters: Consumer<UnitMeter>,
}

/// What one feed drain read under `settings` and `ends`, besides the events and taps it moved out.
pub(crate) struct FeedDrain {
    /// The engine generation the drain read (`Core::engine_gen`).
    pub(crate) gen: u64,
    /// That engine's overview; `None` while no engine exists.
    pub(crate) overview: Option<Arc<Overview>>,
    /// The frames one scope column of that engine covers (0: no engine).
    pub(crate) scope_bin: u32,
    /// That engine's sample rate (0: no engine).
    pub(crate) rate: u32,
    /// The chain model's snapshot read in the same `settings` + `ends` acquisition as the events, of the
    /// generation `gen` names (mid-swap, the old one's last: `Model::snapshot_for`).
    pub(crate) chains: chain::ChainRev,
    /// True if [`Core::stash`] overflowed while the feed was stale (the feed sends a reset).
    pub(crate) stash_lost: bool,
}
