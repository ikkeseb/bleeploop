//! The `#[tauri::command]` IPC surface for the `PluginHost` capability boundary
//! (`src/platform/host.ts`), plus the `--scan-one` child entry point and the DEV render-rate-mode
//! startup log.
//!
//! Contract notes mirrored from `host.ts`:
//!   - `slot` is 0 | 1 (two native slots); we take it as `u8` and validate.
//!   - `loadPlugin` REQUIRES `id`: one `.clap` bundle can export several descriptors, so
//!     `(slot, path)` alone would silently load `descriptor[0]`.
//!   - state is opaque plugin-defined bytes (CLAP `state` ext); JS sees a Uint8Array.
//!   - every command returns `Result<_, String>` so a stub/error surfaces as a rejected JS promise
//!     rather than a panic across the IPC boundary.

use super::state::{AudioInputDevice, AudioOutputDevice, ParamDesc, PluginDescriptor, PluginHostState, PluginInfo};

fn validate_slot(slot: u8) -> Result<(), String> {
    match slot {
        0 | 1 => Ok(()),
        _ => Err(format!("invalid plugin slot {slot} (expected 0 or 1)")),
    }
}

/// In engine mode the engine owns the audio device: the live line's device arms are refused (GO LIVE
/// is the engine's `SetSlotLive`).
#[cfg(windows)]
fn refuse_in_engine_mode(command: &str) -> Result<(), String> {
    if crate::engine_io::mode::active() {
        return Err(format!("{command}: engine mode owns the audio device"));
    }
    Ok(())
}

fn validate_note_event(note: u16, velocity: Option<f64>) -> Result<(), String> {
    if note > 127 {
        return Err(format!("invalid MIDI note {note} (expected 0..=127)"));
    }
    if let Some(velocity) = velocity {
        if !velocity.is_finite() || !(0.0..=1.0).contains(&velocity) {
            return Err(format!(
                "invalid MIDI note-on velocity {velocity} (expected finite 0.0..=1.0)"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod note_validation_tests {
    use super::validate_note_event;

    #[test]
    fn midi_note_and_note_on_velocity_stay_inside_the_protocol_domain() {
        assert!(validate_note_event(0, Some(0.0)).is_ok());
        assert!(validate_note_event(127, None).is_ok());
        assert!(validate_note_event(128, None).is_err());
        assert!(validate_note_event(127, Some(f64::INFINITY)).is_err());
        assert!(validate_note_event(127, Some(f64::NAN)).is_err());
        assert!(validate_note_event(127, Some(-0.1)).is_err());
        assert!(validate_note_event(127, Some(1.0)).is_ok());
        assert!(validate_note_event(127, Some(1.1)).is_err());
    }
}
/// The sample rates `host_init` accepts. Every real `AudioContext` rate sits inside; outside it
/// the RT pacing math (`Duration::from_secs_f64` of a block period) can panic or degenerate.
const SAMPLE_RATE_RANGE: std::ops::RangeInclusive<f64> = 8_000.0..=384_000.0;

fn validate_sample_rate(sample_rate: f64) -> Result<(), String> {
    if sample_rate.is_finite() && SAMPLE_RATE_RANGE.contains(&sample_rate) {
        Ok(())
    } else {
        Err(format!(
            "unsupported sample rate {sample_rate} Hz (expected {}..={} Hz)",
            SAMPLE_RATE_RANGE.start(),
            SAMPLE_RATE_RANGE.end()
        ))
    }
}

#[cfg(test)]
mod sample_rate_tests {
    use super::validate_sample_rate;

    #[test]
    fn host_init_sample_rate_is_bounded_to_real_audio_rates() {
        for ok in [8_000.0, 44_100.0, 48_000.0, 96_000.0, 384_000.0] {
            assert!(validate_sample_rate(ok).is_ok(), "{ok} must be accepted");
        }
        for bad in [
            0.0,
            -48_000.0,
            1e-300,
            f64::MIN_POSITIVE,
            7_999.0,
            384_001.0,
            1e12,
            f64::INFINITY,
            f64::NAN,
        ] {
            assert!(validate_sample_rate(bad).is_err(), "{bad} must be refused");
        }
    }
}

/// JS owns the `AudioContext`; it hands Rust the sample rate at startup so a later `loadPlugin`
/// can `activate()` the plugin at the right rate (P9.2). Persists it into shared state.
#[tauri::command]
pub async fn host_init(
    sample_rate: f64,
    state: tauri::State<'_, PluginHostState>,
) -> Result<u32, String> {
    validate_sample_rate(sample_rate)?;
    #[cfg(windows)]
    let frontend_epoch = super::clap::begin_frontend_session(&state)?;
    state
        .sample_rate
        .store(sample_rate.to_bits(), std::sync::atomic::Ordering::Relaxed);
    #[cfg(windows)]
    {
        log::info!(
            "[plugin_host] host_init: sample_rate={sample_rate} frontend_epoch={frontend_epoch}"
        );
        Ok(frontend_epoch)
    }
    #[cfg(not(windows))]
    {
        log::info!("[plugin_host] host_init: sample_rate={sample_rate}");
        Ok(0)
    }
}
/// P9.1: hand-rolled out-of-process `walkdir` scan of the CLAP + VST3 search paths. Each bundle is
/// loaded in a short-lived `--scan-one` child process (foreign entry-init code can crash and
/// `catch_unwind` can't contain a C abort — only a process boundary makes a bad bundle survivable),
/// unless the scan cache under the app's local data dir (`plugin-scan.json`) already knows that
/// binary; `force` (the picker's rescan button) bypasses and rewrites it. Emits the gate diag in
/// debug builds.
#[tauri::command]
pub async fn plugin_scan(app: tauri::AppHandle, force: bool) -> Result<Vec<PluginDescriptor>, String> {
    #[cfg(windows)]
    {
        use tauri::Manager;
        let cache_path = match app.path().app_local_data_dir() {
            Ok(dir) => Some(dir.join("plugin-scan.json")),
            Err(e) => {
                log::warn!("[scan] no app-local data dir ({e}); scanning without a cache");
                None
            }
        };
        super::scan::scan_all(cache_path.as_deref(), force)
    }
    #[cfg(not(windows))]
    {
        let _ = (app, force);
        Ok(Vec::new())
    }
}
/// P9.2/P9.3: `PluginEntry::load` → `PluginInstance::new` → `activate`, then drive `process()` on a
/// device-less high-priority RT thread that sums L+R→mono and writes the hop-1 WebView2 SharedBuffer
/// ring. The `!Send` instance lives on a dedicated owner thread (clack's "main thread"); only the
/// Send `Stopped` processor crosses to the RT thread (and back, for `deactivate`, at unload). The
/// SharedBuffer is created on the UI thread (`window`) and posted to JS once per load.
#[tauri::command]
pub async fn plugin_load(
    slot: u8,
    path: String,
    id: String,
    frontend_epoch: u32,
    load_token: u32,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, PluginHostState>,
) -> Result<PluginInfo, String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        // Dispatch by bundle extension: `.vst3` (file or folder bundle) → the VST3 host; everything
        // else (`.clap`) → the CLAP host. The control plane downstream (event ring, shared buffer,
        // gate) is identical; only the upstream render differs.
        if path.to_ascii_lowercase().ends_with(".vst3") {
            super::clap::vst3_load(&state, &window, slot, path, id, frontend_epoch, load_token)
        } else {
            super::clap::load(&state, &window, slot, path, id, frontend_epoch, load_token)
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (&state, &window, &path, &id, frontend_epoch, load_token);
        Err(format!("plugin_load is Windows-only (slot={slot})"))
    }
}
/// P9.2/P9.3: signal the producer to stop, join the owner thread (which `stop_processing()`s on the
/// RT thread, ships the `Stopped` back, and `deactivate`s on the owner/main thread), then `Close()`
/// the WebView2 SharedBuffer on the UI thread and drop the slot. JS releases its own view first
/// (`plugin-bridge.teardown` → `chrome.webview.releaseBuffer`) before invoking this.
#[tauri::command]
pub async fn plugin_unload(
    slot: u8,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, PluginHostState>,
) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        super::clap::unload(&state, &window, slot)
    }
    #[cfg(not(windows))]
    {
        let _ = (&state, &window);
        Ok(())
    }
}
/// Frontend-reload wedge resync (2026-07-06): list the plugins currently loaded in the native slots
/// so the frontend can detect slots stranded by a WebView reload (frontend reset to synth defaults
/// while the native slots stayed loaded) and unload them before the next load hits "slot N already
/// has a plugin loaded". Reads shared state only (no owner hop, no loaded plugin needed); `[]` in the
/// web build.
#[tauri::command]
pub async fn plugin_list_loaded(
    state: tauri::State<'_, PluginHostState>,
) -> Result<Vec<PluginInfo>, String> {
    #[cfg(windows)]
    {
        super::clap::list_loaded(&state)
    }
    #[cfg(not(windows))]
    {
        let _ = &state;
        Ok(Vec::new())
    }
}
/// P9.5: route a note-on to the plugin. Enqueues a `NoteOnEvent` onto the slot's main→audio rtrb
/// event ring; the RT producer drains it into the next block's `InputEvents`. `velocity` is the CLAP
/// 0..1 normalised form (the JS sink divides MIDI velocity by 127). Note events never touch a
/// main-thread plugin call — they ride the standard process-input event queue (CLAP-correct).
#[tauri::command]
pub async fn plugin_note_on(
    slot: u8,
    note: u16,
    velocity: f64,
    state: tauri::State<'_, PluginHostState>,
) -> Result<(), String> {
    validate_slot(slot)?;
    validate_note_event(note, Some(velocity))?;
    #[cfg(windows)]
    {
        super::clap::enqueue_event(&state, slot, super::clap::PluginEvent::NoteOn { key: note, velocity })
    }
    #[cfg(not(windows))]
    {
        let _ = (&state, note, velocity);
        Ok(())
    }
}
/// P9.5: route a note-off to the plugin. Enqueues a `NoteOffEvent` (matched by key, wildcard
/// note_id) onto the slot's event ring. A full ring is an `Err` (the note would otherwise stick
/// silently); the push waits on nothing but the producer Mutex.
#[tauri::command]
pub async fn plugin_note_off(
    slot: u8,
    note: u16,
    state: tauri::State<'_, PluginHostState>,
) -> Result<(), String> {
    validate_slot(slot)?;
    validate_note_event(note, None)?;
    #[cfg(windows)]
    {
        super::clap::enqueue_event(&state, slot, super::clap::PluginEvent::NoteOff { key: note })
    }
    #[cfg(not(windows))]
    {
        let _ = (&state, note);
        Ok(())
    }
}
/// P9.5: enqueue a `ParamValueEvent` onto the main→audio rtrb event ring (params are set on the
/// audio thread via the process-input queue, never a direct main-thread setter). `param_id` must
/// be one the plugin listed (`listParams`) — an unknown id is an `Err` here and never reaches the
/// ring, because a plugin may crash on it. A full ring is an `Err` too.
#[tauri::command]
pub async fn plugin_set_param(
    slot: u8,
    param_id: u32,
    value: f64,
    state: tauri::State<'_, PluginHostState>,
) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        super::clap::set_param(&state, slot, param_id, value)
    }
    #[cfg(not(windows))]
    {
        let _ = (&state, param_id, value);
        Ok(())
    }
}
/// P9.5: serialise the live plugin via clack's `state` extension. The `!Send` instance lives on the
/// owner thread, so this hands a request to it and waits for the bytes back (state save/load is a
/// CLAP main-thread call). DEV-only until production save/recall ships; JS receives a Uint8Array.
#[cfg(debug_assertions)]
#[tauri::command]
pub async fn plugin_save_state(
    slot: u8,
    state: tauri::State<'_, PluginHostState>,
) -> Result<Vec<u8>, String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        super::clap::save_state(&state, slot)
    }
    #[cfg(not(windows))]
    {
        let _ = &state;
        Err(format!("plugin_save_state is Windows-only (slot={slot})"))
    }
}
/// P9.5: restore opaque plugin-defined bytes via clack's `state` extension (owner/main thread).
/// DEV-only until production save/recall ships.
#[cfg(debug_assertions)]
#[tauri::command]
pub async fn plugin_load_state(
    slot: u8,
    bytes: Vec<u8>,
    state: tauri::State<'_, PluginHostState>,
) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        super::clap::load_state(&state, slot, bytes)
    }
    #[cfg(not(windows))]
    {
        let _ = (&state, bytes);
        Err(format!("plugin_load_state is Windows-only (slot={slot})"))
    }
}
/// P9.5: enumerate the loaded plugin's parameters (stable ids + ranges). Drives the future param UI
/// and lets a caller pick a real param id to `setParameter` (a CLAP main-thread `params` query).
#[tauri::command]
pub async fn plugin_list_params(
    slot: u8,
    state: tauri::State<'_, PluginHostState>,
) -> Result<Vec<ParamDesc>, String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        super::clap::list_params(&state, slot)
    }
    #[cfg(not(windows))]
    {
        let _ = &state;
        Ok(Vec::new())
    }
}
/// P10.0: open the slot's plugin editor in a floating OS window (CLAP `gui` ext). In floating mode the
/// plugin owns + pumps its own window; the host only negotiates WIN32/floating, calls `create`, pins
/// it transient to the main window, and `show`s it — all on the owner/main thread via the
/// `OwnerRequest` channel. `mode` is accepted for forward-compat; P10.0 is floating-only (the
/// embedded path + silent fallback is P10.3).
#[tauri::command]
pub async fn plugin_open_editor(
    slot: u8,
    mode: String,
    state: tauri::State<'_, PluginHostState>,
) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        let _ = mode; // P10.0: always floating
        super::clap::open_editor(&state, slot)
    }
    #[cfg(not(windows))]
    {
        let _ = (&state, mode);
        Err(format!("plugin_open_editor is Windows-only (slot={slot})"))
    }
}
/// P10.0: hide + destroy the slot's plugin editor (owner/main thread). Idempotent — closing a
/// non-open editor is a harmless no-op.
#[tauri::command]
pub async fn plugin_close_editor(
    slot: u8,
    state: tauri::State<'_, PluginHostState>,
) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        super::clap::close_editor(&state, slot)
    }
    #[cfg(not(windows))]
    {
        let _ = &state;
        Ok(())
    }
}
/// P11.0: enumerate native (cpal/WASAPI-shared) capture devices for the input-device picker. Opens
/// no stream, so it runs directly on the command thread — no owner-thread hop or loaded plugin
/// needed. The web build returns `[]` (the boundary stub); this is the native answer.
#[tauri::command]
pub async fn plugin_list_input_devices(
    state: tauri::State<'_, PluginHostState>,
) -> Result<Vec<AudioInputDevice>, String> {
    let _ = &state;
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
/// P11.0: arm a hardware input on the slot's plugin — open a cpal capture stream on `device_id`
/// (None = default), isolating `channel` (None = auto), and feed it into the plugin's audio input
/// bus. The `!Send` cpal `Stream` lives on the owner thread (like the editor window), so this rides
/// the `OwnerRequest` channel. Errors cleanly if the slot's plugin has no audio input bus (a synth)
/// — the slot is left disarmed.
#[tauri::command]
pub async fn plugin_arm_input(
    slot: u8,
    device_id: Option<String>,
    channel: Option<u32>,
    state: tauri::State<'_, PluginHostState>,
) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        refuse_in_engine_mode("plugin_arm_input")?;
        super::clap::arm_input(&state, slot, device_id, channel)
    }
    #[cfg(not(windows))]
    {
        let _ = (&state, device_id, channel);
        Err(format!("plugin_arm_input is Windows-only (slot={slot})"))
    }
}
/// P11.0: disarm the slot's hardware input (drop the cpal stream). Idempotent — disarming an
/// unarmed slot is a benign no-op (the RT loop just keeps draining an empty input ring → silence).
#[tauri::command]
pub async fn plugin_disarm_input(
    slot: u8,
    state: tauri::State<'_, PluginHostState>,
) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        super::clap::disarm_input(&state, slot)
    }
    #[cfg(not(windows))]
    {
        let _ = &state;
        Ok(())
    }
}
/// P11.3: enumerate native output devices for the monitor picker. Opens no stream → runs on the
/// command thread (no owner hop / loaded plugin needed). The web build returns `[]` (boundary stub).
#[tauri::command]
pub async fn plugin_list_output_devices(
    state: tauri::State<'_, PluginHostState>,
) -> Result<Vec<AudioOutputDevice>, String> {
    let _ = &state;
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
/// P11.3: arm the native low-latency monitor on the slot — open a cpal OUTPUT stream on `device_id`
/// (None = default output) fed the wet plugin signal (branch-1), bypassing the WebView2 round-trip.
/// The `!Send` cpal `Stream` lives on the owner thread, so this rides the `OwnerRequest` channel.
#[tauri::command]
pub async fn plugin_arm_monitor(
    slot: u8,
    device_id: Option<String>,
    state: tauri::State<'_, PluginHostState>,
) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        refuse_in_engine_mode("plugin_arm_monitor")?;
        super::clap::arm_monitor(&state, slot, device_id)
    }
    #[cfg(not(windows))]
    {
        let _ = (&state, device_id);
        Err(format!("plugin_arm_monitor is Windows-only (slot={slot})"))
    }
}
/// P11.3: disarm the slot's native monitor (drop the cpal output stream). Idempotent.
#[tauri::command]
pub async fn plugin_disarm_monitor(
    slot: u8,
    state: tauri::State<'_, PluginHostState>,
) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        super::clap::disarm_monitor(&state, slot)
    }
    #[cfg(not(windows))]
    {
        let _ = &state;
        Ok(())
    }
}
/// P11.3: set the slot's native-monitor output gain (linear; the JS output slider drives this when the
/// native monitor is armed, so the heard level tracks the slider even though the web monitor is muted).
/// Stored directly into the slot's `monitor_gain` atomic (no owner hop). No-ops in the web build.
#[tauri::command]
pub async fn plugin_set_monitor_gain(
    slot: u8,
    gain: f32,
    state: tauri::State<'_, PluginHostState>,
) -> Result<(), String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        super::clap::set_monitor_gain(&state, slot, gain)
    }
    #[cfg(not(windows))]
    {
        let _ = (&state, gain);
        Ok(())
    }
}
/// P11.3: set the process-wide master factor for the native wet-monitor path (linear, 0..1).
/// Stored directly into the output callback's atomic; it never touches the Web Audio record tap.
#[tauri::command]
pub async fn plugin_set_master_gain(gain: f32) -> Result<(), String> {
    #[cfg(windows)]
    {
        crate::audio_output::set_master_gain(gain);
    }
    #[cfg(not(windows))]
    {
        let _ = gain;
    }
    Ok(())
}
/// P11.3 record-latency: the slot's native-monitor output ("cpal_out") latency in SECONDS, read by the
/// looper's automatic record-latency compensation at record-arm time (cached per arm). 0.0 in the web
/// build / when the slot's monitor is disarmed. See `clap::monitor_latency_seconds`.
#[tauri::command]
pub async fn plugin_monitor_latency(
    slot: u8,
    state: tauri::State<'_, PluginHostState>,
) -> Result<f64, String> {
    validate_slot(slot)?;
    #[cfg(windows)]
    {
        super::clap::monitor_latency_seconds(&state, slot)
    }
    #[cfg(not(windows))]
    {
        let _ = &state;
        Ok(0.0)
    }
}
/// P11.3: set the global RT buffer size (frames) — the dominant native monitor-latency knob. One
/// process-wide value that re-paces both producer loops without a plugin reload. Validated against the
/// allowed set (mirrors JS `BUFFER_FRAMES_OPTIONS`). No-ops in the web build.
#[tauri::command]
pub async fn plugin_set_buffer_size(frames: u32) -> Result<(), String> {
    const ALLOWED: [u32; 5] = [64, 128, 256, 512, 1024];
    if !ALLOWED.contains(&frames) {
        return Err(format!("invalid buffer size {frames}"));
    }
    #[cfg(windows)]
    {
        super::clap::set_buffer_size(frames);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}
/// P11.3 ASIO-default: set the runtime preference for the ASIO low-latency tier (the Audio Settings
/// toggle). When ON and an ASIO device is present, capture + monitor use ASIO; OFF forces WASAPI.
/// Next-arm effect (a live stream keeps the host it was opened with). Backed by a process-global atomic,
/// so no `state` is needed. No-ops without the `asio` feature / on a non-Windows build.
#[tauri::command]
pub fn plugin_set_asio_enabled(enabled: bool) {
    #[cfg(windows)]
    {
        crate::audio_output::set_asio_enabled(enabled);
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
    }
}
/// P11.3 ASIO-default: whether an ASIO low-latency device is available to select (the `asio` feature is
/// compiled AND a device was cached at startup). The Audio Settings toggle reads this to enable itself;
/// false in a WASAPI-only / web build.
#[tauri::command]
pub fn plugin_asio_available() -> bool {
    #[cfg(windows)]
    {
        crate::audio_output::asio_available()
    }
    #[cfg(not(windows))]
    {
        false
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

/// The one-per-process ASIO probe (`asio_startup.rs`). The frontend calls this AFTER the window is up:
/// at boot with `explicit=false` only when the saved preference is on, and from the Audio Settings
/// toggle/Retry with `explicit=true`. Runs on a blocking runtime thread for at most the probe deadline;
/// the returned report is also logged so a `tauri dev` grep sees the decision.
#[tauri::command]
pub async fn plugin_asio_probe(
    app: tauri::AppHandle,
    explicit: bool,
) -> Result<crate::asio_startup::AsioStatusReport, String> {
    #[cfg(windows)]
    {
        use tauri::Manager;
        let dir = app
            .path()
            .app_local_data_dir()
            .map_err(|e| format!("app_local_data_dir: {e}"))?;
        let sentinel = dir.join("asio-probe-in-progress");
        let report = tauri::async_runtime::spawn_blocking(move || {
            crate::audio_output::probe_asio_startup(&sentinel, explicit)
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
        let _ = (app, explicit);
        Ok(plugin_asio_status())
    }
}

/// Read cached metadata only; never enumerate or reopen an ASIO driver held by a live stream.
#[tauri::command]
pub fn plugin_asio_device_info() -> Option<super::state::AsioDeviceInfo> {
    #[cfg(all(windows, feature = "asio"))]
    {
        crate::audio_output::asio_cache().map(|cache| super::state::AsioDeviceInfo {
            name: cache.name.clone(),
            input_channels: cache.in_cfg.channels as u32,
            output_channels: cache.out_cfg.channels as u32,
        })
    }
    #[cfg(not(all(windows, feature = "asio")))]
    None
}
/// Entry point for the `--scan-one <path>` child process (P9.1). Loads ONE bundle, prints its
/// descriptors as a JSON array on stdout, and exits 0. A handled error → stderr + non-zero exit; a
/// hard crash in the bundle's foreign code dies here (the child), never the host. Windows-only —
/// `lib.rs::run()` dispatches `--scan-one` to this before Tauri ever starts.
#[cfg(windows)]
pub fn scan_one_main(path: &str) -> i32 {
    // Wait until the parent has this process in its kill-on-close Job (no gate when run by hand).
    if let Err(e) = super::scan::await_scan_gate() {
        eprintln!("scan_one error: {e}");
        return 3;
    }
    match super::scan::scan_one(path) {
        Ok(descs) => match serde_json::to_string(&descs) {
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
/// DEV startup diagnostic: trigger the one-shot render-rate-mode log (native vs `LF_FORCE_48K`=48k)
/// so a `tauri dev` shows the mode immediately in stdout, without waiting for a plugin load. Idempotent
/// (the value is cached in a `OnceLock`); called once from `run()`'s setup after the logger is up.
#[cfg(windows)]
pub fn log_render_rate_mode() {
    let _ = super::transport::lf_force_48k();
}
