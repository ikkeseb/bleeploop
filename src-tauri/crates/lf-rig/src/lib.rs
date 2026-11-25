//! lf-rig: chain mode's rig record, owned by the native host: the record's shape, the model's view of
//! the chains it is projected from, the add answers the restore reacts to, and what the page is told.
//! Pure: no file, lock, thread, device or IPC call. The app's Windows glue (`engine_io/rig/` in the
//! app crate, from CP2b) does the I/O and drives these rules, so `cargo test -p lf-rig` runs on Linux
//! in seconds (CI's `engine` job, `scripts/engine-test.mjs`).
//!
//! **Who writes what.** One `Rig` per process, in the app's `EngineApp`. Only `Rig::commit` writes the
//! record file, and only the restore's `CommitMarker` action writes the crash marker; the page writes
//! neither. The page's part is one handover: `rig_open`, and on `absent` one `rig_import` of what
//! its upgrade of the legacy keys made ([`answer`]).
//!
//! **This checkpoint (CP1) is the seam:** the types only, fixed before the parallel writers build on
//! them. The rules land in the modules the plan names: the tolerant parse, the store and the
//! projection (CP2a); the restore machine, the marker and placement (CP2c).
//!
//! | Module | Owns |
//! |---|---|
//! | [`record`] | The record as stored: `lf.rig` v1's JSON unchanged ([`RigRecord`]) |
//! | [`view`] | The chain model as the rig reads it ([`RigView`], [`PlateView`]) |
//! | [`add`] | An add's answer as the restore sees it ([`AddOk`], [`AddFail`]) and where it goes ([`Place`]) |
//! | [`answer`] | The handover's IPC shapes ([`RigAnswer`], [`RigImport`], [`NoticeId`]) |
//! | [`plan`] | What the restore holds per record unit ([`Plan`], [`Disposition`]): filled by the restore, read by the projection |
//! | [`project`](mod@project) | The record as a projection of the model ([`project()`]), a change's kind ([`Change`]), the import ([`import()`]) |
//! | [`store`] | The record's file over a [`Disk`]: the lock, the commit criterion, the writer's pace ([`Store`]) |
//!
//! The decisions cited as `D<n>` are the build plan's (`native-rig-plan.md`, a working document
//! while the move is open); each lands in `docs/unit-contract.md` with the checkpoint that builds it.

pub mod add;
pub mod answer;
pub mod plan;
pub mod project;
pub mod record;
pub mod store;
pub mod view;

pub use add::{AddFail, AddOk, AddState, Place, Retry};
pub use answer::{Notice, NoticeId, RigAnswer, RigImport, RigStatus, IMPORT_STALE};
pub use plan::{Disposition, Plan};
pub use project::{change, import, project, volumes, Change, ImportError, Volumes};
pub use record::{Params, PluginRecord, RigRecord, SlotRecord, UnitBody, UnitRecord, Unreadable, Version1, SLOT_GAIN_MAX};
pub use store::{Commit, Disk, Files, Opened, Refusal, Store, DEV_FILES, RELEASE_FILES, SAVE_QUIET, TICK};
pub use view::{PlateState, PlateView, PluginRefView, RigView};

/// The unit vocabulary the record and the model share, from the engine.
pub use lf_engine::{InstallToken, UnitId, UnitKind, UnitParam};
