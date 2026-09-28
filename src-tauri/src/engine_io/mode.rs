//! OWNS: engine mode (`docs/plans/native-engine.md` § Stage 5): the process's [`EngineApp`] with its
//! one [`EngineHost`], its feed and its plugin slots (`plugins`), the `engine_*` Tauri commands, and
//! the shutdown on exit.
//!
//! The app always runs on the engine: setup starts the host (its device owner; no device opens until
//! the UI asks) and the feed once per launch. If either does not start, the launch has no audio: every
//! `engine_*` command but `engine_status` answers an error, and so do the `plugin_*` commands that
//! route to the engine's slots (`engine()`). Blocking work (an open waits up to 15 s) runs off the
//! IPC thread.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use lf_engine::{TimedCommand, SLOT_COUNT};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

use super::feed::FeedThread;
use super::plugins::EngineSlot;
use crate::host::tone::{ToneHandoff, ToneStore};
use super::wire::{FeedFrame, WireCommand};
use super::{DeviceRequest, DeviceStatus, EngineHost, HostConfig, OpenError};

/// The tone store's folder in the app-local data folder (`host/tone.rs`).
const TONES_DIR: &str = "tones";

/// Set once, at setup.
static APP: OnceLock<EngineApp> = OnceLock::new();

/// How long the exit waits for the engine's shutdown (the plugins' unloads, then the device's close).
const SHUTDOWN_WAIT: Duration = Duration::from_secs(8);

/// This launch's engine; an error when it did not start (the log says why).
pub fn engine() -> Result<&'static EngineApp, String> {
    APP.get().filter(|app| app.engine.is_some()).ok_or_else(|| "the native engine is not running".to_string())
}

/// The ASIO driver switch: `switch` (`audio_output::switch_asio_driver`) runs on the device owner
/// (`EngineHost::switch_asio`), which closes its ASIO run first and reopens it after, so nothing holds
/// the driver while it is replaced.
pub fn switch_asio(
    switch: impl FnOnce() -> Result<crate::asio_startup::AsioStatusReport, String> + Send + 'static,
) -> Result<crate::asio_startup::AsioStatusReport, String> {
    engine()?.host()?.switch_asio(switch)
}

/// Engine mode's state: the host, its feed, its plugin slots and the tone store.
pub struct EngineApp {
    /// `None` when the engine did not start this launch.
    engine: Option<(EngineHost, FeedThread)>,
    pub(super) slots: Mutex<[EngineSlot; SLOT_COUNT]>,
    /// `None` without an app-local data folder: plugins then load at their defaults and keep nothing.
    pub(super) tones: Option<ToneStore>,
    /// A session import's tone, parked for the reload of the slot that held its plugin (`plugins`).
    pub(super) reload_tones: Mutex<ToneHandoff<SLOT_COUNT>>,
}

impl EngineApp {
    /// Start the host (its device owner; no device opens until the UI asks) and the feed. Once per
    /// process.
    pub fn setup(app: &AppHandle) {
        if APP.set(EngineApp::start(app)).is_err() {
            log::error!("[engine_io] engine mode was set up twice; the second is ignored");
        }
    }

    fn start(app: &AppHandle) -> EngineApp {
        let tones = match app.path().app_local_data_dir() {
            Ok(dir) => Some(ToneStore::new(dir.join(TONES_DIR))),
            Err(e) => {
                log::warn!("[engine_io] no app-local data dir ({e}); no tone is kept");
                None
            }
        };
        let host = EngineHost::new(HostConfig::default());
        let engine = match FeedThread::spawn(host.clone()) {
            Ok(feed) => {
                log::info!("[engine_io] engine mode: the native engine owns the audio device");
                Some((host, feed))
            }
            Err(e) => {
                log::error!("[engine_io] the feed thread did not start ({e}); this launch has no audio engine");
                host.shutdown();
                None
            }
        };
        EngineApp {
            engine,
            slots: Mutex::new(std::array::from_fn(|_| EngineSlot::Empty)),
            tones,
            reload_tones: Mutex::default(),
        }
    }

    pub(super) fn host(&self) -> Result<EngineHost, String> {
        self.engine.as_ref().map(|(host, _)| host.clone()).ok_or_else(|| "the native engine is not running".to_string())
    }

    /// On exit: stop the feed, save every slot's tone (`save_tones_on_exit`, bounded on its own), unload
    /// the plugins while the device still plays (each crossfades out), then close the device and drop
    /// the engine on its owner thread. Waits at most `SHUTDOWN_WAIT`: a plugin that hangs in its save or
    /// its teardown is left to the process's exit rather than holding the app open.
    pub fn shutdown() {
        let Ok(app) = engine() else { return };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("lf-engine-shutdown".into()).spawn(move || {
            if let Some((host, feed)) = &app.engine {
                feed.stop();
                app.save_tones_on_exit();
                app.unload_all();
                host.shutdown();
            }
            let _ = done_tx.send(());
        });
        match spawned.map(|_| done_rx.recv_timeout(SHUTDOWN_WAIT)) {
            Ok(Ok(())) => log::info!("[engine_io] engine mode shut down"),
            Ok(Err(_)) => log::error!("[engine_io] the engine did not shut down within {} s; exiting anyway", SHUTDOWN_WAIT.as_secs()),
            Err(e) => log::error!("[engine_io] no thread for the engine's shutdown ({e}); exiting without it"),
        }
    }
}

/// The app's one state, for a command.
fn app() -> Result<&'static EngineApp, String> {
    APP.get().ok_or_else(|| "engine mode is not set up".to_string())
}

/// Open the device, or switch to another; resolves with the device that runs. A switch to another rate
/// while the engine holds audio is refused (`OpenError::RateChange`, which the UI confirms) unless `force`.
#[tauri::command]
pub async fn engine_open(request: DeviceRequest, force: bool) -> Result<DeviceStatus, OpenError> {
    let host = app()?.host()?;
    log::info!("[engine_io] open requested: {request:?}{}", if force { " (forced)" } else { "" });
    tauri::async_runtime::spawn_blocking(move || host.open(request, force)).await.map_err(|e| format!("engine_open: {e}"))?
}

/// Stop the device (the loops pause in place).
#[tauri::command]
pub async fn engine_close() -> Result<(), String> {
    let host = app()?.host()?;
    tauri::async_runtime::spawn_blocking(move || host.close()).await.map_err(|e| format!("engine_close: {e}"))?
}

/// The device that runs, or `null`.
#[tauri::command]
pub async fn engine_status() -> Result<Option<DeviceStatus>, String> {
    Ok(app().ok().and_then(|app| app.host().ok()).and_then(|host| host.status()))
}

/// A plugin slot's capture channel (0-based; `null` = auto), switched without rebuilding a stream.
#[tauri::command]
pub async fn engine_set_slot_input_channel(slot: u8, channel: Option<u32>) -> Result<(), String> {
    let host = app()?.host()?;
    tauri::async_runtime::spawn_blocking(move || host.set_slot_input_channel(slot as usize, channel))
        .await
        .map_err(|e| format!("engine_set_slot_input_channel: {e}"))?
}

/// A batch of commands, in order, at the next block. Fire-and-forget: what the engine refuses comes
/// back on the feed; an error means the rest of the batch did not reach it. Each slot reads its own
/// input, so several may be live at once: which are is the UI's call (`src/audio/native-io.ts`).
/// Synchronous: it runs on the main thread, where the IPC hands requests over in order, so two batches
/// cannot swap (an async command runs on the runtime's pool); it only takes two brief locks.
#[tauri::command]
pub fn engine_send(commands: Vec<WireCommand>) -> Result<(), String> {
    let host = app()?.host()?;
    host.send_all(commands.into_iter().map(|c| TimedCommand { frame: None, command: c.0 }))
}

/// Share output's WASAPI render endpoint, or `null` for off.
#[tauri::command]
pub async fn engine_set_share(endpoint: Option<String>) -> Result<(), String> {
    let host = app()?.host()?;
    tauri::async_runtime::spawn_blocking(move || host.set_share(endpoint)).await.map_err(|e| format!("engine_set_share: {e}"))?
}

/// The committed loops as raw bytes (`session.rs`'s layout): JS gets an ArrayBuffer, not a JSON array.
/// Off the IPC thread: a snapshot copies for up to about a second and a half.
#[tauri::command]
pub async fn engine_snapshot() -> Result<tauri::ipc::Response, String> {
    let host = app()?.host()?;
    let bytes = tauri::async_runtime::spawn_blocking(move || host.snapshot()).await.map_err(|e| format!("engine_snapshot: {e}"))??;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Load a session into an engine whose lanes are all EMPTY: the raw request body is the session's
/// bytes (`session.rs`'s layout; `invoke('engine_load_session', bytes)` with a `Uint8Array`).
#[tauri::command]
pub async fn engine_load_session(request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("engine_load_session takes the session's bytes as the raw request body".to_string());
    };
    let (host, bytes) = (app()?.host()?, bytes.clone());
    tauri::async_runtime::spawn_blocking(move || host.load_session(&bytes)).await.map_err(|e| format!("engine_load_session: {e}"))?
}

/// Subscribe `channel` to the feed (replacing the last subscriber); its first frame is a reset.
#[tauri::command]
pub async fn engine_feed(channel: Channel<FeedFrame>) -> Result<(), String> {
    let (_, feed) = app()?.engine.as_ref().ok_or("the native engine is not running")?;
    feed.subscribe(move |frame| channel.send(frame).is_ok());
    Ok(())
}
