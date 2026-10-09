mod host;
// ASIO startup coordinator (one probe per process, requested by the frontend after the UI is up).
mod asio_startup;
// Native audio input devices (cpal, WASAPI-shared): the picker's list and the engine's WASAPI pick.
#[cfg(windows)]
mod audio_input;
// Native audio output devices, the backend type and the ASIO driver cache the engine opens from.
#[cfg(windows)]
mod audio_output;
// The native engine's device side (briefing: `engine_io/mod.rs`): engine mode
// (`engine_io::mode`) runs it; a DEV probe drives it too.
#[cfg(windows)]
#[allow(dead_code)]
mod engine_io;
// DEV: the loopback chirp analysis the spike and the engine probe share.
#[cfg(all(windows, debug_assertions))]
mod chirp_lag;
// Stage 1 silent-share probe (docs/ARCHITECTURE.md § Measured premise, S1).
#[cfg(all(windows, debug_assertions))]
mod share_probe;
// The app updater's check and install (`update.rs`).
mod update;

/// A diagnostic sink the frontend invokes once on startup (DEV only) so headless verification can
/// read WebView2-internal facts (crossOriginIsolated, getUserMedia, MIDI, host kind) from
/// `tauri dev` stdout — there is no Playwright into WebView2. Plain command; no capability needed.
#[cfg(debug_assertions)]
#[tauri::command]
fn diag(report: String) {
    println!("[diag] {report}");
    log::info!("diag: {report}");
}

/// Close guard (2026-08-16): a window close used to kill a jam with zero warning. The frontend owns
/// the "is a jam in progress" answer, so CloseRequested is vetoed here and forwarded as an event;
/// the frontend confirms with the user and calls `app_confirm_close` to actually close. If the
/// frontend is gone or hung the event simply goes unanswered — OS "End task" still works.
static CLOSE_ALLOWED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[tauri::command]
fn app_confirm_close(window: tauri::WebviewWindow) -> Result<(), String> {
    allow_close_then(|| window.close().map_err(|e| e.to_string()))
}

/// Open the close guard, then run `close`. The flag must be up BEFORE the call: `close()` raises a
/// fresh `CloseRequested` that the guard would veto otherwise. A failed close drops the flag again,
/// so the next OS close still asks the frontend (audit B6).
fn allow_close_then(close: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    use std::sync::atomic::Ordering::SeqCst;
    CLOSE_ALLOWED.store(true, SeqCst);
    close().map_err(|e| {
        CLOSE_ALLOWED.store(false, SeqCst);
        log::error!("window close failed: {e}");
        format!("could not close the window: {e}")
    })
}

#[cfg(test)]
mod close_guard_tests {
    use super::*;
    use std::sync::atomic::Ordering::SeqCst;

    #[test]
    fn a_failed_close_leaves_the_close_guard_armed() {
        let err = allow_close_then(|| {
            assert!(CLOSE_ALLOWED.load(SeqCst), "the flag is up while close runs");
            Err("injected".to_string())
        });
        assert!(err.is_err());
        assert!(!CLOSE_ALLOWED.load(SeqCst), "a failed close must not bypass the confirm");

        assert!(allow_close_then(|| Ok(())).is_ok());
        assert!(CLOSE_ALLOWED.load(SeqCst));
        CLOSE_ALLOWED.store(false, SeqCst);
    }
}

/// Sink for the frontend's `console.error` (and uncaught errors/rejections). A release WebView2 has
/// no visible console, so the platform layer's log pipe (`src/platform/logging.ts`) forwards every
/// error string here where it lands in the same rotated log file as the Rust side — the one place a
/// friend's crash report can be read after the fact. `target: "webview"` tags the origin.
#[tauri::command]
fn frontend_log(message: String) {
    log::error!(target: "webview", "{message}");
}

/// The release log's folder as Help's Copy diagnostics names it: tauri-plugin-log's `LogDir` target
/// (set up in `run()`) resolves `app_log_dir()`. `%LOCALAPPDATA%` stands in for the user's profile, so
/// a pasted report does not carry the Windows user name; Explorer's address bar expands it.
#[tauri::command]
fn app_log_dir(app: tauri::AppHandle) -> Result<String, String> {
    use tauri::Manager;
    let dir = app.path().app_log_dir().map_err(|e| e.to_string())?;
    let under_local = app.path().local_data_dir().ok().and_then(|local| dir.strip_prefix(local).ok());
    let shown = match under_local {
        Some(rest) => std::path::Path::new("%LOCALAPPDATA%").join(rest),
        None => dir.clone(),
    };
    Ok(shown.display().to_string())
}

/// Help's Open log folder: Explorer on the release log's folder, so a tester can attach the log to a
/// report. No shell plugin: the one fixed folder, no path from the WebView. Explorer is not waited on
/// (it can stay up as the shell), and it exits non-zero even on success.
#[tauri::command]
fn app_open_log_dir(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    let dir = app.path().app_log_dir().map_err(|e| e.to_string())?;
    std::process::Command::new("explorer").arg(&dir).spawn().map(drop).map_err(|e| {
        log::error!("opening the log folder {} failed: {e}", dir.display());
        format!("could not open the log folder: {e}")
    })
}

/// Windows: DENY the WebView2 Web MIDI permission to the app's own pages, without a prompt. One MIDI
/// path per run: native MIDI (`engine_io/midi`, started with the engine) owns the input ports, and a
/// WinMM input port may be exclusive, so a Web MIDI open beside it could take a controller from it or
/// fail on it. Non-sysex Web MIDI surfaces as UNKNOWN_PERMISSION and sysex as
/// MIDI_SYSTEM_EXCLUSIVE_MESSAGES, and the exact kind varies by runtime, so both are denied.
/// Anything else (camera, geolocation, notifications, …) and any request from a foreign origin keeps
/// WebView2's default handling.
#[cfg(windows)]
fn register_midi_permission_deny(window: &tauri::WebviewWindow) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_PERMISSION_KIND, COREWEBVIEW2_PERMISSION_KIND_MIDI_SYSTEM_EXCLUSIVE_MESSAGES,
        COREWEBVIEW2_PERMISSION_KIND_UNKNOWN_PERMISSION, COREWEBVIEW2_PERMISSION_STATE_DENY,
    };
    use webview2_com::PermissionRequestedEventHandler;

    // with_webview dispatches this closure onto the WebView2 UI (main) thread.
    let _ = window.with_webview(|webview| {
        // SAFETY: all WebView2 COM calls must run on the UI thread, which this closure does.
        unsafe {
            let controller = webview.controller();
            let core = match controller.CoreWebView2() {
                Ok(core) => core,
                Err(_) => return,
            };
            let handler = PermissionRequestedEventHandler::create(Box::new(move |_sender, args| {
                if let Some(args) = args {
                    let mut kind = COREWEBVIEW2_PERMISSION_KIND::default();
                    args.PermissionKind(&mut kind)?;
                    let mut uri = windows::core::PWSTR::null();
                    args.Uri(&mut uri)?;
                    let uri = webview2_com::take_pwstr(uri);
                    // Release serves from tauri.localhost, `tauri dev` from the vite port.
                    let own_origin = ["http://tauri.localhost", "http://localhost:1420"]
                        .iter()
                        .any(|o| uri == *o || uri.starts_with(&format!("{o}/")));
                    let midi = kind == COREWEBVIEW2_PERMISSION_KIND_UNKNOWN_PERMISSION
                        || kind == COREWEBVIEW2_PERMISSION_KIND_MIDI_SYSTEM_EXCLUSIVE_MESSAGES;
                    if own_origin && midi {
                        args.SetState(COREWEBVIEW2_PERMISSION_STATE_DENY)?;
                        log::warn!("webview MIDI permission denied (native MIDI owns the ports): kind {}", kind.0);
                    } else {
                        log::warn!("webview permission left to WebView2: kind {} from {uri}", kind.0);
                    }
                }
                Ok(())
            }));
            let mut token = 0i64;
            let _ = core.add_PermissionRequested(&handler, &mut token);
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // P9.1: out-of-process plugin scan child mode. `app.exe --scan-one <path>` loads ONE bundle
    // (CLAP, VST3 or VST2), prints what it holds as JSON, and exits BEFORE Tauri starts — so a
    // crashy plugin bundle takes down only this throwaway child, never the host. The parent's
    // plugin_scan command spawns it per bundle.
    #[cfg(windows)]
    {
        let args: Vec<String> = std::env::args().collect();
        // Native-engine Stage 1 premise spike (`docs/ARCHITECTURE.md` § Measured premise).
        #[cfg(debug_assertions)]
        if let Some(pos) = args.iter().position(|a| a == "--probe-engine-spike") {
            match host::engine_spike_run(&args[pos + 1..]) {
                Ok(()) => std::process::exit(0),
                Err(error) => {
                    eprintln!("[engine-spike] {error}");
                    std::process::exit(1);
                }
            }
        }
        // The native engine's rig probe (`engine_io/probe.rs`).
        #[cfg(debug_assertions)]
        if let Some(pos) = args.iter().position(|a| a == "--probe-engine") {
            match engine_io::probe::run(&args[pos + 1..]) {
                Ok(()) => std::process::exit(0),
                Err(error) => {
                    eprintln!("[engine-probe] {error}");
                    std::process::exit(1);
                }
            }
        }
        #[cfg(debug_assertions)]
        if let Some(pos) = args.iter().position(|a| a == "--probe-share" || a == "--probe-share-child") {
            match share_probe::run(args[pos] == "--probe-share-child", &args[pos + 1..]) {
                Ok(()) => std::process::exit(0),
                Err(error) => {
                    eprintln!("[share-probe] {error}");
                    std::process::exit(1);
                }
            }
        }
        if let Some(pos) = args.iter().position(|a| a == "--scan-one") {
            let path = args.get(pos + 1).map(String::as_str).unwrap_or_default();
            std::process::exit(host::scan_one_main(path));
        }
        // P11.3 ASIO tier: the duplex device + configs are resolved ONCE per process while the driver
        // is free (once a stream holds it, cpal can't re-resolve or re-query), but NOT here: resolving
        // loads the third-party driver DLL in-process, and a broken driver would hang or crash the app
        // before any window exists. The frontend requests the probe (`plugin_asio_probe`) after the UI
        // is up and only if the saved preference is on — `asio_startup.rs` owns the rules.
        // `--disable-asio` is the escape hatch: no probe this launch, whatever the preference says.
        if args.iter().any(|a| a == "--disable-asio") {
            audio_output::set_asio_disabled_by_flag();
        }
    }

    tauri::Builder::default()
        // Shared host state: the WebView document epoch (`host_init`).
        .manage(host::PluginHostState::default())
        // The updater's manifest and public key: `plugins.updater` in `tauri.conf.json`.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            // Field debuggability: register the log plugin UNCONDITIONALLY, not just
            // in debug builds — a release build previously produced ZERO logs, so any "it
            // broke" was unreproducible. Two sinks: Stdout (a load-bearing dev convention — `tauri
            // dev` workflows grep stdout for log::info lines) AND a rotated file under
            // %LOCALAPPDATA%\com.bleeploop.app\logs. KeepAll rotation means a crash report survives
            // restarts (each rotated file is renamed with its date rather than overwritten).
            use tauri_plugin_log::{Target, TargetKind};
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Info)
                    // The updater logs a failed check (offline, no release yet) as ERROR, and hands it
                    // back too; `update.rs` logs every outcome itself, so an ERROR stays a real fault.
                    .level_for("tauri_plugin_updater", log::LevelFilter::Off)
                    .max_file_size(2_000_000) // ~2 MB per file before rotation
                    .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepAll)
                    .targets([
                        Target::new(TargetKind::Stdout),
                        Target::new(TargetKind::LogDir {
                            file_name: Some("bleeploop".into()),
                        }),
                    ])
                    .build(),
            )?;

            // Startup marker — pins the app version at the top of every log/crash report.
            log::info!("BleepLoop v{} starting", env!("CARGO_PKG_VERSION"));

            // Route panics into the log (release WebView2 has no console, and a release panic
            // otherwise vanishes) while chaining to the previous hook so dev stderr backtraces are
            // preserved. The payload is a &str for `panic!("literal")` and a String for formatted
            // panics — try both.
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let payload = info
                    .payload()
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| info.payload().downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic payload>".to_string());
                let location = info
                    .location()
                    .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                    .unwrap_or_else(|| "<unknown location>".to_string());
                log::error!("panic at {location}: {payload}");
                prev(info);
            }));

            #[cfg(windows)]
            {
                use tauri::Manager;
                if let Some(window) = app.get_webview_window("main") {
                    register_midi_permission_deny(&window);
                }
                // The native engine: its device owner, feed and native MIDI, once per launch.
                engine_io::mode::EngineApp::setup(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if !CLOSE_ALLOWED.load(std::sync::atomic::Ordering::SeqCst) {
                    use tauri::Emitter;
                    api.prevent_close();
                    let _ = window.emit("lf://close-requested", ());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            #[cfg(debug_assertions)]
            diag,
            frontend_log,
            app_log_dir,
            app_open_log_dir,
            app_confirm_close,
            update::app_update_check,
            update::app_update_install,
            host::host_init,
            host::plugin_scan,
            // The player's own scan folders (host/folders.rs); the add opens the native dialog itself.
            host::plugin_folders,
            host::plugin_folder_add,
            host::plugin_folder_remove,
            host::plugin_load,
            host::plugin_unload,
            host::plugin_list_loaded,
            host::plugin_set_param,
            host::plugin_list_params,
            // Tone recall (host/tone.rs): a session export's and import's tones.
            host::plugin_tone_take,
            host::plugin_tone_import,
            host::plugin_tone_forget,
            host::plugin_open_editor,
            host::plugin_close_editor,
            // Device pickers (native devices; the engine opens them).
            host::plugin_list_input_devices,
            host::plugin_list_output_devices,
            // The ASIO tier: availability, cached driver, startup probe and driver switch.
            host::plugin_asio_device_info,
            host::plugin_asio_status,
            host::plugin_asio_probe,
            host::plugin_asio_switch,
            host::plugin_asio_drivers,
            // The native engine (`engine_io/mode.rs`).
            #[cfg(windows)]
            engine_io::mode::engine_open,
            #[cfg(windows)]
            engine_io::mode::engine_close,
            #[cfg(windows)]
            engine_io::mode::engine_status,
            #[cfg(windows)]
            engine_io::mode::engine_set_slot_input_channel,
            #[cfg(windows)]
            engine_io::mode::engine_set_share,
            #[cfg(windows)]
            engine_io::mode::engine_feed,
            #[cfg(windows)]
            engine_io::mode::engine_snapshot,
            #[cfg(windows)]
            engine_io::mode::engine_load_session,
            // Native MIDI (`engine_io/midi_mode.rs`): the UI's one ordered input path (its engine
            // commands and note input, synchronous), its subscription, and the learn UI's calls.
            #[cfg(windows)]
            engine_io::midi_mode::input_send,
            #[cfg(windows)]
            engine_io::midi_mode::midi_subscribe,
            #[cfg(windows)]
            engine_io::midi_mode::midi_learn,
            #[cfg(windows)]
            engine_io::midi_mode::midi_cancel_learn,
            #[cfg(windows)]
            engine_io::midi_mode::midi_forget,
            #[cfg(windows)]
            engine_io::midi_mode::midi_set_momentary,
            #[cfg(windows)]
            engine_io::midi_mode::midi_set_hold,
            #[cfg(windows)]
            engine_io::midi_mode::midi_assign,
            #[cfg(windows)]
            engine_io::midi_mode::midi_import_legacy,
            // DEV: the MIDI benchmark's UI stalls (`engine_io/midi_bench.rs`; the frontend's side is
            // `src/platform/host.tauri.ts`, DEV only).
            #[cfg(all(windows, debug_assertions))]
            engine_io::midi_bench::midi_bench_stall_plan,
            #[cfg(all(windows, debug_assertions))]
            engine_io::midi_bench::midi_bench_stall,
            #[cfg(all(windows, debug_assertions))]
            engine_io::midi_bench::midi_bench_clock,
            #[cfg(all(windows, debug_assertions))]
            engine_io::midi_bench::midi_bench_clock_sync,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // The engine closes its device and drops the engine on its own threads, not in a destructor.
            let _ = app;
            #[cfg(windows)]
            if let tauri::RunEvent::Exit = event {
                engine_io::mode::EngineApp::shutdown();
            }
            #[cfg(not(windows))]
            let _ = event;
        });
}
