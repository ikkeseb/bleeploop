//! A restore add as the restore machine sees it (D5, D9): where the unit goes, and the answer it gets.
//! The app's typed result (`unit_owner::load_unit` and the built-in add, from CP3a) carries the owner's
//! exit latch itself; the glue reads it into [`AddFail`]'s `exited` and, when it was not set yet, feeds
//! the machine a `LatchSet` for that unit later. So the machine holds plain values, never a handle.

use lf_engine::{InstallToken, UnitId};

/// Where a restore add goes in its slot's chain (D9). Resolved to an index inside the app's
/// `EngineHost::unit`, under the model's `state` lock, while the add holds its slot's order lock; an
/// anchor gone meanwhile falls back to the next earlier record unit present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// Right after this unit: the nearest record unit before it in the record (`None`: the head).
    After(Option<UnitId>),
    /// At the chain's end.
    End,
}

/// An add the host took: the unit is in the engine or waiting to go in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddOk {
    pub state: AddState,
    /// The install token of the plate the add created ([`crate::PlateView::added_by`]): the restore
    /// follows the unit's outcome by it in the views that follow.
    pub token: InstallToken,
}

/// How an add the host took stands at its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddState {
    /// Applied by the engine.
    Seated,
    /// A held install (a take holds the alignment): not applied yet, so not terminal for the marker.
    Waiting,
}

/// An add that failed, by who refused it (D5). `why` is the host's reason as the wire's `chain_add`
/// carries it, for logs and the notice. `exited` is whether the unit's owner thread had ended (its
/// exit latch set) when the answer came: only then can a failed plugin load leave the crash marker
/// (D6); when false, a later `LatchSet` for the unit says it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddFail {
    /// Refused before any plugin code ran (a format, slot, closing or legacy-mix refusal, the id
    /// reserved or still leaving, the owner's spawn, no engine). No owner ran, so nothing to wait for.
    /// The restore holds the unit at its place (`Held`).
    Host { why: String, retry: Retry },
    /// The engine or the host refused the seat after the plugin ran (a seat refusal, a full port, a
    /// replaced unit, a source already heading the slot). The unit is `Held`, or `Yielded` for a
    /// source the player put in (D9).
    Seat { why: String, exited: bool },
    /// The plugin's own setup or activation failed. The unit is `Forgotten`; its failed plate stays
    /// visible.
    Plugin { why: String, exited: bool },
    /// No answer to trust: the setup timed out with the owner possibly still in plugin code, the add
    /// went unanswered, or a built-in did not answer. The unit may still seat: it stays `Issued` until
    /// its plate applies (`Restored`) or fails with the latch set (`Held`).
    Unresolved { why: String, exited: bool },
}

/// When a [`AddFail::Host`] refusal may be tried again under the same id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
    /// In this process, once a device runs (the refusal was no engine).
    Device,
    /// At the next launch.
    Launch,
}
