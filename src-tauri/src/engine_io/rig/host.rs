//! The app's [`RestoreHost`] (CP3b): what the restore drives, over the process's [`EngineHost`] and
//! its chain host ([`Chains`]). `mode.rs` builds it with the rig (`Rig::with_restore`) once the engine
//! started; its tests build it over a test device with a fake [`Loader`] (`host_tests.rs`).
//!
//! - **The seed and its barrier (D1, D2):** [`EngineHost::seed`] decides under `settings` which slot
//!   levels go (a slot whose gain the settings memory holds keeps it) and pushes them with the four
//!   send-offs as one batch; the host keeps the batch's mark (engine generation, ring count), and
//!   [`EngineHost::wait_taken`] waits for that engine's blocks to take it, re-targeted when a rebuild
//!   moves the generation (the new engine's replay carries the seed).
//! - **The add (D5, D9):** `Chains::add_as_host`, the player's typed add at a [`Place`] (resolved under
//!   the slot's order lock and `state`), with a match before the mint and no replacement of the
//!   player's source (the slot's head, or one awaiting its reseat after a rebuild). Refused before the
//!   mint (`Host`, no token) once the host is closing (`Retry::Launch`; read again under `state` at the
//!   reservation, so an add that passed the first check as the shutdown began reserves nothing) and
//!   while no device runs (`Retry::Device`), so no failed plate shows for it.
//! - **The latch (D5, D6):** [`RestoreHost::exited`] is `Chains::exit_known`, reading a token the
//!   chain host never registered as ended: every add registers its attempt's latch at its mint, before
//!   it answers, so a token the restore follows and the host does not know names no attempt an owner
//!   ran for.
//! - **The forget:** `Chains::forget_files` in the tone store's folder, under the chain host's id
//!   reservation lock (`id_guard`), never a slot's order lock. `Deleted` only when no file of the id is
//!   left: a delete that fails for any reason but not-found (a sharing violation, a denied access)
//!   answers `Held`, so the driver asks again at its next turn, as for a unit that holds the id.

use std::path::PathBuf;
use std::sync::atomic::Ordering::Acquire;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lf_engine::{UnitId, UnitKind, SLOT_COUNT};
use lf_rig::{AddFail, AddOk, InstallToken, Place, PluginRefView, Retry, RigView, UnitBody, UnitRecord, Volumes};
use tauri::{Emitter, Manager};

use super::{lock, Forgot, RestoreHost, RigModel, SeedTaken};
use crate::engine_io::chain::{PluginRef, UnitSpec, WireParams};
use crate::engine_io::chain_cmds::{chain_event, forget_picks, Chains};
use crate::engine_io::mode::engine;
use crate::engine_io::plugins::EngineSlot;
use crate::engine_io::unit_host::UnitHost;
use crate::engine_io::EngineHost;
use crate::host::engine_slot::{EventSink, PluginFormat};
use crate::host::tone::{ToneBinding, ToneStore};
use crate::host::unit_owner::{self, Answer, LoadFail, UnitOwner};

/// The scan's cache and the player's plugin folders in the app-local data folder, as
/// `plugin_scan_paths` names them (`host/commands.rs`).
const SCAN_CACHE: &str = "plugin-scan.json";
const FOLDERS_FILE: &str = "plugin-folders.json";

/// What a restore add needs of the app besides the chain host: the plugin's load, its window events,
/// and what follows an add. The app's is [`AppLoader`]; a test's builds fake units.
pub(crate) trait Loader: Send + Sync {
    /// Load the plugin unit (`unit_owner::load_unit` in the app).
    fn open(&self, format: PluginFormat, plugin: &PluginRef, unit: UnitHost, sink: EventSink, tone: Option<ToneBinding>) -> Result<(UnitOwner, Answer), LoadFail>;
    /// Where unit `unit`'s owner sends its `chain:*` events.
    fn sink(&self, unit: UnitId) -> EventSink;
    /// After an add, whatever its outcome (as `chain_add` does).
    fn added(&self, _slot: usize, _unit: UnitId) {}
}

/// The app's [`Loader`]: the plugin's editor is parented to the main window, its events go to every
/// page (the page listens to `chain:*` globally), and native MIDI's router forgets the slot's picks
/// (`forget_picks`: the slot's seated SYNTH may have changed).
pub(crate) struct AppLoader {
    pub(crate) app: tauri::AppHandle,
}

impl Loader for AppLoader {
    fn open(&self, format: PluginFormat, plugin: &PluginRef, unit: UnitHost, sink: EventSink, tone: Option<ToneBinding>) -> Result<(UnitOwner, Answer), LoadFail> {
        let parent = self.app.get_webview_window("main").and_then(|w| w.hwnd().ok()).map(|h| h.0 as usize).unwrap_or(0);
        unit_owner::load_unit(format, plugin.path.clone(), plugin.id.clone(), unit, parent, sink, tone)
    }

    fn sink(&self, unit: UnitId) -> EventSink {
        let app = self.app.clone();
        Arc::new(move |event| {
            let (name, payload) = chain_event(unit, event);
            let _ = app.emit(name, payload);
        })
    }

    fn added(&self, slot: usize, unit: UnitId) {
        if let Ok(app) = engine() {
            forget_picks(app, u8::try_from(slot).ok(), unit);
        }
    }
}

/// The app's restore host (the module doc).
pub(crate) struct AppHost {
    host: EngineHost,
    chains: Arc<Chains>,
    slots: Arc<Mutex<[EngineSlot; SLOT_COUNT]>>,
    /// `None` without an app-local data folder: no tone is loaded or kept, and a forget deletes nothing.
    tones: Option<ToneStore>,
    /// The app-local data folder, for the scan's cache and the player's plugin folders.
    data_dir: Option<PathBuf>,
    loader: Box<dyn Loader>,
    /// The last seed the ring took whole: the engine generation and the ring's count through it
    /// (`None`: sent with no engine, so the first engine's replay carries it). Read and moved by
    /// [`RestoreHost::wait_taken`].
    mark: Mutex<Option<(u64, u64)>>,
}

impl AppHost {
    pub(crate) fn new(
        host: EngineHost,
        chains: Arc<Chains>,
        slots: Arc<Mutex<[EngineSlot; SLOT_COUNT]>>,
        tones: Option<ToneStore>,
        data_dir: Option<PathBuf>,
        loader: Box<dyn Loader>,
    ) -> AppHost {
        AppHost { host, chains, slots, tones, data_dir, loader, mark: Mutex::new(None) }
    }
}

/// A record unit as the chain host's add takes it.
fn unit_spec(unit: &UnitRecord) -> UnitSpec {
    match &unit.body {
        UnitBody::Plugin(p) => UnitSpec {
            kind: UnitKind::Plugin,
            plugin: Some(PluginRef { format: p.plugin.format.clone(), path: p.plugin.path.clone(), id: p.plugin.id.clone() }),
            params: None,
            bypass: unit.bypassed(),
        },
        UnitBody::Builtin { kind, params } => UnitSpec { kind: *kind, plugin: None, params: params.clone().map(WireParams), bypass: unit.bypassed() },
    }
}

impl RigModel for AppHost {
    fn gains(&self) -> Volumes {
        RigModel::gains(&self.host)
    }

    fn view(&self) -> RigView {
        RigModel::view(&self.host)
    }

    fn wait_feed(&self) {
        RigModel::wait_feed(&self.host);
    }
}

impl RestoreHost for AppHost {
    fn seed(&self, volumes: [f64; 2]) -> bool {
        match self.host.seed(volumes) {
            Ok(mark) => {
                *lock(&self.mark) = mark;
                true
            }
            Err(why) => {
                log::debug!("[rig] the seed was not taken whole ({why}); it is sent again");
                false
            }
        }
    }

    fn wait_taken(&self, within: Duration) -> SeedTaken {
        // The mark is the rig thread's alone (the seed and this wait both run there): held across the
        // bounded wait, it blocks no one.
        let mut mark = lock(&self.mark);
        self.host.wait_taken(&mut mark, within)
    }

    fn device(&self) -> Option<u64> {
        self.host.device()
    }

    fn scan(&self, paths: &[String]) -> Vec<PluginRefView> {
        let cache = self.data_dir.as_ref().map(|dir| dir.join(SCAN_CACHE));
        let folders = self.data_dir.as_ref().map(|dir| dir.join(FOLDERS_FILE));
        match crate::host::scan_paths(cache.as_deref(), folders.as_deref(), paths) {
            Ok(found) => found.into_iter().map(|d| PluginRefView { format: d.format, path: d.path, id: d.id }).collect(),
            Err(why) => {
                log::warn!("[rig] the restore's plugin scan failed ({why}); its plugins are held for a later launch");
                Vec::new()
            }
        }
    }

    fn add(&self, slot: usize, id: UnitId, unit: &UnitRecord, place: &Place) -> Result<AddOk, AddFail> {
        if self.host.core.closing.load(Acquire) {
            return Err(AddFail::Host { why: "the host is closing".to_string(), retry: Retry::Launch, token: None });
        }
        if self.host.device().is_none() {
            return Err(AddFail::Host { why: "no audio device is open".to_string(), retry: Retry::Device, token: None });
        }
        let open = |format: PluginFormat, plugin: &PluginRef, unit: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
            self.loader.open(format, plugin, unit, sink, tone)
        };
        let added = self.chains.add_as_host(&self.host, &self.slots, self.tones.as_ref(), slot, id, unit_spec(unit), place, self.loader.sink(id), &open);
        self.loader.added(slot, id);
        added
    }

    fn exited(&self, token: InstallToken) -> bool {
        self.chains.exit_known(token).unwrap_or(true)
    }

    fn forget(&self, id: UnitId) -> Forgot {
        self.chains.forget_files(&self.host, id, self.tones.as_ref().map(ToneStore::dir))
    }
}
