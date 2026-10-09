//! The app updater (`tauri-plugin-updater`). The release workflow publishes a signed installer and a
//! `latest.json` beside it (`.github/workflows/build-exe.yml`); `tauri.conf.json` names that manifest
//! and the public key the signature must match. The frontend asks (`src/app/update.ts`); nothing
//! installs without the player's press.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri_plugin_updater::UpdaterExt;

/// A newer release: its version and its "What's new" (the release tag's notes).
#[derive(serde::Serialize)]
pub struct UpdateInfo {
    version: String,
    notes: String,
}

/// Ask the release channel for a newer version; `None` when this one is the latest. A failed check
/// (offline, GitHub unreachable) is logged here and answered as an error the frontend ignores.
#[tauri::command]
pub async fn app_update_check(app: tauri::AppHandle) -> Result<Option<UpdateInfo>, String> {
    let checked = match app.updater_builder().timeout(Duration::from_secs(30)).build() {
        Ok(updater) => updater.check().await,
        Err(e) => Err(e),
    };
    match checked {
        Ok(Some(update)) => {
            log::info!("[update] v{} is available", update.version);
            Ok(Some(UpdateInfo { version: update.version, notes: update.body.unwrap_or_default() }))
        }
        Ok(None) => {
            log::info!("[update] this is the latest release");
            Ok(None)
        }
        Err(e) => {
            log::info!("[update] check failed: {e}");
            Err(e.to_string())
        }
    }
}

/// Download the offered update and check its signature, then shut the engine down as an exit does
/// (native MIDI's ports closed, tones saved, plugins unloaded, the device closed) and start the
/// installer, which quits the app and opens the new version (Windows: the plugin exits the process
/// once the installer runs). Returns only on failure.
#[tauri::command]
pub async fn app_update_install(app: tauri::AppHandle) -> Result<(), String> {
    // The plugin's own hook runs `cleanup_before_exit`; this one replaces it and shuts the engine down
    // first. `std::process::exit` skips `RunEvent::Exit`, where the engine's shutdown otherwise runs.
    let engine_closed = Arc::new(AtomicBool::new(false));
    let hook = {
        let app = app.clone();
        let engine_closed = engine_closed.clone();
        move || {
            #[cfg(windows)]
            crate::engine_io::mode::EngineApp::shutdown();
            engine_closed.store(true, Ordering::SeqCst);
            app.cleanup_before_exit();
        }
    };
    let failed = |step: &str, e: tauri_plugin_updater::Error| {
        log::error!("[update] {step} failed: {e}");
        format!("{step} failed: {e}")
    };
    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(180))
        .on_before_exit(hook)
        .build()
        .map_err(|e| failed("setup", e))?;
    let update = updater.check().await.map_err(|e| failed("the check", e))?.ok_or("no update is offered")?;
    log::info!("[update] v{}: downloading", update.version);
    let bytes = update.download(|_, _| {}, || {}).await.map_err(|e| failed("the download", e))?;
    log::info!("[update] v{}: {} bytes, signature verified; starting the installer", update.version, bytes.len());
    let installed = tauri::async_runtime::spawn_blocking(move || update.install(bytes))
        .await
        .map_err(|e| format!("the install did not run: {e}"))?;
    installed.map_err(|e| {
        let message = failed("the install", e);
        if engine_closed.load(Ordering::SeqCst) {
            format!("{message}. The audio engine is already closed: restart BleepLoop")
        } else {
            message
        }
    })
}
