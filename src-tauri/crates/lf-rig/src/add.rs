//! A restore add as the restore machine sees it (D5, D9): where the unit goes, and the answer it gets.
//! The app's typed result (`unit_owner::load_unit` and the built-in add, from CP3a) carries the owner's
//! exit latch itself; the glue reads it into [`AddFail`]'s `exited` and, when it was not set yet, feeds
//! the machine a `LatchSet` for that attempt later. So the machine holds plain values, never a handle.
//!
//! **An attempt is its install token (D3, D5, D6).** `EngineHost::unit` mints the token (its `Row`)
//! before any plugin code runs, and the plate the add creates carries it as
//! [`crate::PlateView::added_by`]. Every answer past that mint names it ([`AddOk::token`], each
//! [`AddFail`]; `Host`'s only when it came after the mint), so the restore ties a late outcome, a `Waiting` that fails later, and the
//! exit latch to this attempt, never to a later plate under the same id. Native keeps the owner's exit
//! latch per token, also after `Waiting`, readable by `close` while the driver is blocked; a later
//! `LatchSet` names the token, not the unit. A `Host` refusal before the mint has no token.

use lf_engine::{InstallToken, UnitId};

/// Where a restore add goes in its slot's chain (D9). Resolved to an index inside the app's
/// `EngineHost::unit`, under the model's `state` lock, while the add holds its slot's order lock, so a
/// unit gone between the rig's view and the add is read where it stands then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// After its predecessors: the record units before this one in its slot, in record order (nearest
    /// last). The add goes right after the nearest one present in the chain; none present (or none
    /// listed) puts it at the head.
    After(Vec<UnitId>),
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
/// carries it, for logs and the notice. `token` is the attempt's install token (the module doc): every
/// variant past the mint carries it. `exited` is whether the unit's owner thread had ended (its exit
/// latch set) when the answer came: only then can a failed plugin load leave the crash marker (D6);
/// when false, a later `LatchSet` for the token says it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddFail {
    /// Refused before any plugin code ran (a format, slot, closing or legacy-mix refusal, the id
    /// reserved or still leaving, the owner's spawn, no engine). No owner ran, so nothing to wait for.
    /// The restore holds the unit at its place (`Held`). `token` is `None` for a refusal before the
    /// mint, and the attempt's for one after it (today the owner's spawn and no engine come after
    /// `EngineHost::unit`, so their failed plate carries it).
    Host { why: String, retry: Retry, token: Option<InstallToken> },
    /// The unit is a source and a source the player put in already heads the slot. The restore yields
    /// it (`Yielded`, D9).
    SourceTaken { why: String, token: InstallToken, exited: bool },
    /// The engine or the host refused the seat after the plugin ran (a seat refusal, a full port, a
    /// replaced unit). The unit is `Held`.
    Seat { why: String, token: InstallToken, exited: bool },
    /// The plugin's own setup or activation failed. The unit is `Forgotten`; its failed plate stays
    /// visible.
    Plugin { why: String, token: InstallToken, exited: bool },
    /// No answer to trust: the setup timed out with the owner possibly still in plugin code, the add
    /// went unanswered, or a built-in did not answer. The unit may still seat: it stays `Issued` until
    /// the plate its token added applies (`Restored`) or fails with the latch set (`Held`).
    Unresolved { why: String, token: InstallToken, exited: bool },
}

/// When a [`AddFail::Host`] refusal may be tried again under the same id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
    /// In this process, once a device runs (the refusal was no engine).
    Device,
    /// At the next launch.
    Launch,
}
