//! OWNS: engine mode (`docs/plans/native-engine.md` § Stage 5, Toggle): the toggle file, the process's
//! [`EngineApp`] with its one [`EngineHost`], its feed and its plugin slots (`plugins`), the `engine_*`
//! Tauri commands, and the shutdown on exit.
//!
//! The toggle is a file in the app-local data folder, read once at setup and applied on the next
//! launch, never live (ASIO allows one client). Engine mode is the default: only `off` in the file (the
//! Audio Settings switch writes `on` or `off`) runs the web audio path, the live line; there every
//! `engine_*` command but `engine_mode`, `engine_set_mode` and `engine_status` answers an error. On, the engine
//! owns the audio device: it claims the ASIO duplex holder, the live line's `plugin_*` commands route to
//! the engine's slots or refuse (`engine()`), and one plugin slot is live at a time. Blocking work (an
//! open waits up to 15 s) runs off the IPC thread.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use lf_engine::{Command, TimedCommand, SLOT_COUNT};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

use super::feed::FeedThread;
use super::plugins::EngineSlot;
use super::wire::{FeedFrame, WireCommand};
use super::{DeviceRequest, DeviceStatus, EngineHost, HostConfig};

/// The toggle file in the app-local data folder: `off` runs this app on the web audio path; anything
/// else, or no file, on the engine.
const TOGGLE_FILE: &str = "engine-mode";
/// The engine's claim on `audio_output`'s ASIO duplex holder (the live line's slots are 0 and 1).
const ASIO_HOLDER: u8 = 2;

/// Set once, at setup.
static APP: OnceLock<EngineApp> = OnceLock::new();

/// How long the exit waits for the engine's shutdown (the plugins' unloads, then the device's close).
const SHUTDOWN_WAIT: Duration = Duration::from_secs(8);

/// This launch's engine, when it runs on the engine: the live line's `plugin_*` commands route to it.
pub fn engine() -> Option<&'static EngineApp> {
    APP.get().filter(|app| app.engine.is_some())
}

/// Whether this launch runs on the engine.
pub fn active() -> bool {
    engine().is_some()
}

/// Engine mode's state: the toggle, and the host, its feed and its plugin slots while it is on.
pub struct EngineApp {
    toggle: Option<PathBuf>,
    engine: Option<(EngineHost, FeedThread)>,
    pub(super) slots: Mutex<[EngineSlot; SLOT_COUNT]>,
}

impl EngineApp {
    /// Read the toggle and, unless it is off, start the host (its device owner; no device opens until
    /// the UI asks) and the feed. Once per process.
    pub fn setup(app: &AppHandle) {
        if APP.set(EngineApp::start(app)).is_err() {
            log::error!("[engine_io] engine mode was set up twice; the second is ignored");
        }
    }

    fn start(app: &AppHandle) -> EngineApp {
        let toggle = match app.path().app_local_data_dir() {
            Ok(dir) => Some(dir.join(TOGGLE_FILE)),
            Err(e) => {
                log::warn!("[engine_io] no app-local data dir ({e}); the engine toggle cannot be read or saved");
                None
            }
        };
        let off = |toggle| EngineApp { toggle, engine: None, slots: Mutex::new(std::array::from_fn(|_| EngineSlot::Empty)) };
        if toggle.as_ref().is_some_and(|path| toggled_off(path)) {
            log::info!("[engine_io] web audio mode: the engine toggle is off");
            return off(toggle);
        }
        if !crate::audio_output::try_acquire_asio_holder(ASIO_HOLDER) {
            log::error!("[engine_io] the ASIO duplex holder is taken before setup; engine mode stays off");
            return off(toggle);
        }
        let host = EngineHost::new(HostConfig::default());
        match FeedThread::spawn(host.clone()) {
            Ok(feed) => {
                log::info!("[engine_io] engine mode: the native engine owns the audio device");
                EngineApp { engine: Some((host, feed)), ..off(toggle) }
            }
            Err(e) => {
                log::error!("[engine_io] the feed thread did not start ({e}); engine mode stays off");
                host.shutdown();
                crate::audio_output::release_asio_holder(ASIO_HOLDER);
                off(toggle)
            }
        }
    }

    pub(super) fn host(&self) -> Result<EngineHost, String> {
        self.engine.as_ref().map(|(host, _)| host.clone()).ok_or_else(|| "engine mode is off".to_string())
    }

    /// On exit: stop the feed, unload the plugins while the device still plays (each crossfades out),
    /// then close the device and drop the engine on its owner thread. Waits at most `SHUTDOWN_WAIT`: a
    /// plugin that hangs in its teardown is left to the process's exit rather than holding the app open.
    pub fn shutdown() {
        let Some(app) = engine() else { return };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("lf-engine-shutdown".into()).spawn(move || {
            if let Some((host, feed)) = &app.engine {
                feed.stop();
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

/// The toggle file says `off`. No file, or one that cannot be read, leaves the engine on.
fn toggled_off(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text.trim() == "off")
}

/// Whether this launch runs on the engine.
#[tauri::command]
pub fn engine_mode() -> bool {
    active()
}

/// Write the toggle for the next launch.
#[tauri::command]
pub async fn engine_set_mode(enabled: bool) -> Result<(), String> {
    let path = app()?.toggle.clone().ok_or("no app-local data folder for the engine toggle")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("engine toggle: {e}"))?;
    }
    std::fs::write(&path, if enabled { "on\n" } else { "off\n" }).map_err(|e| format!("engine toggle: {e}"))?;
    log::info!("[engine_io] engine mode {} from the next launch", if enabled { "on" } else { "off" });
    Ok(())
}

/// Open the device, or switch to another; resolves with the device that runs.
#[tauri::command]
pub async fn engine_open(request: DeviceRequest) -> Result<DeviceStatus, String> {
    let host = app()?.host()?;
    log::info!("[engine_io] open requested: {request:?}");
    tauri::async_runtime::spawn_blocking(move || host.open(request)).await.map_err(|e| format!("engine_open: {e}"))?
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

/// The capture channel (0-based; `null` = auto), switched without rebuilding a stream.
#[tauri::command]
pub async fn engine_set_input_channel(channel: Option<u32>) -> Result<(), String> {
    let host = app()?.host()?;
    tauri::async_runtime::spawn_blocking(move || host.set_input_channel(channel)).await.map_err(|e| format!("engine_set_input_channel: {e}"))?
}

/// A batch of commands, in order, at the next block. Fire-and-forget: what the engine refuses comes
/// back on the feed; an error means the rest of the batch did not reach it. A slot going live takes the
/// other slot off first: one is live at a time (two would sum the dry input twice). Synchronous: it
/// runs on the main thread, where the IPC hands requests over in order, so two batches cannot swap
/// (an async command runs on the runtime's pool); it only takes two brief locks.
#[tauri::command]
pub fn engine_send(commands: Vec<WireCommand>) -> Result<(), String> {
    let host = app()?.host()?;
    host.send_all(one_live(commands.into_iter().map(|c| c.0)).map(|command| TimedCommand { frame: None, command }))
}

/// `SetSlotLive(i, true)` preceded by every other slot's `SetSlotLive(j, false)`.
fn one_live(commands: impl Iterator<Item = Command>) -> impl Iterator<Item = Command> {
    commands.flat_map(|command| {
        let others = match command {
            Command::SetSlotLive(i, true) => (0..SLOT_COUNT as u8).filter(|&j| j != i).collect(),
            _ => Vec::new(),
        };
        others.into_iter().map(|j| Command::SetSlotLive(j, false)).chain([command])
    })
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
    let (_, feed) = app()?.engine.as_ref().ok_or("engine mode is off")?;
    feed.subscribe(move |frame| channel.send(frame).is_ok());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_off_in_the_toggle_file_turns_the_engine_off() {
        let dir = std::env::temp_dir().join(format!("lf-engine-toggle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(TOGGLE_FILE);
        let _ = std::fs::remove_file(&path);
        assert!(!toggled_off(&path), "no file: the engine");
        for (text, off) in [("off\n", true), ("off", true), ("on\n", false), ("", false), ("OFF", false)] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(toggled_off(&path), off, "{text:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slot_going_live_takes_the_other_off_first() {
        let sent: Vec<Command> = one_live([Command::SetSlotLive(1, true), Command::PlayAll, Command::SetSlotLive(0, false)].into_iter()).collect();
        assert_eq!(sent, [Command::SetSlotLive(0, false), Command::SetSlotLive(1, true), Command::PlayAll, Command::SetSlotLive(0, false)]);
    }
}
