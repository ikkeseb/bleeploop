//! The app updater (`tauri-plugin-updater`). The release workflow publishes a signed installer and a
//! `latest.json` beside it (`.github/workflows/build-exe.yml`); `tauri.conf.json` names that manifest
//! and the public key the signature must match. The frontend asks (`src/app/update.ts`); nothing
//! installs without the player's press, which downloads (the jam plays on), saves the jam as a close
//! does, then installs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::Emitter;
use tauri_plugin_updater::UpdaterExt;

/// A newer release: its version and its "What's new" (the release tag's notes).
#[derive(serde::Serialize)]
pub struct UpdateInfo {
    version: String,
    notes: String,
}

/// How far `app_update_download` and `app_update_install` have got, over `lf://update-progress` (the UI half:
/// `src/platform/host.tauri.ts`). The install has three waits, each long enough to watch, and the
/// download is the only one with a number: the package comes over the network, then its signature is
/// checked over the whole of it, then the installer runs and the app quits. A press with no read-out
/// looks like a hang, which is what the player reported.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "stage")]
enum Progress {
    /// Bytes in so far, with the package's size when the server declared a `Content-Length` (it is a
    /// GitHub release asset, which does, but nothing in the protocol promises one).
    Downloading { downloaded: u64, total: Option<u64> },
    /// Every byte is in; the signature is being checked against the public key.
    Verifying,
    /// The engine is shutting down and the installer is starting. The app quits from here, so this is
    /// the last frame the player sees.
    Installing,
}

/// Without a `Content-Length` there is no percentage, so progress is reported every this many bytes.
const PROGRESS_STEP: u64 = 512 * 1024;

/// A `Progress` to the frontend. Best-effort: a send that fails (no window) must not fail the update.
fn report(app: &tauri::AppHandle, progress: Progress) {
    let _ = app.emit("lf://update-progress", progress);
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

/// A downloaded update whose signature checked, waiting for `app_update_install`, and whether the
/// engine's shutdown hook (`on_before_exit`, built in with it) has run.
struct Downloaded {
    update: tauri_plugin_updater::Update,
    bytes: Vec<u8>,
    engine_closed: Arc<AtomicBool>,
}

/// The update `app_update_download` fetched this launch, until `app_update_install` takes it.
#[derive(Default)]
pub struct UpdateState(Mutex<Option<Downloaded>>);

/// Download the offered update and check its signature while the jam plays on, and keep it for
/// `app_update_install`; one already kept answers at once. Each step reports itself over
/// `lf://update-progress`. The UI saves the jam between the two (`src/app/update.ts`), so a take
/// committed during the download, which can take minutes, is in the recovery the new version restores.
#[tauri::command]
pub async fn app_update_download(app: tauri::AppHandle, state: tauri::State<'_, UpdateState>) -> Result<(), String> {
    if state.0.lock().map_err(|_| "update state poisoned".to_string())?.is_some() {
        return Ok(());
    }
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
    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(180))
        .on_before_exit(hook)
        .build()
        .map_err(|e| failed("setup", e))?;
    let update = updater.check().await.map_err(|e| failed("the check", e))?.ok_or("no update is offered")?;
    log::info!("[update] v{}: downloading", update.version);
    // One event per whole percent (a hundred over a 20 MB package), not one per chunk, which would be
    // thousands. `mark` is that percent, or the 512 KiB block when the server declared no size;
    // `u64::MAX` is no mark yet, so the first chunk always reports and the read-out starts at once.
    let mut downloaded = 0u64;
    let mut mark = u64::MAX;
    let on_chunk = {
        let app = app.clone();
        move |len: usize, total: Option<u64>| {
            downloaded += len as u64;
            let next = match total {
                Some(total) if total > 0 => downloaded.saturating_mul(100) / total,
                _ => downloaded / PROGRESS_STEP,
            };
            if next == mark {
                return;
            }
            mark = next;
            report(&app, Progress::Downloading { downloaded, total });
        }
    };
    // The crate calls this once the last chunk is in, before it checks the signature over the whole
    // package (`Update::download`, tauri-plugin-updater 2.12.0), so this is where verifying starts.
    let on_finish = {
        let app = app.clone();
        move || report(&app, Progress::Verifying)
    };
    let bytes = update.download(on_chunk, on_finish).await.map_err(|e| failed("the download", e))?;
    log::info!("[update] v{}: {} bytes, signature verified", update.version, bytes.len());
    *state.0.lock().map_err(|_| "update state poisoned".to_string())? = Some(Downloaded { update, bytes, engine_closed });
    Ok(())
}

/// Install the update `app_update_download` kept: shut the engine down as an exit does (native MIDI's
/// ports closed, tones saved, plugins unloaded, the device closed) and start the installer, which quits
/// the app and opens the new version (Windows: the plugin exits the process once the installer runs).
/// Returns only on failure: one before the engine's shutdown (the package would not extract) keeps the
/// download for another press, and the jam plays on; one after it leaves no engine
/// (`src-tauri/OPEN-THREADS.md`), and says so.
#[tauri::command]
pub async fn app_update_install(app: tauri::AppHandle, state: tauri::State<'_, UpdateState>) -> Result<(), String> {
    let Downloaded { update, bytes, engine_closed } =
        state.0.lock().map_err(|_| "update state poisoned".to_string())?.take().ok_or("no update is downloaded")?;
    log::info!("[update] v{}: starting the installer", update.version);
    report(&app, Progress::Installing);
    let (installed, update, bytes) = tauri::async_runtime::spawn_blocking(move || {
        let installed = update.install(&bytes);
        (installed, update, bytes)
    })
    .await
    .map_err(|e| format!("the install did not run: {e}"))?;
    let Err(e) = installed else { return Ok(()) };
    let message = failed("the install", e);
    if engine_closed.load(Ordering::SeqCst) {
        return Err(format!("{message}. The audio engine is already closed: restart BleepLoop"));
    }
    if let Ok(mut kept) = state.0.lock() {
        *kept = Some(Downloaded { update, bytes, engine_closed });
    }
    Err(message)
}

/// A failed step, logged and worded for the frontend.
fn failed(step: &str, e: tauri_plugin_updater::Error) -> String {
    log::error!("[update] {step} failed: {e}");
    format!("{step} failed: {e}")
}
