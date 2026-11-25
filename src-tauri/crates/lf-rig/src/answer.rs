//! The handover's IPC shapes (D13), mirrored in `src/platform/host.ts` (`RigHost`, `RigAnswer`). Three
//! commands, registered only in a DEV build beside `chain_*` until activation, their disk I/O on the
//! blocking pool:
//!
//! - `rig_open() -> RigAnswer`: what the native record holds. A chain-mode page calls it before it
//!   opens the device. The first call of a process starts the restore; a later one (a reload) answers
//!   the same facts and starts nothing.
//! - `rig_import(text, frontendEpoch) -> RigAnswer`, the args as [`RigImport`]: on `absent`, the page's
//!   upgrade of the legacy keys. Stored only while no record exists (else it answers `present` and
//!   stores nothing); its commit is the import's durability. A replaced page's import is refused with
//!   [`IMPORT_STALE`]; a failed commit is an `Err` with the reason, and nothing is restored.
//! - `rig_seen(id)`: the page showed the notice [`NoticeId`] `id`; it clears it. Answers nothing.

use serde::{Deserialize, Serialize};

/// `rig_import`'s error for a page whose document was replaced (its `frontendEpoch` is not the
/// current one).
pub const IMPORT_STALE: &str = "stale";

/// The answer to `rig_open` and `rig_import`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RigAnswer {
    pub status: RigStatus,
    /// Each slot's volume the fader shows, A then B (D2: the settings memory's slot gain, else the
    /// record's); `Some` only with [`RigStatus::Present`].
    pub volumes: Option<[f64; 2]>,
    /// What the restore forgot or held back, until the page confirms it with `rig_seen`.
    pub notice: Option<Notice>,
}

/// What the native record is, for this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RigStatus {
    /// A record exists (read, or stored by this import). After activation it is also the page's cue to
    /// drop the legacy keys.
    Present,
    /// No record file: the page runs the upgrade and imports it.
    Absent,
    /// A file that cannot be read, or that a newer build wrote: kept byte for byte, never written over;
    /// nothing is restored or written this process (D8).
    Unreadable,
    /// No data folder, or another process owns the record (its lock file is taken, D8): this process
    /// neither reads nor writes the record or the marker.
    Unavailable,
}

/// A notice the page shows once and confirms (D6, D13). Every open answers it until `rig_seen(id)`,
/// so a reply lost to a reload is shown by the next page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    /// Which notice this is, for `rig_seen`; minted by the process's `Rig`.
    pub id: NoticeId,
    /// The display names of the plugin units the crash marker named, now forgotten.
    pub forgotten: Vec<String>,
    /// The crash marker could not be read: no plugin was restored this launch (built-ins were).
    pub plugins_held: bool,
}

/// A notice's id. A `u32`, so the page's number holds it exactly.
pub type NoticeId = u32;

/// `rig_import`'s arguments, as the page names them (`{ text, frontendEpoch }`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RigImport {
    /// The record as `serializeRig` writes it (`lf.rig` v1's JSON, D12).
    pub text: String,
    /// The page's document epoch from `host_init` (the plugin host's `frontendEpoch`).
    pub frontend_epoch: u32,
}
