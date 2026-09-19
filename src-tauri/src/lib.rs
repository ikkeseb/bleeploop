mod host;
// P11.0: native audio-input capture (cpal, WASAPI-shared). Windows-only; the wet signal returns to
// Web Audio via the unchanged P9 hop-1 path, so no boundary change.
#[cfg(windows)]
mod audio_input;
// P11.3: native audio-output monitor (cpal, WASAPI-shared). Windows-only; the low-latency live
// monitor for the wet plugin signal (branch-1 of the split). No boundary change (cpal is Rust-internal).
#[cfg(windows)]
mod audio_output;
#[cfg(all(windows, debug_assertions))]
mod audio_latency_probe;
#[cfg(all(windows, debug_assertions))]
mod marker_probe;

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
fn app_confirm_close(window: tauri::WebviewWindow) {
    CLOSE_ALLOWED.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = window.close();
}

/// Sink for the frontend's `console.error` (and uncaught errors/rejections). A release WebView2 has
/// no visible console, so the platform layer's log pipe (`src/platform/logging.ts`) forwards every
/// error string here where it lands in the same rotated log file as the Rust side — the one place a
/// friend's crash report can be read after the fact. `target: "webview"` tags the origin.
#[tauri::command]
fn frontend_log(message: String) {
    log::error!(target: "webview", "{message}");
}

/// Windows: auto-grant the WebView2 permission requests the app itself makes (Web MIDI, microphone)
/// so navigator.requestMIDIAccess / getUserMedia resolve without a prompt — the desktop-app-native
/// behaviour (P8). WebView2 v149 supports Web MIDI natively, so no midir bridge is needed; only the
/// permission needs granting. Non-sysex Web MIDI surfaces as UNKNOWN_PERMISSION and sysex as
/// MIDI_SYSTEM_EXCLUSIVE_MESSAGES, and the exact kind varies by runtime, so both are granted.
/// Anything else (camera, geolocation, notifications, …) and any request from a foreign origin keeps
/// WebView2's default handling.
#[cfg(windows)]
fn register_permission_autogrant(window: &tauri::WebviewWindow) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_PERMISSION_KIND, COREWEBVIEW2_PERMISSION_KIND_MICROPHONE,
        COREWEBVIEW2_PERMISSION_KIND_MIDI_SYSTEM_EXCLUSIVE_MESSAGES,
        COREWEBVIEW2_PERMISSION_KIND_UNKNOWN_PERMISSION, COREWEBVIEW2_PERMISSION_STATE_ALLOW,
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
                    let wanted = kind == COREWEBVIEW2_PERMISSION_KIND_UNKNOWN_PERMISSION
                        || kind == COREWEBVIEW2_PERMISSION_KIND_MICROPHONE
                        || kind == COREWEBVIEW2_PERMISSION_KIND_MIDI_SYSTEM_EXCLUSIVE_MESSAGES;
                    if own_origin && wanted {
                        args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)?;
                        log::info!("webview permission auto-granted: kind {}", kind.0);
                    } else {
                        log::warn!("webview permission not auto-granted: kind {} from {uri}", kind.0);
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
    // P9.1: out-of-process CLAP scan child mode. `app.exe --scan-one <path>` loads ONE bundle,
    // prints its descriptors as JSON, and exits BEFORE Tauri starts — so a crashy plugin bundle
    // takes down only this throwaway child, never the host. The parent's plugin_scan command
    // spawns it per `.clap`.
    #[cfg(windows)]
    {
        let args: Vec<String> = std::env::args().collect();
        #[cfg(debug_assertions)]
        if let Some(pos) = args.iter().position(|a| a == "--probe-output-latency") {
            match audio_latency_probe::run(&args[pos + 1..]) {
                Ok(()) => std::process::exit(0),
                Err(error) => {
                    eprintln!("[output-latency-probe] {error}");
                    std::process::exit(1);
                }
            }
        }
        if let Some(pos) = args.iter().position(|a| a == "--scan-one") {
            let path = args.get(pos + 1).map(String::as_str).unwrap_or_default();
            std::process::exit(host::scan_one_main(path));
        }
        // P11.3 de-risk: `app.exe --probe-asio` enumerates the ASIO host's devices (proves the cpal
        // `asio` feature compiled + the machine's ASIO drivers are visible), then exits before Tauri.
        if args.iter().any(|a| a == "--probe-asio") {
            audio_input::probe_asio();
            std::process::exit(0);
        }
        // P11.3 ASIO de-risk #2: can cpal build + run an ASIO INPUT and OUTPUT stream on the same
        // full-duplex device simultaneously? (The single ASIO driver can't be re-enumerated while one
        // direction holds it — this tests building both before play.) Runs standalone, then exits.
        if args.iter().any(|a| a == "--probe-asio-duplex") {
            audio_input::probe_asio_duplex();
            std::process::exit(0);
        }
        // P11.3 ASIO tier: resolve + cache the ASIO duplex device + configs NOW, while the single ASIO
        // driver is free. Once a stream (input OR output) seizes the driver, cpal can't re-resolve the
        // device or re-query configs — but both capture + monitor streams still build from this cache
        // (cpal supports ASIO duplex). No-op without the asio feature / on a non-ASIO rig.
        #[cfg(feature = "asio")]
        audio_output::cache_asio();
    }

    tauri::Builder::default()
        // P9.2: shared host state — JS-provided sample rate + the per-slot RT producer handles.
        // The !Send PluginInstance never lives here (State must be Send+Sync); only Send control
        // handles (Arc atomics + the owner-thread JoinHandle) do.
        .manage(host::PluginHostState::default())
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
                // DEV by-ear diagnostic: log whether the RT loop renders at the native rate
                // (prod-realistic, default) or the forced 48k P9.4 drift-gate rate (LF_FORCE_48K=1).
                host::log_render_rate_mode();
                if let Some(window) = app.get_webview_window("main") {
                    register_permission_autogrant(&window);
                }
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
            #[cfg(all(windows, debug_assertions))]
            marker_probe::marker_probe_clock,
            #[cfg(all(windows, debug_assertions))]
            marker_probe::marker_probe_begin,
            #[cfg(all(windows, debug_assertions))]
            marker_probe::marker_probe_cancel,
            #[cfg(all(windows, debug_assertions))]
            marker_probe::marker_probe_result,
            #[cfg(debug_assertions)]
            diag,
            frontend_log,
            app_confirm_close,
            host::host_init,
            host::plugin_scan,
            host::plugin_load,
            host::plugin_unload,
            host::plugin_list_loaded,
            host::plugin_note_on,
            host::plugin_note_off,
            host::plugin_set_param,
            host::plugin_list_params,
            #[cfg(debug_assertions)]
            host::plugin_save_state,
            #[cfg(debug_assertions)]
            host::plugin_load_state,
            host::plugin_open_editor,
            host::plugin_close_editor,
            // P11.0 native audio-input path (guitar → plugin → wet monitor/record).
            host::plugin_list_input_devices,
            host::plugin_arm_input,
            host::plugin_disarm_input,
            // P11.3 native low-latency monitor (wet → cpal output, same device).
            host::plugin_list_output_devices,
            host::plugin_arm_monitor,
            host::plugin_disarm_monitor,
            host::plugin_set_monitor_gain,
            host::plugin_set_master_gain,
            // P11.3 record-latency: cpal_out latency readout for the looper's auto record compensation.
            host::plugin_monitor_latency,
            // P11.3 live buffer-size control (global RT block).
            host::plugin_set_buffer_size,
            // P11.3 ASIO-default: runtime host-tier toggle + availability query (Audio Settings).
            host::plugin_set_asio_enabled,
            host::plugin_asio_available,
            host::plugin_asio_device_info,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
