//! The chain model as the rig reads it (D3): the app's `Model::rig_view` copies it under the model's
//! `state` lock and releases the lock before the rig projects, places or matches anything. Built from
//! the app's `chain.rs` `Plate`; legacy plates never appear in it.

use lf_engine::{InstallToken, UnitId, UnitKind};
use serde::{Deserialize, Serialize};

use crate::record::Params;

/// Both slots' chains as one consistent copy.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RigView {
    /// The model's revision when the copy was taken (`chain.rs` `Model::rev`, the one the feed's
    /// `chains` carry).
    pub rev: u64,
    /// Slot A's plates, then slot B's, each in chain order (head first), legacy plates left out.
    pub slots: [Vec<PlateView>; 2],
}

/// One plate of a slot's chain, as the rig needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct PlateView {
    pub id: UnitId,
    /// The install token `EngineHost::unit` minted for the add that created this plate. A plate is
    /// created per add, so a reused id has a new token: a disposition or an `applied` fact never
    /// passes to the next unit under the same id. The restore matches its add's answer to the plate by
    /// it ([`crate::AddOk::token`]).
    pub added_by: InstallToken,
    pub kind: UnitKind,
    /// The plugin's identity: `Some` for a plugin, `None` for a built-in.
    pub plugin: Option<PluginRefView>,
    /// The display name (the scan's for a plugin, the kind's for a built-in), for the notice and logs.
    pub name: String,
    /// A built-in's applied values; `None` for a plugin (its state is its tone file, not the record).
    pub params: Option<Params>,
    pub bypass: bool,
    /// Whether the unit plays notes and heads its chain (a plugin's activation said so, or SYNTH).
    /// For a plugin it means something only once `applied` (D9: a slot's role is read from its
    /// applied head).
    pub source: bool,
    pub state: PlateState,
    /// Set at the plate's first applied install, in either place the model applies one (`on_seat`'s
    /// `Applied`, a rebuild's settle), and never cleared: a reload, a rebuild, a withdrawal or a park
    /// keeps it. A plate never applied (still loading, `Waiting`, or reseating a first install that
    /// never applied) is not recorded (D4), unless the restore holds its id.
    pub applied: bool,
    /// The plate is a failed add (`Plate::is_failed_add`: failed, not parked, no seat in this engine,
    /// not reseating). Such a plate is never a match for a restore add and leaves the model when its id
    /// is removed or reused (D9).
    pub failed_add: bool,
}

/// A plate's state (the app's `chain.rs` `PlateState`, which the wire spells lowercase).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlateState {
    /// Its owner is building it; nothing is in the engine yet.
    Loading,
    /// A held install: a take holds the alignment, so it has not applied.
    Waiting,
    /// In the engine (a parked, withdrawn or reloading plate reads seated too).
    Seated,
    /// Its removal is queued; a refusal reverts it, so the rig keeps it until it is gone.
    Leaving,
    /// Its load or its seat failed, or it was lost; see [`PlateView::failed_add`].
    Failed,
}

/// A plugin's identity as the scan names it (`format`, `path`, `id`): the tone store's key, so a
/// unit's plugin and its tone are one key (the app's `ToneIdentity`). `format` is `clap`, `vst3` or
/// `vst2`, kept as text as the app keeps it; paths are compared byte for byte, never canonicalised.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PluginRefView {
    pub format: String,
    pub path: String,
    pub id: String,
}
