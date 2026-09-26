//! OWNS: engine mode (`docs/plans/native-engine.md` § Stage 5, Toggle): the toggle file, the managed
//! [`EngineApp`] with the process's one [`EngineHost`] and its feed, the `engine_*` Tauri commands, and
//! the host's shutdown on exit.
//!
//! The toggle is a file in the app-local data folder, read once at setup and applied on the next
//! launch, never live (ASIO allows one client). Off, the app is the live line and every `engine_*`
//! command but `engine_mode`, `engine_set_mode` and `engine_status` answers an error. On, the engine
//! owns the audio device: it claims the ASIO duplex holder, and the live line's input and monitor arms
//! are refused (`active`). Blocking work (an open waits up to 15 s) runs off the IPC thread.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use lf_engine::TimedCommand;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};

use super::feed::FeedThread;
use super::wire::{FeedFrame, WireCommand};
use super::{DeviceRequest, DeviceStatus, EngineHost, HostConfig};

/// The toggle file in the app-local data folder: `on` runs this app on the engine.
const TOGGLE_FILE: &str = "engine-mode";
/// The engine's claim on `audio_output`'s ASIO duplex holder (the live line's slots are 0 and 1).
const ASIO_HOLDER: u8 = 2;

/// This launch runs on the engine (set once, at setup).
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether this launch runs on the engine: the live line's device commands refuse while it does.
pub fn active() -> bool {
    ACTIVE.load(Relaxed)
}

/// Managed state: the engine host and its feed while engine mode is on.
pub struct EngineApp {
    toggle: Option<PathBuf>,
    engine: Option<(EngineHost, FeedThread)>,
}

impl EngineApp {
    /// Read the toggle and, when it is on, start the host (its device owner; no device opens until the
    /// UI asks) and the feed.
    pub fn setup(app: &AppHandle) -> EngineApp {
        let toggle = match app.path().app_local_data_dir() {
            Ok(dir) => Some(dir.join(TOGGLE_FILE)),
            Err(e) => {
                log::warn!("[engine_io] no app-local data dir ({e}); engine mode stays off");
                None
            }
        };
        let on = toggle.as_ref().is_some_and(|path| read_toggle(path));
        if !on {
            return EngineApp { toggle, engine: None };
        }
        if !crate::audio_output::try_acquire_asio_holder(ASIO_HOLDER) {
            log::error!("[engine_io] the ASIO duplex holder is taken before setup; engine mode stays off");
            return EngineApp { toggle, engine: None };
        }
        let host = EngineHost::new(HostConfig::default());
        match FeedThread::spawn(host.clone()) {
            Ok(feed) => {
                ACTIVE.store(true, Relaxed);
                log::info!("[engine_io] engine mode: the native engine owns the audio device");
                EngineApp { toggle, engine: Some((host, feed)) }
            }
            Err(e) => {
                log::error!("[engine_io] the feed thread did not start ({e}); engine mode stays off");
                host.shutdown();
                crate::audio_output::release_asio_holder(ASIO_HOLDER);
                EngineApp { toggle, engine: None }
            }
        }
    }

    fn host(&self) -> Result<EngineHost, String> {
        self.engine.as_ref().map(|(host, _)| host.clone()).ok_or_else(|| "engine mode is off".to_string())
    }

    /// On exit: stop the feed, then close the device and drop the engine on its owner thread.
    pub fn shutdown(&self) {
        if let Some((host, feed)) = &self.engine {
            feed.stop();
            host.shutdown();
            log::info!("[engine_io] engine mode shut down");
        }
    }
}

fn read_toggle(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text.trim() == "on")
}

/// Whether this launch runs on the engine.
#[tauri::command]
pub fn engine_mode() -> bool {
    active()
}

/// Write the toggle for the next launch.
#[tauri::command]
pub async fn engine_set_mode(enabled: bool, state: State<'_, EngineApp>) -> Result<(), String> {
    let path = state.toggle.clone().ok_or("no app-local data folder for the engine toggle")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("engine toggle: {e}"))?;
    }
    std::fs::write(&path, if enabled { "on\n" } else { "off\n" }).map_err(|e| format!("engine toggle: {e}"))?;
    log::info!("[engine_io] engine mode {} from the next launch", if enabled { "on" } else { "off" });
    Ok(())
}

/// Open the device, or switch to another; resolves with the device that runs.
#[tauri::command]
pub async fn engine_open(request: DeviceRequest, state: State<'_, EngineApp>) -> Result<DeviceStatus, String> {
    let host = state.host()?;
    log::info!("[engine_io] open requested: {request:?}");
    tauri::async_runtime::spawn_blocking(move || host.open(request)).await.map_err(|e| format!("engine_open: {e}"))?
}

/// Stop the device (the loops pause in place).
#[tauri::command]
pub async fn engine_close(state: State<'_, EngineApp>) -> Result<(), String> {
    let host = state.host()?;
    tauri::async_runtime::spawn_blocking(move || host.close()).await.map_err(|e| format!("engine_close: {e}"))?
}

/// The device that runs, or `null`.
#[tauri::command]
pub async fn engine_status(state: State<'_, EngineApp>) -> Result<Option<DeviceStatus>, String> {
    Ok(state.host().ok().and_then(|host| host.status()))
}

/// The capture channel (0-based; `null` = auto), switched without rebuilding a stream.
#[tauri::command]
pub async fn engine_set_input_channel(channel: Option<u32>, state: State<'_, EngineApp>) -> Result<(), String> {
    let host = state.host()?;
    tauri::async_runtime::spawn_blocking(move || host.set_input_channel(channel)).await.map_err(|e| format!("engine_set_input_channel: {e}"))?
}

/// A batch of commands, in order, at the next block. Fire-and-forget: what the engine refuses comes
/// back on the feed; an error means the rest of the batch did not reach it.
#[tauri::command]
pub async fn engine_send(commands: Vec<WireCommand>, state: State<'_, EngineApp>) -> Result<(), String> {
    let host = state.host()?;
    host.send_all(commands.into_iter().map(|c| TimedCommand { frame: None, command: c.0 }))
}

/// Share output's WASAPI render endpoint, or `null` for off.
#[tauri::command]
pub async fn engine_set_share(endpoint: Option<String>, state: State<'_, EngineApp>) -> Result<(), String> {
    let host = state.host()?;
    tauri::async_runtime::spawn_blocking(move || host.set_share(endpoint)).await.map_err(|e| format!("engine_set_share: {e}"))?
}

/// Subscribe `channel` to the feed (replacing the last subscriber); its first frame is a reset.
#[tauri::command]
pub async fn engine_feed(channel: Channel<FeedFrame>, state: State<'_, EngineApp>) -> Result<(), String> {
    let (_, feed) = state.engine.as_ref().ok_or("engine mode is off")?;
    feed.subscribe(move |frame| channel.send(frame).is_ok());
    Ok(())
}
