//! P11.3 — native audio OUTPUT monitor (cpal, WASAPI-shared). Branch-1 of the wet-signal split.
//!
//! The hosted plugin's processed ("wet") mono output is written DIRECTLY to a cpal output stream on
//! the same physical device as the capture (one crystal), bypassing the WebView2 round-trip (branch-2
//! — the P9 hop-1 ring → Web Audio split, which carries ~30ms of hop-2 fill + WebView2 output
//! latency and stays connected to the looper record tap). Same device in+out ⇒ the low-latency LIVE
//! monitor.
//!
//! Mirror of `audio_input.rs`, reversed: the RT producer loop fills the host side of an `rtrb` ring
//! (the `Producer`), and THIS cpal output callback drains it (the `Consumer`), duplicating the mono
//! wet signal across all output channels and applying the monitor gain. **No PCM crosses the
//! capability boundary** — cpal is Rust-internal; the wet signal still also reaches Web Audio via the
//! unchanged P9 AudioNode path (branch-2).
//!
//! cpal 0.18 facts mirrored from `audio_input.rs`: `StreamConfig` is `Copy`/by-value; `sample_rate` is
//! a bare `u32` alias; `description()`/`id()` replace the deprecated `name()`; `stream.play()` is
//! MANDATORY (streams return paused). WASAPI is shared-mode only here (the asio feature is the future
//! low-latency tier) → `BufferSize::Default` (the shared-mode period is engine-fixed).
#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicI8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Sample, SampleFormat, StreamConfig};
use rtrb::Consumer;

/// P11.3 declick: length (seconds) of the per-sample envelope that ramps the native monitor to/from
/// silence on disarm/arm, so the cpal stream is built/torn down on a (near-)zero sample instead of a
/// step discontinuity (the audible click). ~10ms is inaudible as a fade but removes the transient; the
/// callback advances a linear envelope toward the atomic `fade` target each frame.
const DECLICK_SECONDS: f32 = 0.010;

/// Callback-local, allocation-free median of the driver's output presentation delay. A stream must
/// supply three valid reports before this replaces the callback-period fallback. Invalid timestamps
/// never enter the window; the absolute ASIO epoch is not used as a clock.
struct OutputLatencyWindow {
    values: [u64; 31],
    count: usize,
    next: usize,
}

impl OutputLatencyWindow {
    fn new() -> Self {
        Self { values: [0; 31], count: 0, next: 0 }
    }

    fn observe(&mut self, timestamp: cpal::OutputStreamTimestamp) -> Option<u64> {
        let delay = timestamp.playback.checked_duration_since(timestamp.callback)?;
        if delay.is_zero() || delay > std::time::Duration::from_secs(1) {
            return None;
        }
        self.values[self.next] = delay.as_nanos() as u64;
        self.next = (self.next + 1) % self.values.len();
        self.count = (self.count + 1).min(self.values.len());
        if self.count < 3 {
            return None;
        }
        let mut sorted = self.values;
        sorted[..self.count].sort_unstable();
        Some((sorted[(self.count - 1) / 2] + sorted[self.count / 2]) / 2)
    }
}

#[cfg(test)]
mod latency_tests {
    use super::OutputLatencyWindow;
    use cpal::{OutputStreamTimestamp, StreamInstant};
    use std::time::Duration;

    fn timestamp(callback: StreamInstant, ms: u64) -> OutputStreamTimestamp {
        OutputStreamTimestamp { callback, playback: callback + Duration::from_millis(ms) }
    }

    #[test]
    fn median_ignores_startup_outlier_and_rejects_invalid_reports() {
        let mut window = OutputLatencyWindow::new();
        let start = StreamInstant::from_nanos(100_000_000);
        assert_eq!(window.observe(timestamp(start, 0)), None);
        assert_eq!(window.observe(timestamp(start, 900)), None);
        assert_eq!(window.observe(timestamp(start, 14)), None);
        assert_eq!(window.observe(timestamp(start, 14)), Some(14_000_000));
        assert_eq!(window.observe(timestamp(start, 1001)), None);
        assert_eq!(window.observe(OutputStreamTimestamp {
            callback: start, playback: StreamInstant::from_nanos(0),
        }), None);
        assert_eq!(window.observe(timestamp(start, 14)), Some(14_000_000));
        for _ in 0..31 {
            window.observe(timestamp(start, 8));
        }
        assert_eq!(window.observe(timestamp(start, 8)), Some(8_000_000));
    }

    #[test]
    fn delta_survives_asio_epoch_wrap_and_crosses_u64_nanoseconds() {
        let mut window = OutputLatencyWindow::new();
        for ns in [u64::MAX - 1, 0, u64::MAX] {
            let reported = window.observe(timestamp(StreamInstant::from_nanos(ns), 14));
            if ns == u64::MAX {
                assert_eq!(reported, Some(14_000_000));
            }
        }
    }
}

/// The user-facing master fader for every native monitor stream (linear, 0..1). Process-global so
/// CLAP and VST3 slots share the same master and newly armed streams inherit its current value.
/// The output callback reads it directly; no owner-request hop or stream rebuild sits on a drag.
static MASTER_GAIN: AtomicU32 = AtomicU32::new(1.0f32.to_bits());

/// Update the process-global native-monitor master. The Web Audio half is driven separately by
/// `src/audio/master.ts`; this factor touches only the audible cpal path, never `recordTap`.
pub fn set_master_gain(gain: f32) {
    MASTER_GAIN.store(gain.max(0.0).min(1.0).to_bits(), Relaxed);
}

/// One enumerated output device. Mirrors `audio_input::InputDeviceInfo` 1:1 → the
/// `AudioOutputDevice` boundary struct `plugin_list_output_devices` returns to JS.
pub struct OutputDeviceInfo {
    pub id: String,
    pub name: String,
    pub channels: u32,
}

/// Enumerate WASAPI-shared output devices. Opens NO stream, so it is safe to call off the command
/// thread (no owner-thread hop). Mirror of `audio_input::list_input_devices`.
pub fn list_output_devices() -> Result<Vec<OutputDeviceInfo>, String> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|e| format!("cpal output_devices: {e}"))?;
    let mut out = Vec::new();
    for dev in devices {
        let name = dev
            .description()
            .map(|d| d.to_string())
            .unwrap_or_else(|_| "Unknown output".to_string());
        let id = dev
            .id()
            .map(|i| i.to_string())
            .unwrap_or_else(|_| name.clone());
        let channels = dev
            .default_output_config()
            .map(|c| c.channels() as u32)
            .unwrap_or(0);
        out.push(OutputDeviceInfo { id, name, channels });
    }
    Ok(out)
}

/// The ASIO device + its in/out configs, captured ONCE while the single ASIO driver is FREE (startup).
/// Why cache the whole Device, not just the config: once ANY stream (input OR output) holds the ASIO
/// driver, cpal can no longer RE-RESOLVE the device (`default_output_device()` → None) NOR re-query
/// configs (`default_*_config()` → Err) — yet it CAN still build the *other* direction's stream from a
/// device object obtained earlier (proven: cpal runs ASIO input+output duplex on one driver; and
/// `cpal::Device` is Send+Sync, so it lives in a static). So both `open_input_stream` and
/// `open_output_stream` build from this one cached device + config rather than re-resolving. WASAPI is
/// unaffected (it resolves fresh each time). Populated by the one-per-process probe (`probe_asio_startup`),
/// requested by the frontend after the UI is up and before any arm.
#[cfg(feature = "asio")]
pub struct AsioCache {
    pub name: String,
    pub device: cpal::Device,
    pub in_cfg: StreamConfig,
    pub in_fmt: SampleFormat,
    pub out_cfg: StreamConfig,
    pub out_fmt: SampleFormat,
}
/// The ONE startup coordinator: owns the probe state machine (`asio_startup.rs`) and publishes the
/// cache. Present in every build so status/probe commands answer uniformly; `compiled` tells the
/// frontend whether ASIO can exist at all.
#[cfg(feature = "asio")]
static ASIO_PROBE: crate::asio_startup::Coordinator<AsioCache> = crate::asio_startup::Coordinator::new(true);
#[cfg(not(feature = "asio"))]
static ASIO_PROBE: crate::asio_startup::Coordinator<()> = crate::asio_startup::Coordinator::new(false);

/// Read the cached ASIO device + configs (None until a probe succeeded, or on a non-ASIO rig).
#[cfg(feature = "asio")]
pub fn asio_cache() -> Option<&'static AsioCache> {
    ASIO_PROBE.payload()
}

/// `--disable-asio` launch policy: recorded once in `run()`, before any command can arrive.
pub fn set_asio_disabled_by_flag() {
    ASIO_PROBE.set_disabled_by_flag();
}

/// Current probe status; never touches the driver.
pub fn asio_startup_status() -> crate::asio_startup::AsioStatusReport {
    ASIO_PROBE.status()
}

/// Deadline for one probe. The device query normally completes well inside a second; a driver that
/// takes longer is treated as hung for this process (see `asio_startup.rs` for why no retry follows).
const ASIO_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Run (or refuse) the one-per-process ASIO probe. Called from the `plugin_asio_probe` command, which
/// the frontend issues AFTER the window is up and only when the saved preference is on (`explicit`
/// false) or the user asks (`explicit` true). `sentinel` lives in the app's local data dir.
pub fn probe_asio_startup(sentinel: &std::path::Path, explicit: bool) -> crate::asio_startup::AsioStatusReport {
    #[cfg(feature = "asio")]
    {
        ASIO_PROBE.probe(sentinel, explicit, resolve_asio_cache, ASIO_PROBE_TIMEOUT)
    }
    #[cfg(not(feature = "asio"))]
    {
        let _ = (sentinel, explicit);
        ASIO_PROBE.status()
    }
}

/// P11.3 ASIO-default: the runtime preference for the ASIO low-latency tier. Default ON — ASIO is the
/// default monitor/capture path whenever a usable ASIO device is present. The Audio Settings toggle
/// flips this via `plugin_set_asio_enabled`; it takes effect on the NEXT arm (a live stream keeps the
/// host it was opened with — same "applies on next arm" rule as the device/buffer pickers). Lives in
/// every build so the command + persistence layer stay uniform, but it only gates anything when the
/// `asio` feature is compiled AND a device was cached at startup (see `use_asio`).
pub static ASIO_ENABLED: AtomicBool = AtomicBool::new(true);

/// Backend captured once at the start of an owner-thread arm request. Stream open helpers receive
/// this value explicitly so a concurrent preference change cannot split one request across ASIO and
/// WASAPI. `NativeIo` also stores it beside each live stream; the global preference is not ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioBackend {
    Wasapi,
    Asio,
}

impl AudioBackend {
    pub fn selected() -> Self {
        if use_asio() {
            Self::Asio
        } else {
            Self::Wasapi
        }
    }

    pub fn is_asio(self) -> bool {
        self == Self::Asio
    }
}

/// Set the ASIO-tier preference (Audio Settings toggle). Next-arm effect; no live stream is touched.
pub fn set_asio_enabled(enabled: bool) {
    ASIO_ENABLED.store(enabled, Relaxed);
}

/// P11.3 direction A — which slot currently holds the single ASIO duplex driver, or -1 = free. ASIO is
/// ONE driver / ONE coordinated in+out buffer set; two slots cannot each hold an ASIO stream (cpal's
/// reuse-path build on a running driver steals the other slot's buffers). Under the keep-alive
/// lifecycle a slot holds the driver from its first go-live until the plugin unloads, so only ONE slot
/// can be ASIO-live at a time — this guard surfaces a clean error instead of corrupting the other slot.
/// Unconditional (just an atomic). Acquisition follows the arm request's captured backend; release
/// follows actual `HeldStream` metadata, never a later read of the mutable preference.
static ASIO_DUPLEX_HOLDER: AtomicI8 = AtomicI8::new(-1);

/// Claim the ASIO duplex driver for `slot`. Returns true if `slot` now holds it (or already did),
/// false if another slot does. Called by the owner before the FIRST ASIO stream build (idempotent
/// across the input+output builds of one go-live). Released after both actual ASIO streams are gone.
pub fn try_acquire_asio_holder(slot: u8) -> bool {
    let s = slot as i8;
    match ASIO_DUPLEX_HOLDER.compare_exchange(-1, s, Relaxed, Relaxed) {
        Ok(_) => true,
        Err(cur) => cur == s, // already ours
    }
}

/// Release the ASIO duplex holder iff `slot` holds it (idempotent; safe when not held). Called at owner
/// The caller must first drop both retained ASIO streams for the slot.
pub fn release_asio_holder(slot: u8) {
    let _ = ASIO_DUPLEX_HOLDER.compare_exchange(slot as i8, -1, Relaxed, Relaxed);
}

/// Whether an ASIO low-latency device is AVAILABLE to select (the `asio` feature is compiled AND the
/// probe published a device). Drives the Audio Settings toggle's enabled state. Always false in a
/// build without the feature, so the toggle reads disabled there.
pub fn asio_available() -> bool {
    #[cfg(feature = "asio")]
    {
        asio_cache().is_some()
    }
    #[cfg(not(feature = "asio"))]
    {
        false
    }
}

/// "Should the next arm use ASIO?": preference ON, a cached device, and the feature compiled.
/// `AudioBackend::selected` samples this once per owner request; `NativeIo` then carries the actual
/// backend through stream open, ownership and the RT ring setpoints. The producer block CAP instead
/// keys on startup-fixed `asio_available()` so load-time allocation cannot skew against a later arm.
/// Audio Settings disables the toggle while a slot is armed; if a flip races an in-flight request,
/// that request keeps its captured backend and a mismatched second direction fails closed.
pub fn use_asio() -> bool {
    #[cfg(feature = "asio")]
    {
        ASIO_ENABLED.load(Relaxed) && asio_cache().is_some()
    }
    #[cfg(not(feature = "asio"))]
    {
        false
    }
}

/// Resolve the ASIO duplex device and its in/out configs while the driver is FREE. This is the ONLY
/// function that contacts an ASIO driver outside a stream build: `default_output_device()` loads and
/// initialises the driver DLL in-process (asio-sys → `CoCreateInstance` + `ASIOInit`), which is where
/// a broken driver hangs or crashes. Runs on the coordinator's probe thread, never from `run()`.
/// A `None` from cpal cannot distinguish "no driver installed" from "every driver failed to load"
/// (cpal skips drivers that fail), so the message says "no usable driver".
#[cfg(feature = "asio")]
fn resolve_asio_cache() -> Result<AsioCache, String> {
    log::info!("[audio_output] ASIO probe: contacting the driver (host, device, configs)");
    let host = cpal::host_from_id(cpal::HostId::Asio)
        .map_err(|e| format!("ASIO host unavailable ({e})"))?;
    let dev = host
        .default_output_device()
        .or_else(|| host.default_input_device())
        .or_else(|| host.devices().ok().and_then(|mut it| it.next()));
    let Some(d) = dev else {
        return Err("no usable ASIO driver found".to_string());
    };
    match (d.default_input_config(), d.default_output_config()) {
        (Ok(ic), Ok(oc)) => {
            if ic.sample_rate() == 0 || oc.sample_rate() == 0 {
                return Err("ASIO driver reported a zero sample rate".to_string());
            }
            let name = d
                .description()
                .map(|x| x.to_string())
                .unwrap_or_else(|_| "ASIO".to_string());
            let cache = AsioCache {
                name: name.clone(),
                in_cfg: StreamConfig {
                    channels: ic.channels(),
                    sample_rate: ic.sample_rate(),
                    buffer_size: BufferSize::Default,
                },
                in_fmt: ic.sample_format(),
                out_cfg: StreamConfig {
                    channels: oc.channels(),
                    sample_rate: oc.sample_rate(),
                    buffer_size: BufferSize::Default,
                },
                out_fmt: oc.sample_format(),
                device: d,
            };
            log::info!(
                "[audio_output] cached ASIO \"{name}\": in {:?} {:?} / out {:?} {:?}",
                cache.in_cfg,
                cache.in_fmt,
                cache.out_cfg,
                cache.out_fmt
            );
            Ok(cache)
        }
        (ic, oc) => Err(format!(
            "ASIO config query failed (input ok={}, output ok={})",
            ic.is_ok(),
            oc.is_ok()
        )),
    }
}

/// Resolve the WASAPI output (StreamConfig, SampleFormat) via a live query (the ASIO path uses the
/// cached config instead — see AsioCache).
fn output_config(device: &cpal::Device) -> Result<(StreamConfig, SampleFormat), String> {
    let c = device
        .default_output_config()
        .map_err(|e| format!("cpal default_output_config: {e}"))?;
    Ok((
        StreamConfig {
            channels: c.channels(),
            sample_rate: c.sample_rate(),
            buffer_size: BufferSize::Default,
        },
        c.sample_format(),
    ))
}

/// Pick the WASAPI output device (None = default). The ASIO low-latency tier does NOT go through here —
/// it uses the startup-cached duplex device (`asio_cache()`), because the single ASIO driver can't be
/// re-resolved once a stream holds it.
fn pick_output_device(device_id: Option<&str>) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    match device_id {
        Some(id) => {
            let parsed = id.parse().map_err(|_| format!("bad cpal device id: {id}"))?;
            host.device_by_id(&parsed)
                .ok_or_else(|| format!("cpal device_by_id({id}): not found"))
        }
        None => host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string()),
    }
}

/// Open a WASAPI-shared output stream on `device_id` (None = default output) that drains mono wet
/// frames from `consumer` (the RT producer's monitor ring), duplicating each frame across ALL output
/// channels and scaling by the linear gain in `gain_bits` (read atomically each callback so a UI
/// slider change needs no stream rebuild). On a ring underrun (producer lagging / disarmed) the
/// callback writes silence and bumps `starves` ONCE per callback (not per frame — `0` is the sync
/// proof, so it must be a meaningful count). Returns the stream paired with the device's **native
/// output rate** (`R_out`, Hz) — the RT producer needs it to build the D→R_out monitor resampler
/// (`OutMonitorPipe`). The `Stream` is `!Send`; the caller MUST build + keep it on the owner thread
/// (dropping it stops the monitor — RAII), exactly like the capture stream.
///
/// `consumer` is `Arc<Mutex<_>>` only to hand the single SPSC `Consumer` to successive cpal callbacks
/// across arm/disarm cycles; steady-state the audio thread never contends it (the owner only swaps it
/// between streams while disarmed). A contended/poisoned lock outputs silence for that callback.
///
/// `fault` is latched true by cpal's ERROR callback — which is TERMINAL by contract: the stream is
/// dead and never resumes (interface unplugged, ASIO driver reset). The owner loop polls it and tears
/// the corpse down; the recoverable underrun case stays the `starves` counter, never this flag.
pub fn open_output_stream(
    backend: AudioBackend,
    device_id: Option<&str>,
    consumer: Arc<Mutex<Consumer<f32>>>,
    gain_bits: Arc<AtomicU32>,
    starves: Arc<AtomicU64>,
    fade: Arc<AtomicU32>,
    faded: Arc<AtomicBool>,
    out_block: Arc<AtomicU32>,
    output_latency_ns: Arc<AtomicU64>,
    fault: Arc<AtomicBool>,
) -> Result<(cpal::Stream, u32), String> {
    // ASIO low-latency tier: build the monitor on the startup-cached duplex device — the single ASIO
    // driver can't be re-resolved once the input stream holds it. WASAPI: resolve fresh by id.
    if backend.is_asio() {
        #[cfg(feature = "asio")]
        {
            let c = asio_cache().ok_or_else(|| "ASIO backend selected without a cached device".to_string())?;
            log::info!("[audio_output] ASIO monitor on cached duplex device");
            // Retry once: the FIRST build after a prior plugin unload left the ASIO driver running with
            // its `asio_streams` still populated → cpal's reuse path calls `driver.start()` on a running
            // driver → BadMode. The failed build rolls back `asio_streams.X = None`, so the retry takes
            // the prepare path (which resets the driver) and succeeds. (No retry is needed on the very
            // first build of the app session — `asio_streams` is empty then.)
            return match build_output_on(
                &c.device,
                c.out_cfg,
                c.out_fmt,
                consumer.clone(),
                gain_bits.clone(),
                starves.clone(),
                fade.clone(),
                faded.clone(),
                out_block.clone(),
                output_latency_ns.clone(),
                fault.clone(),
            ) {
                Ok(r) => Ok(r),
                Err(first) => {
                    log::warn!("[audio_output] ASIO output build failed ({first}); retrying once");
                    build_output_on(
                        &c.device, c.out_cfg, c.out_fmt, consumer, gain_bits, starves, fade, faded,
                        out_block, output_latency_ns, fault,
                    )
                }
            };
        }
        #[cfg(not(feature = "asio"))]
        return Err("ASIO backend is unavailable in this build".to_string());
    }
    let device = pick_output_device(device_id)?;
    let (config, fmt) = output_config(&device)?;
    build_output_on(
        &device, config, fmt, consumer, gain_bits, starves, fade, faded, out_block, output_latency_ns, fault,
    )
}

/// Build + start an output stream on `device` with `config`/`fmt`. Pops mono wet from `consumer`,
/// duplicates it across all out channels, scales by the atomic linear gain, and zero-fills + counts a
/// starve (once per callback) on underrun. Returns (stream, R_out). Shared by the WASAPI + ASIO paths;
/// the `!Send` stream must stay on the caller's (owner) thread.
///
/// `out_block` is written each callback with the device's frames-per-callback (`data.len()/out_ch`,
/// R_out frames) — the cpal output device's own buffer period. The record-latency compensation reads it
/// (mirrored into `ProducerDiag::monitor_out_block`) for diagnostics and startup fallback. The device
/// term uses the median callback-to-playback timestamp DELTA instead. ASIO supplies its reported
/// hardware latency; WASAPI supplies queued padding plus stream latency. The ASIO epoch can wrap,
/// but cpal constructs playback from that same converted callback instant before adding the delay.
///
/// `fault` carries the terminal-error latch (see `open_output_stream`).
fn build_output_on(
    device: &cpal::Device,
    config: StreamConfig,
    fmt: SampleFormat,
    consumer: Arc<Mutex<Consumer<f32>>>,
    gain_bits: Arc<AtomicU32>,
    starves: Arc<AtomicU64>,
    fade: Arc<AtomicU32>,
    faded: Arc<AtomicBool>,
    out_block: Arc<AtomicU32>,
    output_latency_ns: Arc<AtomicU64>,
    fault: Arc<AtomicBool>,
) -> Result<(cpal::Stream, u32), String> {
    #[cfg(debug_assertions)]
    let config = crate::audio_latency_probe::config(config);
    // out_ch ≥ 1 so chunks_mut never panics; the shared-mode (or ASIO) mix rate is R_out.
    let out_ch = (config.channels as usize).max(1);
    let out_rate = config.sample_rate;
    // P11.3 declick: per-sample linear envelope step (units/sample) ramping the monitor to/from silence
    // over DECLICK_SECONDS. The callback advances `env` toward the atomic `fade` target each frame and
    // scales output by it; on disarm the owner drives the target to 0 and waits for `faded`, so the
    // stream drops on a zero sample (no click). max(1.0) guards a pathological 0 rate.
    let step = 1.0f32 / (DECLICK_SECONDS * out_rate as f32).max(1.0);

    // Per sample-format because the device may want I16/U16/I32 rather than F32; `<$T>::from_sample`
    // converts the gained f32 back to the device format.
    macro_rules! build_stream {
        ($T:ty) => {{
            let cons = consumer.clone();
            let gain_a = gain_bits.clone();
            let starve_a = starves.clone();
            let fade_a = fade.clone();
            let faded_a = faded.clone();
            let outblk_a = out_block.clone();
            let latency_a = output_latency_ns.clone();
            let fault_a = fault.clone();
            // Declick envelope state, per stream: starts silent and ramps UP to the arm target (1.0),
            // ramps DOWN to 0 on disarm. Captured mutably by the FnMut callback.
            let mut env = 0.0f32;
            let mut latency_window = OutputLatencyWindow::new();
            device.build_output_stream(
                config,
                move |data: &mut [$T], _info: &cpal::OutputCallbackInfo| {
                    #[cfg(debug_assertions)]
                    let marker_context = crate::marker_probe::output_begin(_info, out_rate);
                    #[cfg(debug_assertions)]
                    crate::audio_latency_probe::observe(data.len() / out_ch, _info);
                    if let Some(ns) = latency_window.observe(_info.timestamp()) {
                        latency_a.store(ns, Relaxed);
                    }
                    let g = f32::from_bits(gain_a.load(Relaxed));
                    let m = f32::from_bits(MASTER_GAIN.load(Relaxed));
                    let target = f32::from_bits(fade_a.load(Relaxed));
                    // Publish the device's frames-per-callback (R_out) for the record-latency
                    // compensation's `cpal_out` term. `out_ch ≥ 1`, so the divide can't panic.
                    outblk_a.store((data.len() / out_ch) as u32, Relaxed);
                    let mut underran = false;
                    if let Ok(mut c) = cons.try_lock() {
                        for (_offset, frame) in data.chunks_mut(out_ch).enumerate() {
                            let popped = c.pop().ok();
                            #[cfg(debug_assertions)]
                            crate::marker_probe::output_sample(marker_context, popped, _offset, out_rate);
                            // Advance the declick envelope one sample toward the target (0 = fade to
                            // silence on disarm, 1 = fade up from silence on arm), clamped. Fading UP
                            // advances ONLY on a real popped sample: an arm-startup underrun would
                            // otherwise burn the ramp on silence and land the first wet sample at full
                            // gain — the exact click the fade exists to prevent. Fading DOWN advances
                            // unconditionally; there the producer is deliberately kept feeding until
                            // the envelope reaches silence.
                            let d = target - env;
                            if d < 0.0 || popped.is_some() {
                                if d.abs() <= step {
                                    env = target;
                                } else if d > 0.0 {
                                    env += step;
                                } else {
                                    env -= step;
                                }
                            }
                            match popped {
                                Some(v) => {
                                    let s = <$T>::from_sample(v * g * m * env);
                                    #[cfg(debug_assertions)]
                                    let s = if marker_context.is_some() { <$T>::from_sample(0.0f32) } else { s };
                                    for ch in frame.iter_mut() {
                                        *ch = s;
                                    }
                                }
                                None => {
                                    let sil = <$T>::from_sample(0.0f32);
                                    for ch in frame.iter_mut() {
                                        *ch = sil;
                                    }
                                    underran = true;
                                }
                            }
                        }
                    } else {
                        #[cfg(debug_assertions)]
                        crate::marker_probe::output_sample(marker_context, None, 0, out_rate);
                        let sil = <$T>::from_sample(0.0f32);
                        for s in data.iter_mut() {
                            *s = sil;
                        }
                        underran = true;
                    }
                    // Count an underrun as a starve ONLY while armed (fade target above the floor). When
                    // disarmed/fading-out (target ≈ 0) the producer has stopped feeding wet BY DESIGN, so
                    // silence is expected — not a real starve. This matters under the ASIO keep-alive
                    // lifecycle (direction A): on disarm the output stream stays alive playing silence (so
                    // the shared ASIO driver's buffers don't loop a stale wet sample = the drone), which
                    // would otherwise spew phantom starves every callback while disarmed.
                    if underran && target > 1e-4 {
                        starve_a.fetch_add(1, Relaxed);
                    }
                    // Tell the owner the fade-out reached silence so it can drop the stream on a zero
                    // sample. Only when fading OUT (target ~0); arm (target 1) never trips this.
                    if target <= 1e-6 && env <= 1e-4 {
                        faded_a.store(true, Relaxed);
                    }
                },
                // Terminal by cpal contract: this stream is finished (device removed, driver reset)
                // and its data callback stops firing — so latch the fault for the owner loop, which
                // drops the dead stream and tells JS to fall back. Log first, latch second: the flag
                // is what the owner acts on, so it must not be observable before the line that
                // explains it. Both are cheap and this callback fires once, off the audio path.
                move |e: cpal::Error| {
                    log::warn!("[audio_output] cpal stream error: {e}");
                    fault_a.store(true, Relaxed);
                },
                None, // timeout: Option<Duration>
            )
        }};
    }

    let stream = match fmt {
        SampleFormat::F32 => build_stream!(f32),
        // I32 is the common ASIO sample format (Focusrite USB ASIO reports it); `<i32>::from_sample`
        // handles the gained f32→int scaling. WASAPI-shared is usually F32, so this only fires under ASIO.
        SampleFormat::I32 => build_stream!(i32),
        SampleFormat::I16 => build_stream!(i16),
        SampleFormat::U16 => build_stream!(u16),
        other => return Err(format!("unsupported output sample format: {other:?}")),
    }
    .map_err(|e| format!("cpal build_output_stream: {e}"))?;

    // MANDATORY in 0.18 — streams return paused; without play() the monitor is silent.
    stream
        .play()
        .map_err(|e| format!("cpal stream.play: {e}"))?;
    Ok((stream, out_rate))
}
