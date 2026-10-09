//! OWNS: engine mode: the process's [`EngineApp`] with its
//! one [`EngineHost`], its feed, its native MIDI ([`MidiHost`]) and its plugin slots (`plugins`), the
//! `engine_*` Tauri commands, and the shutdown on exit.
//!
//! The app always runs on the engine: setup starts the host (its device owner; no device opens until
//! the UI asks), the feed and native MIDI (its ports open at once) once per launch. If the host or the
//! feed does not start, the launch has no audio and no MIDI: every `engine_*` command but
//! `engine_status` answers an error, and so do the `midi_*` commands, `input_send` (`midi_mode`, which
//! carries the UI's engine commands too) and the `plugin_*` commands that route to the engine's slots
//! (`engine()`). Blocking work (an open waits up to 15 s) runs off the IPC thread.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use lf_engine::SLOT_COUNT;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

use super::feed::{FeedHold, FeedThread};
use super::midi::MidiHost;
use super::plugins::EngineSlot;
use crate::host::tone::{ToneHandoff, ToneStore};
use super::wire::FeedFrame;
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

/// Engine mode's state: the host, its feed, native MIDI, its plugin slots and the tone store.
pub struct EngineApp {
    /// `None` when the engine did not start this launch.
    engine: Option<(EngineHost, FeedThread)>,
    /// Native MIDI on the host; `None` when the engine did not start, and from the shutdown on. Its lock
    /// is held across every call into it ([`EngineApp::with_midi`]), so the shutdown's take waits for a
    /// call in flight and drops it before the host shuts down.
    midi: Mutex<Option<MidiHost>>,
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
        let data_dir = match app.path().app_local_data_dir() {
            Ok(dir) => Some(dir),
            Err(e) => {
                log::warn!("[engine_io] no app-local data dir ({e}); no tone is kept, and the MIDI bindings live in memory only");
                None
            }
        };
        let tones = data_dir.as_ref().map(|dir| ToneStore::new(dir.join(TONES_DIR)));
        let host = EngineHost::new(HostConfig::default());
        let (engine, midi) = match FeedThread::spawn(host.clone()) {
            Ok(feed) => {
                log::info!("[engine_io] engine mode: the native engine owns the audio device");
                // DEV: the MIDI latency benchmark and the lock-wait log, when an environment variable asks.
                #[cfg(debug_assertions)]
                super::midi_bench::start_from_env(&host);
                // Its bindings beside `plugin-folders.json` (`host/folders.rs`); it registers its rebuild
                // handshake on the host.
                let midi = MidiHost::start(Arc::new(host.clone()), data_dir);
                (Some((host, feed)), Some(midi))
            }
            Err(e) => {
                log::error!("[engine_io] the feed thread did not start ({e}); this launch has no audio engine");
                host.shutdown();
                (None, None)
            }
        };
        EngineApp {
            engine,
            midi: Mutex::new(midi),
            slots: Mutex::new(std::array::from_fn(|_| EngineSlot::Empty)),
            tones,
            reload_tones: Mutex::default(),
        }
    }

    /// Hold the feed's sends until the guard drops (`FeedThread::hold`): for as long as the UI
    /// thread cannot read them. `None` when the engine did not start (there is no feed).
    pub(crate) fn hold_feed(&self) -> Option<FeedHold> {
        self.engine.as_ref().map(|(_, feed)| feed.hold())
    }

    pub(super) fn host(&self) -> Result<EngineHost, String> {
        self.engine.as_ref().map(|(host, _)| host.clone()).ok_or_else(|| "the native engine is not running".to_string())
    }

    /// Run `f` on native MIDI, under its lock; an error when it does not run (the engine did not start,
    /// or it is shutting down).
    pub(super) fn with_midi<R>(&self, f: impl FnOnce(&MidiHost) -> R) -> Result<R, String> {
        let midi = self.midi.lock().unwrap_or_else(|e| e.into_inner());
        midi.as_ref().map(f).ok_or_else(|| "native MIDI is not running".to_string())
    }

    /// On exit and before the updater's installer (`update.rs`): stop native MIDI (its ports close, what
    /// they and the UI held is released while the device still plays, a bindings write still due is
    /// made, and its rebuild hook leaves the host), stop the feed, save every slot's tone
    /// (`save_tones_on_exit`, bounded on its own), unload the plugins while the device still plays (each
    /// crossfades out), then close the device and drop the engine on its owner thread. Waits at most
    /// `SHUTDOWN_WAIT`: a plugin that hangs in its save or its teardown, or a MIDI driver that hangs in
    /// its close, is left to the process's exit rather than holding the app open.
    pub fn shutdown() {
        let Ok(app) = engine() else { return };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("lf-engine-shutdown".into()).spawn(move || {
            if let Some((host, feed)) = &app.engine {
                // Taken out under its lock (a call in flight finishes first), dropped after it.
                let midi = app.midi.lock().unwrap_or_else(|e| e.into_inner()).take();
                drop(midi);
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

/// Share output's WASAPI render endpoint, or `null` for off.
#[tauri::command]
pub async fn engine_set_share(endpoint: Option<String>) -> Result<(), String> {
    let host = app()?.host()?;
    tauri::async_runtime::spawn_blocking(move || host.set_share(endpoint)).await.map_err(|e| format!("engine_set_share: {e}"))?
}

/// The committed loops as raw bytes (`session.rs`'s layout): JS gets an ArrayBuffer, not a JSON array.
/// With `master` (an export's), the wet master rendered offline from them too. Off the IPC thread: a
/// snapshot copies for up to about a second and a half, and a master's render can take longer.
#[tauri::command]
pub async fn engine_snapshot(master: bool) -> Result<tauri::ipc::Response, String> {
    let host = app()?.host()?;
    let bytes = tauri::async_runtime::spawn_blocking(move || host.snapshot(master)).await.map_err(|e| format!("engine_snapshot: {e}"))??;
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
