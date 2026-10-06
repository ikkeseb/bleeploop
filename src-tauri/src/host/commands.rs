//! The `#[tauri::command]` IPC surface for the `PluginHost` capability boundary
//! (`src/platform/host.ts`), plus the `--scan-one` child entry point. The plugin commands route to
//! the native engine's slots (`engine_io::plugins`).
//!
//! Contract notes mirrored from `host.ts`:
//!   - `slot` is 0 | 1 (two native slots); we take it as `u8` and validate.
//!   - `loadPlugin` REQUIRES `id`: one `.clap` bundle can export several descriptors, so
//!     `(slot, path)` alone would silently load `descriptor[0]`.
//!   - a tone is a tone file's bytes (`host/tone.rs`), raw both ways: JS sees an
//!     ArrayBuffer and sends a Uint8Array.
//!   - every command returns `Result<_, String>` so a stub/error surfaces as a rejected JS promise
//!     rather than a panic across the IPC boundary.

use super::state::{
    AudioInputDevice, AudioOutputDevice, ParamDesc, PluginDescriptor, PluginFolders, PluginHostState, PluginInfo,
    ToneImport,
};

fn validate_slot(slot: u8) -> Result<(), String> {
    match slot {
        0 | 1 => Ok(()),
        _ => Err(format!("invalid plugin slot {slot} (expected 0 or 1)")),
    }
}

/// The native engine's slots (`engine_io::plugins`); an error when the engine did not start.
#[cfg(windows)]
use crate::engine_io::mode::engine;

/// One call per WebView document: begins its session and answers its `frontendEpoch`, which every
/// `plugin_load` presents so a load from a replaced document cannot park a slot
/// (`engine_io::plugins`). `sample_rate` is the document's `AudioContext` rate, logged only.
#[tauri::command]
pub async fn host_init(
    sample_rate: f64,
    state: tauri::State<'_, PluginHostState>,
) -> Result<u32, String> {
    let frontend_epoch = state.begin_frontend_session();
    log::info!("[plugin_host] host_init: sample_rate={sample_rate} frontend_epoch={frontend_epoch}");
    Ok(frontend_epoch)
}
/// P9.1: hand-rolled out-of-process `walkdir` scan of the CLAP, VST3 and VST2 search paths and the
/// player's own folders (`plugin-folders.json`, `host/folders.rs`). Each bundle is
/// loaded in a short-lived `--scan-one` child process (foreign entry-init code can crash and
/// `catch_unwind` can't contain a C abort — only a process boundary makes a bad bundle survivable),
/// unless the scan cache under the app's local data dir (`plugin-scan.json`) already knows that
/// binary; `force` (the picker's rescan button) bypasses and rewrites it. One scan runs at a time
/// (`scan::scan_all`), on a blocking thread. The plugins it found and cannot host (32-bit, a VST2
/// shell) go to the host state with the scan, for the next `plugin_folders`. Emits the gate diag in
/// debug builds.
#[tauri::command]
pub async fn plugin_scan(app: tauri::AppHandle, force: bool) -> Result<Vec<PluginDescriptor>, String> {
    #[cfg(windows)]
    {
        use tauri::Manager;
        let data_dir = match app.path().app_local_data_dir() {
            Ok(dir) => Some(dir),
            Err(e) => {
                log::warn!("[scan] no app-local data dir ({e}); scanning the built-in folders without a cache");
                None
            }
        };
        tauri::async_runtime::spawn_blocking(move || {
            let cache_path = data_dir.as_ref().map(|dir| dir.join("plugin-scan.json"));
            let folders_path = data_dir.as_ref().map(|dir| dir.join(FOLDERS_FILE));
            let state = app.state::<PluginHostState>();
            super::scan::scan_all(cache_path.as_deref(), folders_path.as_deref(), force, &state.scan_unsupported)
        })
        .await
        .map_err(|e| format!("plugin scan task: {e}"))?
    }
    #[cfg(not(windows))]
    {
        let _ = (app, force);
        Ok(Vec::new())
    }
}

/// The player's plugin folder list, beside the scan cache in the app's local data dir.
#[cfg(windows)]
const FOLDERS_FILE: &str = "plugin-folders.json";

#[cfg(windows)]
fn folders_file(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("app_local_data_dir: {e}"))?;
    Ok(dir.join(FOLDERS_FILE))
}

/// The folder list as Audio Settings shows it, with what the last scan could not host.
#[cfg(windows)]
fn folders_view(state: &PluginHostState, user: &[String]) -> PluginFolders {
    let unsupported = state.scan_unsupported.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    super::folders::view(user, unsupported)
}

/// The folders the scan walks: its built-in roots (read-only) and the player's own
/// (`host/folders.rs`), and the plugins the last scan found in them and cannot host, each with why:
/// one call after a `plugin_scan` describes that scan. An `Err` when the stored list cannot be read;
/// it is then left as it is.
#[tauri::command]
pub async fn plugin_folders(
    app: tauri::AppHandle,
    state: tauri::State<'_, PluginHostState>,
) -> Result<PluginFolders, String> {
    #[cfg(windows)]
    {
        let user = super::folders::load(&folders_file(&app)?)?;
        Ok(folders_view(&state, &user))
    }
    #[cfg(not(windows))]
    {
        let _ = (app, state);
        Err("plugin_folders is Windows-only".to_string())
    }
}

/// Add a plugin folder the player picks in the native folder dialog; `Ok(None)` = cancelled. The
/// path comes from the dialog alone: the WebView supplies none. The dialog runs on the UI thread,
/// owned by the main window it belongs to (`folders::pick_folder`), and its answer comes back over a
/// channel; no lock is held while it is open. The feed is held for as long: the UI thread reads no
/// frame while the dialog is modal on it, and the close resyncs it with one reset (`engine_io/feed.rs`).
#[tauri::command]
pub async fn plugin_folder_add(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, PluginHostState>,
) -> Result<Option<PluginFolders>, String> {
    #[cfg(windows)]
    {
        use std::sync::atomic::Ordering::SeqCst;
        use tauri::Manager;
        let file = folders_file(window.app_handle())?;
        if state.folder_dialog_open.swap(true, SeqCst) {
            return Err("the folder dialog is already open".to_string());
        }
        let (tx, mut rx) = tauri::async_runtime::channel(1);
        let owner = window.clone();
        // From just before the dialog shows until its answer is back, on every path out of here.
        let feed_held = engine().ok().and_then(|app| app.hold_feed());
        let shown = window.run_on_main_thread(move || {
            let picked = owner
                .hwnd()
                .map_err(|e| format!("main window handle: {e}"))
                .and_then(super::folders::pick_folder);
            let _ = tx.try_send(picked);
        });
        // A closure that never ran (the app is closing) drops its sender: `recv` answers None.
        let picked = match shown {
            Ok(()) => rx.recv().await.unwrap_or_else(|| Err("the folder dialog did not run".to_string())),
            Err(e) => Err(format!("folder dialog: {e}")),
        };
        drop(feed_held);
        state.folder_dialog_open.store(false, SeqCst);
        let Some(folder) = picked? else { return Ok(None) };
        let user = super::folders::add(&state.folders_write, &file, &folder)?;
        log::info!("[scan] plugin folder added: {folder}");
        Ok(Some(folders_view(&state, &user)))
    }
    #[cfg(not(windows))]
    {
        let _ = (window, state);
        Err("plugin_folder_add is Windows-only".to_string())
    }
}

/// Remove `path` from the player's plugin folders: only an entry spelled exactly so is taken.
#[tauri::command]
pub async fn plugin_folder_remove(
    path: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, PluginHostState>,
) -> Result<PluginFolders, String> {
    #[cfg(windows)]
    {
        let user = super::folders::remove(&state.folders_write, &folders_file(&app)?, &path)?;
        log::info!("[scan] plugin folder removed: {path}");
        Ok(folders_view(&state, &user))
    }
    #[cfg(not(windows))]
    {
        let _ = (path, app, state);
        Err("plugin_folder_remove is Windows-only".to_string())
    }
}
/// Load plugin `id` from `path` into the engine's `slot` (≤ 15 s) for the document with
/// `frontend_epoch` (`engine_io::plugins`); `tone_token` is the reload token a session import answered, when this load is that reload.
#[tauri::command]
pub async fn plugin_load(
    slot: u8,
    path: String,
    id: String,
    frontend_epoch: u32,
    tone_token: Option<u32>,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, PluginHostState>,
) -> Result<PluginInfo, String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        engine()?.plugin_load(&state, &window, slot, path, id, frontend_epoch, tone_token)
    }
    #[cfg(not(windows))]
    {
        let _ = (&state, &window, &path, &id, frontend_epoch, tone_token);
        Err(format!("plugin_load is Windows-only (slot={slot})"))
    }
}
/// Unload the slot's plugin: it crossfades out of the engine, then its owner tears it down.
#[tauri::command]
pub async fn plugin_unload(slot: u8) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        engine()?.plugin_unload(slot)
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}
/// Frontend-reload wedge resync (2026-07-06): list the plugins currently loaded in the native slots
/// so the frontend can detect slots stranded by a WebView reload (frontend reset to synth defaults
/// while the native slots stayed loaded) and unload them before the next load hits "slot N already
/// has a plugin loaded". `[]` in the web build.
#[tauri::command]
pub async fn plugin_list_loaded() -> Result<Vec<PluginInfo>, String> {
    #[cfg(windows)]
    {
        engine()?.plugin_list_loaded()
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}
/// P9.5: set a parameter. `param_id` must be one the plugin listed (`listParams`) — an unknown id is
/// an `Err` here and never reaches the plugin, because a plugin may crash on it.
#[tauri::command]
pub async fn plugin_set_param(slot: u8, param_id: u32, value: f64) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        engine()?.plugin_set_param(slot, param_id, value)
    }
    #[cfg(not(windows))]
    {
        let _ = (param_id, value);
        Ok(())
    }
}
/// Tone recall (`host/tone.rs`): save the slot's plugin tone now, through its owner, into the store,
/// and answer the tone file's bytes as the raw response (empty: the plugin keeps no state). A session
/// export takes each loaded slot's tone this way.
#[tauri::command]
pub async fn plugin_tone_take(slot: u8) -> Result<tauri::ipc::Response, String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        Ok(tauri::ipc::Response::new(engine()?.plugin_tone_take(slot)?))
    }
    #[cfg(not(windows))]
    {
        Err(format!("plugin_tone_take is Windows-only (slot={slot})"))
    }
}
/// Store a session import's tone for a slot. The raw request body is the tone file's bytes, the
/// `slot` header names the slot and the `plugin` header the plugin session.json names for it
/// (`{ format, path, id }` as JSON, ASCII with `\u` escapes; a tone file of another plugin is
/// refused). The answer names the plugin the tone belongs to and, when the slot holds it now, the token
/// of the reload that hears it; nothing is loaded or swapped here (`EngineApp::plugin_tone_import`).
#[tauri::command]
pub async fn plugin_tone_import(request: tauri::ipc::Request<'_>) -> Result<ToneImport, String> {
    let slot: u8 = request
        .headers()
        .get("slot")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or("plugin_tone_import needs a slot header")?;
    validate_slot(slot)?;
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("plugin_tone_import takes the tone file's bytes as the raw request body".to_string());
    };
    #[cfg(windows)]
    {
        let expected: super::tone::ToneIdentity = request
            .headers()
            .get("plugin")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| serde_json::from_str(v).ok())
            .ok_or("plugin_tone_import needs the plugin the session names (a plugin header)")?;
        engine()?.plugin_tone_import(slot, bytes, &expected)
    }
    #[cfg(not(windows))]
    {
        let _ = bytes;
        Err(format!("plugin_tone_import is Windows-only (slot={slot})"))
    }
}
/// The reload a session import answered `token` for did not happen (the slot moved while the import
/// ran): drop the tone parked for it (`EngineApp::plugin_tone_forget`).
#[tauri::command]
pub async fn plugin_tone_forget(slot: u8, token: u32) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        engine()?.plugin_tone_forget(slot, token)
    }
    #[cfg(not(windows))]
    {
        let _ = token;
        Err(format!("plugin_tone_forget is Windows-only (slot={slot})"))
    }
}
/// P9.5: enumerate the loaded plugin's parameters (stable ids + ranges + live values), so a caller
/// can set a real param id (an owner-thread query).
#[tauri::command]
pub async fn plugin_list_params(slot: u8) -> Result<Vec<ParamDesc>, String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        engine()?.plugin_list_params(slot)
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}
/// P10.0: open the slot's plugin editor, on its owner thread: a plugin-owned floating window, else
/// one embedded in a host window (`clap::editor_open`, `vst3_host::vst3_editor_open`, the VST2
/// owner's `embed_editor`). `mode` is
/// accepted for forward-compat and ignored.
#[tauri::command]
pub async fn plugin_open_editor(slot: u8, mode: String) -> Result<(), String> {
    validate_slot(slot)?;
    let _ = mode;
    #[cfg(windows)]
    {
        engine()?.plugin_open_editor(slot)
    }
    #[cfg(not(windows))]
    {
        Err(format!("plugin_open_editor is Windows-only (slot={slot})"))
    }
}
/// P10.0: hide + destroy the slot's plugin editor (owner thread). Idempotent — closing a
/// non-open editor is a harmless no-op.
#[tauri::command]
pub async fn plugin_close_editor(slot: u8) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        engine()?.plugin_close_editor(slot)
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}
/// P11.0: enumerate native (cpal/WASAPI-shared) capture devices for the input-device picker. Opens
/// no stream, so it runs directly on the command thread. The web build returns `[]` (the boundary
/// stub); this is the native answer.
#[tauri::command]
pub async fn plugin_list_input_devices() -> Result<Vec<AudioInputDevice>, String> {
    #[cfg(windows)]
    {
        let devs = crate::audio_input::list_input_devices()?;
        Ok(devs
            .into_iter()
            .map(|d| AudioInputDevice {
                id: d.id,
                name: d.name,
                channels: d.channels,
            })
            .collect())
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}
/// P11.3: enumerate native output devices for the output picker. Opens no stream → runs on the
/// command thread. The web build returns `[]` (boundary stub).
#[tauri::command]
pub async fn plugin_list_output_devices() -> Result<Vec<AudioOutputDevice>, String> {
    #[cfg(windows)]
    {
        let devs = crate::audio_output::list_output_devices()?;
        Ok(devs
            .into_iter()
            .map(|d| AudioOutputDevice {
                id: d.id,
                name: d.name,
                channels: d.channels,
            })
            .collect())
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

/// ASIO startup status; never touches the driver. Mirrors `AsioStatusReport` in `host.ts`.
#[tauri::command]
pub fn plugin_asio_status() -> crate::asio_startup::AsioStatusReport {
    #[cfg(windows)]
    {
        let report = crate::audio_output::asio_startup_status();
        // Logged so a `tauri dev` grep can see a launch that never probes (DisabledByFlag, saved off).
        log::info!("[asio] status read: {:?} {}", report.status, report.detail);
        report
    }
    #[cfg(not(windows))]
    {
        crate::asio_startup::AsioStatusReport {
            status: crate::asio_startup::AsioStartupStatus::NotCompiled,
            detail: String::new(),
        }
    }
}

/// The ASIO probe's attempt-in-progress marker, in the app's local data dir (`asio_startup.rs`).
#[cfg(windows)]
fn asio_sentinel(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("app_local_data_dir: {e}"))?;
    Ok(dir.join("asio-probe-in-progress"))
}

/// The one-per-process ASIO probe (`asio_startup.rs`). The frontend calls this AFTER the window is up:
/// at boot with `explicit=false` only when the saved preference is on, and from the Audio Settings
/// toggle/Retry with `explicit=true`. `driver` is the saved driver pick (`None` = automatic). Runs on
/// a blocking runtime thread for at most the probe deadline; the returned report is also logged so a
/// `tauri dev` grep sees the decision.
#[tauri::command]
pub async fn plugin_asio_probe(
    app: tauri::AppHandle,
    explicit: bool,
    driver: Option<String>,
) -> Result<crate::asio_startup::AsioStatusReport, String> {
    #[cfg(windows)]
    {
        let sentinel = asio_sentinel(&app)?;
        let report = tauri::async_runtime::spawn_blocking(move || {
            crate::audio_output::probe_asio_driver(&sentinel, explicit, driver)
        })
        .await
        .map_err(|e| format!("asio probe task: {e}"))?;
        log::info!(
            "[asio] probe result (explicit={explicit}): {:?} {}",
            report.status,
            report.detail
        );
        Ok(report)
    }
    #[cfg(not(windows))]
    {
        let _ = (app, explicit, driver);
        Ok(plugin_asio_status())
    }
}

/// Switch the ASIO driver without a restart (`driver`: `None` = automatic): the cached driver is dropped
/// and the new one probed, on the engine's device owner, which closes an ASIO run first and reopens it
/// after (`engine_io::mode::switch_asio`).
#[tauri::command]
pub async fn plugin_asio_switch(
    app: tauri::AppHandle,
    driver: Option<String>,
) -> Result<crate::asio_startup::AsioStatusReport, String> {
    #[cfg(windows)]
    {
        let sentinel = asio_sentinel(&app)?;
        log::info!("[asio] driver switch requested: {driver:?}");
        let report = tauri::async_runtime::spawn_blocking(move || {
            crate::engine_io::mode::switch_asio(move || crate::audio_output::switch_asio_driver(&sentinel, driver))
        })
        .await
        .map_err(|e| format!("asio switch task: {e}"))??;
        log::info!("[asio] driver switch result: {:?} {}", report.status, report.detail);
        Ok(report)
    }
    #[cfg(not(windows))]
    {
        let _ = (app, driver);
        Ok(plugin_asio_status())
    }
}

/// The installed ASIO drivers' names (the SDK's registry list; no driver is loaded). Empty without ASIO.
#[tauri::command]
pub async fn plugin_asio_drivers() -> Result<Vec<String>, String> {
    #[cfg(windows)]
    {
        tauri::async_runtime::spawn_blocking(crate::audio_output::asio_driver_names)
            .await
            .map_err(|e| format!("asio driver list task: {e}"))
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

/// `AsioDeviceInfo` plus the buffer sizes the driver takes (`null` when it did not say) and the rates
/// on offer it runs (`AsioCache::sample_rates`).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AsioDriverInfo {
    #[serde(flatten)]
    device: super::state::AsioDeviceInfo,
    buffer_min: Option<u32>,
    buffer_max: Option<u32>,
    sample_rates: Vec<u32>,
}

/// Read cached metadata only; never enumerate or reopen an ASIO driver held by a live stream.
#[tauri::command]
pub fn plugin_asio_device_info() -> Option<AsioDriverInfo> {
    #[cfg(all(windows, feature = "asio"))]
    {
        crate::audio_output::asio_cache().map(|cache| AsioDriverInfo {
            device: super::state::AsioDeviceInfo {
                name: cache.name.clone(),
                input_channels: cache.in_cfg.channels as u32,
                output_channels: cache.out_cfg.channels as u32,
            },
            buffer_min: cache.buffer_range.map(|(min, _)| min),
            buffer_max: cache.buffer_range.map(|(_, max)| max),
            sample_rates: cache.sample_rates.clone(),
        })
    }
    #[cfg(not(all(windows, feature = "asio")))]
    None
}
/// Entry point for the `--scan-one <path>` child process (P9.1). Loads ONE bundle of any format,
/// prints its outcome as one JSON object on stdout (`scan::ScanOutcome`: `plugins`, its
/// descriptors, and `unsupported`, null or why this build cannot host it), and exits 0. A handled
/// error → stderr + non-zero exit; a hard crash in the bundle's foreign code dies here (the child),
/// never the host. Windows-only — `lib.rs::run()` dispatches `--scan-one` to this before Tauri ever
/// starts.
#[cfg(windows)]
pub fn scan_one_main(path: &str) -> i32 {
    // Wait until the parent has this process in its kill-on-close Job (no gate when run by hand).
    if let Err(e) = super::scan::await_scan_gate() {
        eprintln!("scan_one error: {e}");
        return 3;
    }
    match super::scan::scan_one(path) {
        Ok(outcome) => match serde_json::to_string(&outcome) {
            Ok(j) => {
                println!("{j}");
                0
            }
            Err(e) => {
                eprintln!("scan_one serialize error: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("scan_one error: {e}");
            2
        }
    }
}
