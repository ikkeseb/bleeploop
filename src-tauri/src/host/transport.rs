//! Format-agnostic RT transport plumbing shared by the CLAP (`clap.rs`) and VST3
//! (`clap::vst3_host`) producers: the hop-1 WebView2 SharedBuffer ring + resampler (`Hop1Pipe`),
//! the cpal-input resampler (`InPipe`), the native-monitor output resampler (`OutMonitorPipe`),
//! the shared drift-correction PI controller (`DriftController`), the per-slot RT diagnostics
//! (`ProducerDiag`), and the high-res pacing timer (`PaceTimer`). This file is the future
//! `crates/lf-rt` seam; no workspace/crate split is made here.

use std::sync::atomic::{
    AtomicBool, AtomicU32, AtomicU64, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer};

use rubato::audioadapter_buffers::direct::SequentialSlice;
use rubato::{Async, FixedAsync, PolynomialDegree, Resampler};

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2Environment12, ICoreWebView2SharedBuffer, ICoreWebView2_17, ICoreWebView2_2,
    COREWEBVIEW2_SHARED_BUFFER_ACCESS_READ_WRITE,
};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
};

use super::state::PluginInfo;

/// Hop-1 (WebView2 SharedBuffer) layout — a single-producer (RT thread) / single-consumer (JS
/// main-thread drain) mono-f32 ring, plus a JS→Rust feedback channel (P9.4 drift control). The
/// JS side does PLAIN ordered reads/writes (Atomics are unsupported on this non-shared ArrayBuffer
/// in Chromium 149); the Rust side uses release/acquire atomics over the same mapped memory, and
/// x86-64 TSO makes the stores visible across in order. Layout MUST match
/// `src/audio/plugin-bridge.ts` (28 bytes, 7×u32, single-writer-per-field):
///   [0] write_frames    Rust→JS  hop-1 produced total (post-resample, C-rate)
///   [1] read_frames     JS→Rust  drain copy cursor (lag-capped; NOT a control signal)
///   [2] capacity_frames Rust→JS  ring cap (written once)
///   [3] hop2_fill       JS→Rust  the PI controller's level signal (PV), discard-neutral (B2)
///   [4] consumed        JS→Rust  worklet STAT_CONSUMED total (gate liveness + drift slope)
///   [5] underruns       JS→Rust  worklet STAT_UNDERRUNS total (M1)
///   [6] js_dropped      JS→Rust  cumulative lag-cap + flush discards (M2)
///   data: `HOP1_CAPACITY_FRAMES` × f32, immediately after the header (offset 28 is 4-byte
///         aligned → f32-aligned). Rust acquire-loads [3]..[6]; JS plain-writes them.
pub(super) const HOP1_HEADER_BYTES: usize = 28;
/// Power of two so the ring index is `frame & (CAP-1)` and wraps seamlessly across the u32 frame
/// counter's own wrap. 16384 ≈ 340 ms @ 48 kHz — generous headroom; the JS drain caps real lag.
pub(super) const HOP1_CAPACITY_FRAMES: u32 = 16384;

/// Hard ceiling for channel rows allocated from foreign plugin metadata. A malformed plugin can
/// report an arbitrary count; validating before the RT buffers are built turns a process-wide OOM
/// into the normal load error path. Zero is valid only for an absent input bus.
const MAX_PLUGIN_CHANNELS: i64 = 64;
pub(super) fn checked_plugin_channels(
    count: i64,
    bus: &str,
    allow_zero: bool,
) -> Result<u32, String> {
    let minimum = if allow_zero { 0 } else { 1 };
    if (minimum..=MAX_PLUGIN_CHANNELS).contains(&count) {
        Ok(count as u32)
    } else {
        Err(format!(
            "{bus} reports unsupported channel count {count} (supported {minimum}..={MAX_PLUGIN_CHANNELS})"
        ))
    }
}

// P9.4 forced rate-mismatch (DEV by-ear vs the drift gate — now RUNTIME-gated, was cfg(debug_assertions)).
// The drift gate is meaningless at matched nominal rates with the resampler near-bypassed (B3): we
// activate the plugin at D, resample D→C, and pace the producer at a clock biased by `force_epsilon_ppm()`
// relative to D — a residual the static C/D ratio CANNOT cancel, so the PI controller must discover it;
// the gate then asserts `drift_ppm ≈ epsilon` and `ratio_spread > 0` (the loop demonstrably moved).
//
// WHY runtime, not cfg: a debug `tauri dev` is the by-ear build, but forcing D=48k there
// (a) makes rubato do a REAL 44.1↔48k sinc conversion both ways and (b) biases the pacing clock by
// 200ppm — both stress the RT loop in a way PROD (native 44.1k, D==C, ratio≈1.0 ⇒ near-passthrough)
// never does, so a dev-build by-ear was untrustworthy (the ASIO "crackling" hunt, 2026-06-17). Default
// is now OFF ⇒ native render rate ⇒ prod-realistic by-ear. Set `LF_FORCE_48K=1` (or `=true`) to re-arm
// the mismatch for the P9.4 drift gate. Release honors the switch too, but `emit_gate` is debug-only
// anyway. Read ONCE via OnceLock so the load + producer-rebuild paths never re-hit the env.

/// `true` iff `LF_FORCE_48K` is set to `1`/`true` (read + logged once at first use). The DEV
/// drift-gate switch; OFF by default so a `tauri dev` by-ear matches a native-rate prod build.
/// `pub(super)` so the top-level startup wrapper can trigger the one-shot log before any load.
pub(super) fn lf_force_48k() -> bool {
    static FORCE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FORCE.get_or_init(|| {
        let on = std::env::var("LF_FORCE_48K")
            .map(|v| {
                let v = v.trim();
                v == "1" || v.eq_ignore_ascii_case("true")
            })
            .unwrap_or(false);
        log::info!(
            "[plugin_host] render rate: {}",
            if on {
                "FORCED 48k — P9.4 drift gate (LF_FORCE_48K=1; NOT prod-realistic)"
            } else {
                "native device rate — prod-realistic (set LF_FORCE_48K=1 for the P9.4 drift gate)"
            },
        );
        on
    })
}

/// DEV render-rate override. `None` ⇒ render D = ctx C (native, prod-realistic). `Some(48k)` ⇒ the
/// P9.4 rate-mismatch, ONLY when `LF_FORCE_48K` is set. See [`lf_force_48k`].
pub(super) fn force_device_rate() -> Option<f64> {
    if lf_force_48k() {
        Some(48_000.0)
    } else {
        None
    }
}

/// Producer-pacing bias (ppm) the static C/D ratio can't cancel — the residual the P9.4 PI controller
/// must discover. Nonzero ONLY under `LF_FORCE_48K` (paired with [`force_device_rate`]); 0 otherwise.
fn force_epsilon_ppm() -> f64 {
    if lf_force_48k() {
        200.0
    } else {
        0.0
    }
}

/// High-resolution one-shot waitable timer for producer pacing (P9.4-fix, 2026-06-15).
///
/// `std::thread::sleep` on Windows rounds a wait up to the system timer tick (≥1 ms; 15.6 ms by
/// default), and the original producer paced with a RELATIVE rest (`period_dur − elapsed`) — so
/// every tick of sleep-overshoot was lost permanently, never caught up. Net result on the PC: the
/// producer ran ~2.4 % slow (46 858 vs the forced 48 009.6 D-frames/s), *beyond* the drift
/// controller's ±1 % authority, so the loop diverged and hop-2 starved ~1.7 % of quanta — the
/// audible crackle and the P9.4 gate FAIL (see the MEASURED OUTCOME doc).
///
/// The real fix is the absolute-deadline accumulation at the call site (`next_deadline +=
/// period_dur`), which makes the MEAN rate exact regardless of per-wait overshoot. This timer just
/// trims the residual jitter the hop-2 buffer must absorb: `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`
/// (Win10 1803+, well within WebView2 v149's floor) gives sub-ms waits. Falls back to `sleep` if
/// the high-res timer can't be created. RT-safe: created once pre-loop; `wait()` is two syscalls,
/// no allocation (as `sleep` already was).
struct PaceTimer(Option<HANDLE>);
impl PaceTimer {
    fn new() -> Self {
        // SAFETY: FFI into kernel32; null attrs/name, high-res flag, full access. `.ok()` maps a
        // failure (e.g. flag unsupported) to None → sleep fallback.
        let h = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0, // SYNCHRONIZATION_ACCESS_RIGHTS newtype → raw u32
            )
        }
        .ok();
        Self(h)
    }
    /// Block for `dur`. No allocation; either a high-res timer set+wait, or `sleep`.
    fn wait(&self, dur: Duration) {
        match self.0 {
            Some(h) => unsafe {
                // Negative due-time = relative, in 100-ns units. Clamp into i64 range.
                let due: i64 = -((dur.as_nanos() / 100).min(i64::MAX as u128) as i64);
                // SAFETY: `h` is a live timer handle owned by self; `due` lives for the call.
                if SetWaitableTimer(h, &due, 0, None, None, false).is_ok() {
                    let _ = WaitForSingleObject(h, INFINITE);
                } else {
                    std::thread::sleep(dur);
                }
            },
            None => std::thread::sleep(dur),
        }
    }
}
impl Drop for PaceTimer {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            // SAFETY: `h` is the timer handle from CreateWaitableTimerExW, dropped once.
            unsafe {
                let _ = CloseHandle(h);
            }
        }
    }
}

/// Process-wide load generation. Bumped when a `plugin_load` reserves its slot; stamped into the slot's
/// `ProducerDiag` so each `[diag]` gate line carries its `load_gen`. The gate consumer uses it to
/// detect a `reloadPlugin` mid-stream (counters reset on a fresh worklet SAB) and restart the
/// 120s clock / skip a poisoned window delta (M4).
pub(super) static LOAD_GEN: AtomicU32 = AtomicU32::new(0);

/// RT failures are latched as bits and reported by the owner thread. Logging directly from the
/// audio thread can block on stdout or the file sink, so the steady-state path only performs one
/// relaxed atomic OR. Repeated failures of the same kind coalesce for the lifetime of a plugin load.
#[repr(u32)]
#[derive(Clone, Copy)]
pub(super) enum RtFault {
    Hop1Resample = 1 << 0,
    InputResample = 1 << 1,
    MonitorResample = 1 << 2,
    Hop1Rebuild = 1 << 3,
    InputRebuild = 1 << 4,
    MonitorRebuild = 1 << 5,
    ClapProcess = 1 << 6,
    Vst3Process = 1 << 7,
}

const RT_FAULT_MESSAGES: [(RtFault, &str); 8] = [
    (RtFault::Hop1Resample, "hop-1 resampler failed; this block became silence"),
    (RtFault::InputResample, "input resampler failed; this block became silence"),
    (RtFault::MonitorResample, "monitor resampler failed; this block became silence"),
    (RtFault::Hop1Rebuild, "hop-1 resampler rebuild failed; the prior block size remains active"),
    (RtFault::InputRebuild, "input resampler rebuild failed; native input was disabled"),
    (RtFault::MonitorRebuild, "monitor resampler rebuild failed; native monitoring was disabled"),
    (RtFault::ClapProcess, "CLAP processing setup or block call failed; plugin output became silence"),
    (RtFault::Vst3Process, "VST3 processing setup or block call failed; plugin output may be incomplete"),
];

/// Owner-thread drain for the RT latch. Each category reaches the release log once per plugin load.
pub(super) fn report_new_rt_faults(
    diag: &ProducerDiag,
    slot: u8,
    reported: &mut u32,
) {
    let new = diag.rt_faults.load(Relaxed) & !*reported;
    if new == 0 {
        return;
    }
    *reported |= new;
    for (fault, message) in RT_FAULT_MESSAGES {
        if new & fault as u32 != 0 {
            log::error!("[plugin_host] slot {slot} RT fault: {message}");
        }
    }
}

/// RT-thread counters, read by the owner thread's ~2s emitter. Counters are relaxed atomics, while
/// input/monitor generation bumps publish their rate + backend metadata with release/acquire. The
/// producer never locks or allocates to update them. The `[3]..[6]` mirrors + the ratio/drift state
/// are written each block by the producer from its header acquire-loads + controller state; the
/// gate emitter (owner thread) is the only reader.
pub struct ProducerDiag {
    pub(super) frames_written: AtomicU64, // C-rate frames produced (post-resample) — liveness, rises forever
    pub(super) frames_dropped: AtomicU64, // of those, frames the (un-drained) hop-1 ring couldn't hold
    pub(super) ring_used: AtomicUsize,    // hop-1 unread frames (write - read), snapshot each block
    pub(super) ring_capacity: AtomicUsize,
    pub(super) alive: AtomicBool,
    pub(super) sample_rate: AtomicU64, // f64 bits — C (ctx rate)
    pub(super) max_frames: AtomicU32,
    pub(super) out_channels: AtomicU32,
    // --- P9.4 feedback mirrors (from hop-1 header [3]..[6], one acquire-load per block) ---
    pub(super) hop2_fill: AtomicU32, // PV — hop-2 fill in C-frames (discard-neutral, B2)
    pub(super) consumed: AtomicU32,  // worklet STAT_CONSUMED total
    pub(super) underruns: AtomicU32, // worklet STAT_UNDERRUNS total (M1)
    pub(super) js_dropped: AtomicU32, // JS lag-cap + flush discards total (M2)
    // --- P9.4 controller state (mirrored each block) ---
    pub(super) resample_ratio_bits: AtomicU64, // f64 — actual ratio = nominal(C/D) * (1 + rel_corr)
    pub(super) ratio_min_bits: AtomicU64,       // f64 — running min ratio since last emit (ratio_spread)
    pub(super) ratio_max_bits: AtomicU64,       // f64 — running max ratio since last emit
    pub(super) drift_ppm_bits: AtomicU64,       // f64 — settled integral in ppm (discovered clock drift)
    pub(super) device_rate_bits: AtomicU64,     // f64 — D (set once at init)
    pub(super) target_frames: AtomicU32,        // controller setpoint in C-frames (set once at init)
    pub(super) ratio_clamp_fails: AtomicU64,    // set_resample_ratio_relative Err count (B1 — never silent)
    pub(super) load_gen: AtomicU32,             // the LOAD_GEN this slot was born with (M4)
    // --- P9.5 control-plane diag ---
    pub(super) out_peak_bits: AtomicU32,  // f32 bits — max |mono| since last emit; >0 proves a routed note voiced
    pub(super) events_dropped: AtomicU64, // ring-full + drain-cap overflow (cumulative; the gate windows it)
    // --- P11.0 audio-input diag (only meaningful when the slot has an input bus + is armed) ---
    pub(super) input_fill: AtomicU32,     // cpal→RT input-ring unread mono frames, snapshot each RT block
    pub(super) input_starves: AtomicU64,  // RT blocks the input ring couldn't fully feed (cumulative; windowed)
    pub(super) input_overruns: AtomicU64, // capture frames the FULL input ring dropped (owner mirrors from the
    // cpal capture stream's counter). The opposite failure to a starve: the RT consumer is BEHIND, which
    // produces no starve at all, so this counter is the only thing that can see the loss.
    // --- P11 input-SRC: cpal capture rate + input drift loop state (owner writes rate; RT reads) ---
    pub(super) input_rate: AtomicU32, // R_in (cpal native capture rate, Hz); 0 = disarmed
    pub(super) input_is_asio: AtomicBool, // actual backend paired with input_rate by input_gen
    pub(super) input_gen: AtomicU32, // owner Release-bump on EVERY arm/disarm → RT Acquire-rebuilds InPipe
    pub(super) input_drift_ppm_bits: AtomicU64, // f64 bits — input DriftController settled integral (ppm)
    // --- P11.3 native monitor (branch-1: wet → cpal output on the same device) ---
    pub(super) monitor_rate: AtomicU32, // R_out (cpal output native rate, Hz); 0 = monitor disarmed
    pub(super) monitor_is_asio: AtomicBool, // actual backend paired with monitor_rate by monitor_gen
    pub(super) monitor_gen: AtomicU32, // owner Release-bump on EVERY arm/disarm → RT Acquire-rebuilds OutMonitorPipe
    pub(super) monitor_fill: AtomicU32, // mon-ring fill (R_out frames), snapshot each RT block
    pub(super) monitor_starves: AtomicU64, // cpal-out callbacks that underran (owner mirrors from the stream's counter)
    pub(super) monitor_overruns: AtomicU64, // wet frames the FULL mon ring dropped on publish (RT-written).
    // Same asymmetry as `input_overruns`: a full ring means the cpal-out consumer is BEHIND, which never
    // shows up as a starve.
    pub(super) monitor_drift_ppm_bits: AtomicU64, // f64 bits — monitor DriftController settled integral (ppm)
    pub(super) monitor_out_block: AtomicU32, // cpal output device frames-per-callback (R_out); owner mirrors from
    // the stream's atomic. The output-device half of `cpal_out` for the record-latency compensation
    // (fallback only until a valid output timestamp window). RETAINS its last value while
    // disarmed (never reset on disarm) — only meaningful when monitor_rate != 0; the sole reader
    // (monitor_latency_seconds) short-circuits on rate==0 before reading it, so a stale value is inert.
    pub(super) monitor_output_latency_ns: AtomicU64, // owner-mirrored callback→playback median; 0 until valid
    // --- P11.3 live buffer size ---
    pub(super) block_frames: AtomicU32, // the producer's ACTIVE RT block (D-frames); the gate reads it to confirm a buffer change
    pub(super) rt_faults: AtomicU32, // [`RtFault`] bitmask; RT sets, owner reports once per load
}

impl ProducerDiag {
    pub(super) fn new() -> Self {
        Self {
            frames_written: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
            ring_used: AtomicUsize::new(0),
            ring_capacity: AtomicUsize::new(0),
            alive: AtomicBool::new(false),
            sample_rate: AtomicU64::new(0),
            max_frames: AtomicU32::new(0),
            out_channels: AtomicU32::new(0),
            hop2_fill: AtomicU32::new(0),
            consumed: AtomicU32::new(0),
            underruns: AtomicU32::new(0),
            js_dropped: AtomicU32::new(0),
            // min seeded +inf, max seeded -inf so the first block's ratio sets both correctly.
            resample_ratio_bits: AtomicU64::new(1.0f64.to_bits()),
            ratio_min_bits: AtomicU64::new(f64::INFINITY.to_bits()),
            ratio_max_bits: AtomicU64::new(f64::NEG_INFINITY.to_bits()),
            drift_ppm_bits: AtomicU64::new(0.0f64.to_bits()),
            device_rate_bits: AtomicU64::new(0),
            target_frames: AtomicU32::new(0),
            ratio_clamp_fails: AtomicU64::new(0),
            load_gen: AtomicU32::new(0),
            out_peak_bits: AtomicU32::new(0),
            events_dropped: AtomicU64::new(0),
            input_fill: AtomicU32::new(0),
            input_starves: AtomicU64::new(0),
            input_overruns: AtomicU64::new(0),
            input_rate: AtomicU32::new(0),
            input_is_asio: AtomicBool::new(false),
            input_gen: AtomicU32::new(0),
            input_drift_ppm_bits: AtomicU64::new(0.0f64.to_bits()),
            monitor_rate: AtomicU32::new(0),
            monitor_is_asio: AtomicBool::new(false),
            monitor_gen: AtomicU32::new(0),
            monitor_fill: AtomicU32::new(0),
            monitor_starves: AtomicU64::new(0),
            monitor_overruns: AtomicU64::new(0),
            monitor_drift_ppm_bits: AtomicU64::new(0.0f64.to_bits()),
            monitor_out_block: AtomicU32::new(0),
            monitor_output_latency_ns: AtomicU64::new(0),
            block_frames: AtomicU32::new(0),
            rt_faults: AtomicU32::new(0),
        }
    }

    #[inline]
    pub(super) fn latch_rt_fault(&self, fault: RtFault) {
        self.rt_faults.fetch_or(fault as u32, Relaxed);
    }

    /// `sample_rate` = C (ctx rate); `device_rate` = D (the forced-mismatch rate). `target_frames`
    /// is the controller's hop-2 setpoint in C-frames. `load_gen` is captured at reservation, so
    /// concurrent slot loads cannot stamp each other's generation (M4).
    pub(super) fn init(
        &self,
        sample_rate: f64,
        device_rate: f64,
        max_frames: u32,
        out_channels: u32,
        capacity: usize,
        target_frames: u32,
        load_gen: u32,
    ) {
        self.sample_rate.store(sample_rate.to_bits(), Relaxed);
        self.device_rate_bits.store(device_rate.to_bits(), Relaxed);
        self.max_frames.store(max_frames, Relaxed);
        self.out_channels.store(out_channels, Relaxed);
        self.ring_capacity.store(capacity, Relaxed);
        self.target_frames.store(target_frames, Relaxed);
        self.load_gen.store(load_gen, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rt_fault_latch_coalesces_repeats_without_losing_categories() {
        let diag = ProducerDiag::new();
        diag.latch_rt_fault(RtFault::Hop1Resample);
        diag.latch_rt_fault(RtFault::Hop1Resample);
        diag.latch_rt_fault(RtFault::ClapProcess);

        let expected = RtFault::Hop1Resample as u32 | RtFault::ClapProcess as u32;
        assert_eq!(diag.rt_faults.load(Relaxed), expected);

        let mut reported = 0;
        report_new_rt_faults(&diag, 0, &mut reported);
        report_new_rt_faults(&diag, 0, &mut reported);
        assert_eq!(reported, expected);
    }
}

// ---- P9.4 drift controller (slow PI on hop-2 fill, discard-neutral PV — B2) ----------------

/// Time-targeted (sample-rate-independent) so the band is rate-agnostic. Derivation in spec §3c.
pub(super) const TARGET_FILL_SECONDS: f64 = 0.030; // ~30ms hop-2 setpoint (HOP2 cap ≈ 340ms @48k → headroom)
// Tuning verified on PC (2026-06-15): with the producer now precisely paced (deadline + high-res
// timer, P9.4-fix), the controller's real job is the residual crystal drift between QPC and the
// sound-card clock (~400 ppm measured here). This ζ=0.625 tuning settles on the forced +200 ppm in
// ~100 s with 0 underruns and a trailing-EWMA of ~+197 (gate PASS). It is mildly underdamped (a
// ±60 ppm, ~160 s ring during the transient) — inaudible (±0.03 % pitch). KP=0.10 (ζ=1.25) was
// measured too: overdamping only made the slow loop (wn=0.04) settle slower (~250 s, EWMA +185),
// no gain — the drift gate scores the tail EWMA, not per-line, so the ring never mattered.
const KP: f64 = 0.05; // ratio-correction per second-of-fill-error
const KI: f64 = 0.0016; // ratio-correction per second² → wn=√Ki=0.04 rad/s, ζ=Kp/(2√Ki)=0.625
const MAX_REL_CORR: f64 = 0.01; // ±1% authority (≫ 200ppm); < ctor max_rel (1.02) → B1 safe
const I_CLAMP: f64 = MAX_REL_CORR;
/// rubato construction band: strictly greater than the controller's authority, so a clamped
/// `set_resample_ratio_relative` call stays inside `[1/max_rel, max_rel]` and never hard-errors
/// `RatioOutOfBounds` (B1). The per-call clamp uses (1 + MAX_REL_CORR); this is the ctor headroom.
const MAX_RESAMPLE_RATIO_RELATIVE: f64 = 1.02;

/// Slow PI controller on hop-2 fill. PV = `hop2_fill` (C-frames, discard-neutral). The plant is a
/// pure integrator (`fill(n) = fill(n-1) + produced − consumed`); PI makes the closed loop type-1
/// so the integrator converges to the unknown residual clock-rate correction (≈ −eps) with zero
/// steady-state fill error. Returns the RELATIVE ratio for rubato (multiplies nominal C/D).
struct DriftController {
    sr_ctx: f64,
    target_frames: f64,
    block_dt: f64,
    integ: f64,
    rel_corr: f64,
}
impl DriftController {
    fn new(sr_ctx: f64, device_rate: f64, block_frames_d: u32) -> Self {
        Self::with_target(sr_ctx, device_rate, block_frames_d, TARGET_FILL_SECONDS)
    }
    /// As `new` but with an explicit fill setpoint (seconds). P11.3's monitor wants a SMALL target
    /// (low latency) vs the ~30ms hop-2 setpoint — same PI, just a different reference.
    fn with_target(
        sr_ctx: f64,
        device_rate: f64,
        block_frames_d: u32,
        target_seconds: f64,
    ) -> Self {
        Self {
            sr_ctx,
            target_frames: target_seconds * sr_ctx,
            block_dt: block_frames_d as f64 / device_rate,
            integ: 0.0,
            rel_corr: 0.0,
        }
    }
    /// `hop2_fill_frames` is C-rate (the worklet pops at C). Returns the relative ratio (≈ 1.0).
    fn step(&mut self, hop2_fill_frames: f64) -> f64 {
        let err_sec = (hop2_fill_frames - self.target_frames) / self.sr_ctx; // +ve = over-full
        let p = KP * err_sec;
        let next_integ = self.integ + KI * err_sec * self.block_dt;
        let unsat = -(p + next_integ);
        // conditional integration (anti-windup): integrate only if not pushing into saturation.
        if unsat.abs() < MAX_REL_CORR || unsat.signum() != (-(p + self.integ)).signum() {
            self.integ = next_integ.clamp(-I_CLAMP, I_CLAMP);
        }
        self.rel_corr = (-(p + self.integ)).clamp(-MAX_REL_CORR, MAX_REL_CORR);
        1.0 + self.rel_corr // relative ratio (multiplies nominal C/D inside rubato)
    }
    fn drift_ppm(&self) -> f64 {
        self.integ * 1.0e6 // settled integral = discovered clock drift
    }
    /// P11.3 live buffer: re-point the ONLY block-dependent term. `block_dt` (= D-block /
    /// device_rate) scales the integral step; `integ` (the learned ~400ppm crystal-drift
    /// correction), `rel_corr`, `target_frames` and `sr_ctx` are block-INDEPENDENT and PRESERVED —
    /// so a buffer-size change does NOT re-trigger the slow (~100s) PI re-convergence a full `::new`
    /// would (the KI≈0.0016 integrator would re-discover the drift from zero).
    fn set_block(&mut self, block_frames_d: u32, device_rate: f64) {
        self.block_dt = block_frames_d as f64 / device_rate;
    }
    /// Seed the integrator with a drift a previous producer of the SAME slot already learned (ppm,
    /// as `drift_ppm` reports it). A plugin-requested restart respawns the producer but changes
    /// neither clock, so the ~400 ppm QPC↔sound-card correction is still right; from zero it would
    /// re-converge over ~100 s (measured 2026-09-10, Surge XT VST3 restarted at t = 30 s: the
    /// settled −392 ppm dropped to 0 and was back to only −3 ppm 14 s later; seeded, it stayed at
    /// −389). Clamped to the integrator's own authority so a garbage seed cannot wind it past
    /// `I_CLAMP`.
    fn resume_ppm(&mut self, ppm: f64) {
        self.integ = (ppm * 1.0e-6).clamp(-I_CLAMP, I_CLAMP);
        self.rel_corr = (-self.integ).clamp(-MAX_REL_CORR, MAX_REL_CORR);
    }
}

/// The `Ok` payload of the owner thread's setup: the descriptor info + the SharedBuffer handle
/// (to store in `SlotHandle` for `Close()` at unload).
pub(super) type LoadReady = Result<(PluginInfo, SharedBufferHandle), String>;

/// The hop-1 `ICoreWebView2SharedBuffer` COM object, parked as a raw owning pointer so it can sit
/// in the `Send + Sync` `SlotHandle` between two UI-thread hops. The object is created inside
/// `with_webview` (the WebView2 UI thread, an STA) and is only ever touched again inside another
/// `with_webview` closure (`close`), i.e. on the same thread — so it never needs marshaling. That
/// is the whole reason for this type: `AgileReference` (cross-apartment marshaling) has no
/// proxy/stub for this interface and failed on every load, which silently leaked the buffer at
/// every unload (the "AgileReference failed …; leaking shared buffer" warning, 2026-09-10).
pub(super) struct SharedBufferHandle(usize);
// SAFETY: the pointer is an opaque token between the two UI-thread hops; the object is never
// used from any other thread.
unsafe impl Send for SharedBufferHandle {}
unsafe impl Sync for SharedBufferHandle {}
impl SharedBufferHandle {
    /// Close a handle while already executing on the WebView UI thread. Used when the owner waiting
    /// for `create_shared_ring` has timed out and the callback can no longer hand the handle back.
    fn close_on_ui_thread(self, slot: u8) {
        let raw = self.0;
        // SAFETY: callers are inside `with_webview`; `raw` is the owning reference kept by into_raw.
        unsafe {
            let buf = ICoreWebView2SharedBuffer::from_raw(raw as *mut std::ffi::c_void);
            match buf.Close() {
                Ok(()) => log::info!("[plugin_host] slot {slot} shared buffer closed"),
                Err(e) => log::warn!("[plugin_host] slot {slot} shared buffer Close() failed: {e}"),
            }
        }
    }

    /// `Close()` + release the buffer on the UI thread. Fire-and-forget (`with_webview` hops); if
    /// the window is already gone (app exit) the reference leaks with the process. Caller
    /// guarantees nothing still writes the mapping (the RT producer is joined) and JS has released
    /// its `ArrayBuffer` (`releasePluginBuffer`) — `Close()` unmaps for both sides.
    pub(super) fn close(self, window: &tauri::WebviewWindow, slot: u8) {
        let _ = window.with_webview(move |_| {
            self.close_on_ui_thread(slot);
        });
    }
}

/// Allocate + post the hop-1 WebView2 SharedBuffer (on the UI thread via `with_webview`) and hand
/// the mapped data pointer (as a `usize` — process-wide stable) back to the caller. `with_webview`
/// is fire-and-forget from a non-UI thread, so the result is recovered through a channel. The COM
/// object comes back as a `SharedBufferHandle` for the UI-thread `Close()` at unload.
pub(super) fn create_shared_ring(
    window: &tauri::WebviewWindow,
    cap_frames: u32,
    slot: u8,
    frontend_epoch: u32,
    load_token: u32,
    load_running: &Arc<AtomicBool>,
    sample_rate: f64,
    in_channels: u32,
) -> Result<(usize, SharedBufferHandle), String> {
    let bytes = HOP1_HEADER_BYTES as u64 + cap_frames as u64 * 4;
    // inChannels (P11 output-gain): the plugin's audio-input port-0 channel count. JS reads it to
    // pick the per-slot output-gain default — >0 ⇒ FX/amp-sim (input-driven, near-unity), 0 ⇒
    // synth (MIDI-driven, conservative). Same `in_channels` the arm-input gate already uses.
    let json = format!(
        r#"{{"kind":"plugin-audio","slot":{slot},"frontendEpoch":{frontend_epoch},"loadToken":{load_token},"capacityFrames":{cap_frames},"headerBytes":{},"sampleRate":{},"inChannels":{in_channels}}}"#,
        HOP1_HEADER_BYTES, sample_rate
    );
    let json_w: Vec<u16> = json.encode_utf16().chain(std::iter::once(0)).collect();

    let (tx, rx) = std::sync::mpsc::sync_channel::<LoadRing>(1);
    let callback_live = Arc::new(AtomicBool::new(true));
    let callback_live_ui = callback_live.clone();
    let load_running_ui = load_running.clone();

    window
        .with_webview(move |webview| {
            if !callback_live_ui.load(Acquire) || !load_running_ui.load(Acquire) {
                let _ = tx.send(Err("shared ring provisioning cancelled".to_string()));
                return;
            }
            let result: LoadRing = (|| {
                // SAFETY: every WebView2 COM call must run on the UI thread; with_webview
                // dispatches this closure there (same contract as register_permission_autogrant).
                unsafe {
                    let controller = webview.controller();
                    let core = controller
                        .CoreWebView2()
                        .map_err(|e| format!("CoreWebView2: {e}"))?;
                    let core17: ICoreWebView2_17 =
                        core.cast().map_err(|e| format!("cast ICoreWebView2_17: {e}"))?;
                    let env12: ICoreWebView2Environment12 = core
                        .cast::<ICoreWebView2_2>()
                        .and_then(|c2| c2.Environment())
                        .and_then(|env| env.cast::<ICoreWebView2Environment12>())
                        .map_err(|e| format!("reach Environment12: {e}"))?;

                    let buf = env12
                        .CreateSharedBuffer(bytes)
                        .map_err(|e| format!("CreateSharedBuffer: {e}"))?;
                    let mut ptr: *mut u8 = std::ptr::null_mut();
                    buf.Buffer(&mut ptr).map_err(|e| format!("Buffer(): {e}"))?;
                    if ptr.is_null() {
                        return Err("CreateSharedBuffer returned null pointer".into());
                    }
                    // Zero the header (write/read/reserved) then stamp capacity into u32[2].
                    std::ptr::write_bytes(ptr, 0, HOP1_HEADER_BYTES);
                    (ptr as *mut u32).add(2).write_volatile(cap_frames);

                    // READ_WRITE: JS writes the consumer read-index (u32[1]) back into the buffer.
                    core17
                        .PostSharedBufferToScript(
                            &buf,
                            COREWEBVIEW2_SHARED_BUFFER_ACCESS_READ_WRITE,
                            PCWSTR(json_w.as_ptr()),
                        )
                        .map_err(|e| format!("PostSharedBufferToScript: {e}"))?;

                    // Park the owning COM reference for the UI-thread Close() at unload
                    // (`SharedBufferHandle`); the mapping stays valid for the plugin's life.
                    Ok((ptr as usize, SharedBufferHandle(buf.into_raw() as usize)))
                }
            })();
            if let Err(std::sync::mpsc::SendError(result)) = tx.send(result) {
                if let Ok((_, shared_buf)) = result {
                    // The receiver timed out. The buffer was already posted, so close the native
                    // mapping here and let the frontend reject/release it by load token.
                    log::warn!("[plugin_host] slot {slot} late shared buffer lost its receiver; closing it on the UI thread");
                    shared_buf.close_on_ui_thread(slot);
                }
            }
        })
        .map_err(|e| format!("with_webview: {e}"))?;

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(e) => {
            callback_live.store(false, Release);
            Err(format!("shared ring provisioning timed out: {e}"))
        }
    }
}

/// The `Ok`/`Err` payload `create_shared_ring` ships back over its channel.
type LoadRing = Result<(usize, SharedBufferHandle), String>;

/// The format-agnostic DOWNSTREAM half of an RT producer, extracted P10.1 so the CLAP and VST3
/// producers share ONE copy of the subtle drift/resample/ring/pacing code (the only difference is
/// upstream: how each format renders its D-rate mono block). Given one D-rate mono block per
/// `publish`, it resamples D→C with rubato under the slow PI drift controller (P9.4) and
/// drop-on-full-writes the C-rate output into the hop-1 WebView2 SharedBuffer ring, mirroring the
/// JS feedback fields + controller state into `diag`; `pace()` holds the absolute-deadline cadence.
/// Built once on the RT thread (never crosses threads — holds raw mapping pointers); the resampler
/// is the sole allocation. ZERO heap allocation in `publish`/`pace` (measured by the DEV
/// alloc-shim, surfaced as `rt_allocs`).
pub(super) struct Hop1Pipe {
    rs: Async<f32>,
    out_scratch: Vec<f32>,
    out_max: usize,
    nominal_ratio: f64, // C / D
    ctrl: DriftController,
    base: *mut u8,  // hop-1 SharedBuffer mapping base (header + f32 data)
    data: *mut f32, // f32 data region (base + HOP1_HEADER_BYTES)
    cap_frames: u32,
    cap: usize,
    mask: u32,
    write_frames: u32, // producer-owned mirror of the hop-1 write index
    pacer: PaceTimer,
    period_dur: Duration,
    next_deadline: Instant,
}

impl Hop1Pipe {
    const MAX_CATCHUP_PERIODS: u32 = 4;

    /// Build the resampler (D→C poly-cubic), ring views, drift controller and pacing timer.
    /// `sample_rate` = C (ctx), `device_rate` = D (render rate), `period_frames` = the D-block size.
    pub(super) fn new(
        shared_ptr: usize,
        cap_frames: u32,
        sample_rate: f64,
        device_rate: f64,
        period_frames: u32,
    ) -> Result<Self, String> {
        let block = period_frames as usize;
        // Producer pacing clock biased by +FORCE_EPSILON_PPM relative to D (B3 — a residual the
        // static C/D ratio can't see, which the PI controller must discover).
        let eps = force_epsilon_ppm() * 1e-6;
        let period_dur =
            Duration::from_secs_f64(period_frames as f64 / (device_rate * (1.0 + eps)));
        // Nominal ratio = out/in = C/D; the controller multiplies it by a relative trim each block.
        // FixedAsync::Input → input chunk fixed at `block`, output VARIES (advance by `produced`).
        // Poly Cubic over sinc: we change the ratio EVERY block and sinc doesn't recompute its
        // anti-alias filters on a ratio change (continuous aliasing); poly has no such filter.
        let nominal_ratio = sample_rate / device_rate;
        let rs = Async::<f32>::new_poly(
            nominal_ratio,
            MAX_RESAMPLE_RATIO_RELATIVE,
            PolynomialDegree::Cubic,
            block,
            1, // mono
            FixedAsync::Input,
        )
        .map_err(|e| format!("rubato Async::new_poly failed: {e}"))?;
        let out_max = rs.output_frames_max(); // hard upper bound → out_scratch never reallocs
        let out_scratch = vec![0.0f32; out_max];
        let ctrl = DriftController::new(sample_rate, device_rate, period_frames);
        let base = shared_ptr as *mut u8;
        // SAFETY: `shared_ptr` is the mapped base of the SharedBuffer from create_shared_ring,
        // alive for this producer's life; the f32 data region begins at HOP1_HEADER_BYTES (28,
        // 4-byte aligned → f32-aligned).
        let data = unsafe { base.add(HOP1_HEADER_BYTES) as *mut f32 };
        // Resume from the cursor already published in the header, not from 0: a fresh load has a
        // zeroed header (create_shared_ring), but a producer respawned by a plugin-requested restart
        // shares the ring with a JS reader that is ~N frames in — restarting at 0 makes `used` wrap,
        // every block drop as "ring full" and the worklet underrun until unload (measured 2026-09-10).
        // SAFETY: header word [0] is the producer-owned write index (4-byte aligned, TSO-visible).
        let write_frames = unsafe { AtomicU32::from_ptr(base as *mut u32) }.load(Acquire);
        Ok(Self {
            rs,
            out_scratch,
            out_max,
            nominal_ratio,
            ctrl,
            base,
            data,
            cap_frames,
            cap: cap_frames as usize,
            mask: cap_frames - 1,
            write_frames,
            pacer: PaceTimer::new(),
            period_dur,
            next_deadline: Instant::now() + period_dur,
        })
    }

    /// The controller's learned clock drift (ppm), handed to the producer that replaces this one
    /// (`RtExit`) so a plugin-requested restart does not relearn it. Same value the gate reports.
    pub(super) fn drift_ppm(&self) -> f64 {
        self.ctrl.drift_ppm()
    }

    /// Seed the drift controller from a predecessor's `drift_ppm` (a respawn after a restart). A
    /// fresh load passes 0.0, which leaves the controller at its construction state.
    pub(super) fn resume_drift_ppm(&mut self, ppm: f64) {
        if ppm != 0.0 {
            self.ctrl.resume_ppm(ppm);
        }
    }

    /// P11.3 live buffer: swap the block-dependent internals to a new D-block WITHOUT a full
    /// `::new`. PRESERVES two things a fresh build would clobber:
    ///   (a) `write_frames` — the producer's mirror of the hop-1 ring write cursor JS reads
    ///       (header[0]); resetting it mid-stream desyncs the ring (JS `read` is already advanced
    ///       → underflow/garbage). MANDATORY correctness.
    ///   (b) the controller's learned `integ` (via `ctrl.set_block`, NOT `::new`) — the ~400ppm
    ///       crystal-drift correction is block-independent; a full rebuild re-discovers it over
    ///       ~100s. `nominal_ratio` (= C/D) is likewise block-independent → reused.
    /// Builds the new resampler into a temp FIRST and only mutates `self` on success, so a
    /// (provably can't-happen: known-good ratio + valid block) failure leaves the pipe fully intact
    /// at the old block rather than half-updated. Reallocs `rs`/`out_scratch` — fine, the caller
    /// fires this only on a config-generation bump (a user buffer pick), OUTSIDE the rt_alloc
    /// guard, not steady state. The only transient is a one-block pacing hiccup (`next_deadline`
    /// reset) + a few-block resampler re-prime, both inside the ~30ms hop-2 buffer (no underrun).
    pub(super) fn rebuild_for_block(&mut self, device_rate: f64, period_frames: u32) -> Result<(), String> {
        let block = period_frames as usize;
        // nominal_ratio (C/D) is block-INDEPENDENT → reuse; only the fixed input chunk changes.
        let rs = Async::<f32>::new_poly(
            self.nominal_ratio,
            MAX_RESAMPLE_RATIO_RELATIVE,
            PolynomialDegree::Cubic,
            block,
            1,
            FixedAsync::Input,
        )
        .map_err(|e| format!("rubato hop1 rebuild_for_block failed: {e}"))?;
        let out_max = rs.output_frames_max();
        let eps = force_epsilon_ppm() * 1e-6;
        self.period_dur =
            Duration::from_secs_f64(period_frames as f64 / (device_rate * (1.0 + eps)));
        self.next_deadline = Instant::now() + self.period_dur;
        self.rs = rs;
        self.out_max = out_max;
        self.out_scratch = vec![0.0f32; out_max];
        self.ctrl.set_block(period_frames, device_rate);
        // PRESERVE write_frames / base / data / cap_frames / cap / mask / pacer / nominal_ratio.
        Ok(())
    }

    /// Publish one D-rate mono block: fold its peak into the gate's `out_peak` (note-routing
    /// proof), step the drift controller when `engaged` (after warmup), resample D→C, drop-on-full
    /// write into the hop-1 ring (Release publish), and mirror the JS feedback + controller state
    /// into `diag`. No heap allocation.
    pub(super) fn publish(&mut self, mono: &[f32], engaged: bool, diag: &ProducerDiag) {
        // hop-1 header atomics over the mapping. SAFETY: the seven header u32s are 4-byte aligned
        // at offsets 0..24 over `base` (alive for self's life); from_ptr yields a borrow valid for
        // this call. [0]=write (Rust→JS, Release), [1]=read (JS→Rust, Acquire), [3..6] = the
        // JS-written feedback fields (Rust acquire-loads them; see the layout doc).
        let write_idx = unsafe { AtomicU32::from_ptr(self.base as *mut u32) };
        let read_idx = unsafe { AtomicU32::from_ptr((self.base as *mut u32).add(1)) };
        let hop2_fill_idx = unsafe { AtomicU32::from_ptr((self.base as *mut u32).add(3)) };
        let consumed_idx = unsafe { AtomicU32::from_ptr((self.base as *mut u32).add(4)) };
        let underruns_idx = unsafe { AtomicU32::from_ptr((self.base as *mut u32).add(5)) };
        let js_dropped_idx = unsafe { AtomicU32::from_ptr((self.base as *mut u32).add(6)) };

        // out_peak (note-routing proof): max |mono| folded into the windowed peak the gate reads.
        let mut blk_peak = 0.0f32;
        for &s in mono {
            let a = s.abs();
            if a > blk_peak {
                blk_peak = a;
            }
        }
        if blk_peak > f32::from_bits(diag.out_peak_bits.load(Relaxed)) {
            diag.out_peak_bits.store(blk_peak.to_bits(), Relaxed);
        }

        // Drift control. PV = hop-2 fill (header[3], discard-neutral — B2). Engage only after
        // warmup (let rubato's output_delay + flush transient settle before the integrator winds);
        // resample every block regardless so the pipe flows.
        let hop2_fill = hop2_fill_idx.load(Acquire);
        let rel = if engaged { self.ctrl.step(hop2_fill as f64) } else { 1.0 };
        // Clamp to the controller's authority (B1): the ctor band (1.02) is strictly wider, so a
        // clamped relative ratio always stays inside rubato's band and never hard-errors.
        let clamped = rel.clamp(1.0 / (1.0 + MAX_REL_CORR), 1.0 + MAX_REL_CORR);
        if self.rs.set_resample_ratio_relative(clamped, true).is_err() {
            diag.ratio_clamp_fails.fetch_add(1, Relaxed); // observable, never silent (B1)
        }
        // Resample D→C. FixedAsync::Input → exactly `block` input frames; output VARIES, so advance
        // the ring by `produced`. RT-safe: adapter/process errors log + produce 0 (a counted
        // underrun the gate catches) rather than panic the audio thread.
        let block = mono.len();
        let produced = match SequentialSlice::new(mono, 1, block) {
            Ok(input) => match SequentialSlice::new_mut(&mut self.out_scratch, 1, self.out_max) {
                Ok(mut output) => match self.rs.process_into_buffer(&input, &mut output, None) {
                    Ok((_in_used, p)) => p,
                    Err(_) => {
                        diag.latch_rt_fault(RtFault::Hop1Resample);
                        0
                    }
                },
                Err(_) => {
                    diag.latch_rt_fault(RtFault::Hop1Resample);
                    0
                }
            },
            Err(_) => {
                diag.latch_rt_fault(RtFault::Hop1Resample);
                0
            }
        };

        // Drop-on-full write of the C-rate out_scratch[..produced] into the hop-1 ring. read_idx
        // is the JS consumer's position (Acquire — observe its plain stores); publish write_idx
        // with Release so the data copy is visible to JS *before* the index advances.
        let read_frames = read_idx.load(Acquire);
        let used = self.write_frames.wrapping_sub(read_frames);
        let free = self.cap_frames.saturating_sub(used);
        let to_write = produced.min(free as usize);
        let dropped = produced - to_write;
        if to_write > 0 {
            let startpos = (self.write_frames & self.mask) as usize;
            let first = to_write.min(self.cap - startpos);
            // SAFETY: startpos < cap; (startpos+first) ≤ cap and (to_write-first) ≤ startpos, so
            // both copies stay within the cap_frames-element f32 mapping. to_write ≤ produced ≤
            // out_max = out_scratch.len(), so the source reads stay in bounds.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.out_scratch.as_ptr(),
                    self.data.add(startpos),
                    first,
                );
                if to_write > first {
                    std::ptr::copy_nonoverlapping(
                        self.out_scratch.as_ptr().add(first),
                        self.data,
                        to_write - first,
                    );
                }
            }
            self.write_frames = self.write_frames.wrapping_add(to_write as u32);
            write_idx.store(self.write_frames, Release);
        }

        diag.frames_written.fetch_add(produced as u64, Relaxed); // C-frames produced
        if dropped > 0 {
            diag.frames_dropped.fetch_add(dropped as u64, Relaxed);
        }
        diag.ring_used
            .store(self.write_frames.wrapping_sub(read_idx.load(Relaxed)) as usize, Relaxed);

        // Mirror the JS feedback fields ([3..6]) + controller state for the gate emitter (owner
        // thread). Relaxed stores, no alloc. `actual_ratio` = nominal * clamped (what rubato applied).
        diag.hop2_fill.store(hop2_fill, Relaxed);
        diag.consumed.store(consumed_idx.load(Acquire), Relaxed);
        diag.underruns.store(underruns_idx.load(Acquire), Relaxed);
        diag.js_dropped.store(js_dropped_idx.load(Acquire), Relaxed);
        let actual_ratio = self.nominal_ratio * clamped;
        diag.resample_ratio_bits.store(actual_ratio.to_bits(), Relaxed);
        diag.drift_ppm_bits.store(self.ctrl.drift_ppm().to_bits(), Relaxed);
        // Running min/max of the actual ratio (sole writer = this RT thread; owner resets the
        // window at each emit) → the gate's `ratio_spread` (B3 loop-engaged proof).
        if actual_ratio < f64::from_bits(diag.ratio_min_bits.load(Relaxed)) {
            diag.ratio_min_bits.store(actual_ratio.to_bits(), Relaxed);
        }
        if actual_ratio > f64::from_bits(diag.ratio_max_bits.load(Relaxed)) {
            diag.ratio_max_bits.store(actual_ratio.to_bits(), Relaxed);
        }
    }

    /// Hold the absolute-deadline cadence: MEAN rate exact regardless of per-wait overshoot (a long
    /// wait is followed by a short one); the high-res `PaceTimer` trims residual jitter. A stall
    /// longer than MAX_CATCHUP periods re-anchors so a recovered hang can't burst-flood hop-1.
    pub(super) fn pace(&mut self) {
        let now = Instant::now();
        if now < self.next_deadline {
            self.pacer.wait(self.next_deadline - now);
            self.next_deadline += self.period_dur;
        } else {
            let behind = now - self.next_deadline;
            self.next_deadline = if behind > self.period_dur * Self::MAX_CATCHUP_PERIODS {
                now + self.period_dur
            } else {
                self.next_deadline + self.period_dur
            };
        }
    }
}

/// P11.3 (latency): cpal→RT input-ring fill setpoint, in SECONDS. The `InPipe` DriftController holds
/// the ring at this fill — pure live-monitor latency (it sits in front of the plugin, ahead of both
/// branches). Distinct from hop-2's `TARGET_FILL_SECONDS` (30ms): that one buffers the lag-tolerant
/// looper-record / WebView2 path and rightly stays generous; the input ring only needs to absorb the
/// phase + jitter between the cpal capture callback (WASAPI-shared period ~10ms) and the QPC-paced RT
/// drain (~10ms block), so ~1.5 callback periods is ample. Lower = less monitor delay; too low → the
/// ring underruns (counted as `input_starves`). Measure-tuned; ASIO's tighter, jitter-free callback
/// will let this drop much further. Reuse hop-2's 30ms here (the old `DriftController::new` default)
/// only if a WASAPI-shared input device starves at 15ms.
const INPUT_TARGET_WASAPI: f64 = 0.015;
/// ASIO tier: with the RT block capped to ASIO_MAX_BLOCK_FRAMES the per-block consume is smaller, so
/// the input ring can hold less. Measure-tuned (the gate's `input_starves` is the floor finder).
/// Both compile in an `asio` build; `InPipe::new` receives the actual captured stream backend, so a
/// WASAPI fallback uses the roomier WASAPI value even if the preference changes after the arm.
const INPUT_TARGET_ASIO: f64 = 0.012;

/// P11 (input-SRC) — the cpal→plugin INPUT resampler, the mirror image of `Hop1Pipe`. cpal
/// captures the guitar/line signal at the device's native rate (`R_in`); the plugin is activated +
/// rendered at `D`. `FixedAsync::Output` ⇒ exactly one `block` (= `period_frames`) D-rate mono
/// output per call, consuming a VARYING number of `R_in` input frames. A `DriftController` on the
/// cpal-ring fill trims the ratio each block to cancel the residual crystal drift between the cpal
/// capture clock and the RT pace clock — SAME PI, SAME SIGN as the output side: ring over-full ⇒
/// rel < 1 ⇒ ratio↓ ⇒ consume MORE input ⇒ the surplus drains. The static rate mismatch (e.g.
/// 44.1k→48k) is the NOMINAL ratio `D/R_in`; the ±1% controller only mops up the ~400 ppm residual
/// (so even an 8.8% rate gap is in range). It is built on the RT thread on each arm/disarm/device-
/// swap (a rare, user-initiated control event), kept OUTSIDE the rt_alloc guard so steady-state
/// `rt_allocs:0` holds. NB: the build DOES allocate (rubato buffers + scratch) on the render
/// thread — an accepted one-shot cost at the moment of arming (a non-performance instant, absorbed
/// by the ~30ms hop-2 output buffer). An off-RT owner-thread build + handover would remove even
/// that, at the cost of a bidirectional pipe-handoff ring; judged disproportionate for a one-block
/// hiccup. fill_block (the per-block hot path) is strictly alloc-free.
pub(super) struct InPipe {
    rs: Async<f32>,
    in_scratch: Vec<f32>, // R_in-rate frames pulled from the cpal ring; sized to input_frames_max
    in_max: usize,
    nominal_ratio: f64, // D / R_in — block-INDEPENDENT; reused by set_block (P11.3 live buffer)
    ctrl: DriftController,
    primed: bool, // false until the ring first holds a full chunk (suppresses arm-time starves)
}
impl InPipe {
    /// Build the `R_in`→`D` resampler + drift controller. `in_rate` = the cpal device's native
    /// capture rate (Hz); `device_rate` = `D`; `period_frames` = the D-block size (= output chunk).
    pub(super) fn new(
        in_rate: u32,
        device_rate: f64,
        period_frames: u32,
        is_asio: bool,
    ) -> Result<Self, String> {
        let block = period_frames as usize;
        let r_in = (in_rate as f64).max(1.0);
        // ratio = out/in = D/R_in. FixedAsync::Output ⇒ output fixed at `block`, input varies. Poly
        // Cubic (not sinc): the ratio is retuned every block and sinc would recompute its anti-alias
        // filters on each change (continuous aliasing) — same reasoning as the output Hop1Pipe.
        let nominal_ratio = device_rate / r_in;
        let rs = Async::<f32>::new_poly(
            nominal_ratio,
            MAX_RESAMPLE_RATIO_RELATIVE,
            PolynomialDegree::Cubic,
            block,
            1, // mono
            FixedAsync::Output,
        )
        .map_err(|e| format!("rubato input Async::new_poly failed: {e}"))?;
        // input_frames_max() = the ctor-band (1.02) upper bound on per-call input. NOT a trivially
        // safe ceiling: for FixedAsync::Output, input_frames_next adds the FULL interpolator_len +
        // last_index, while input_frames_max adds only interpolator_len/2 — so at a tiny block size
        // they can converge. At the real ~480-frame WASAPI period there is comfortable headroom
        // (need≈448 < max≈454) AND the per-call drift clamp (±1.01) stays strictly inside the ctor
        // band (1.02), so need ≤ in_max holds; fill_block clamps with `.min(in_max)` as a genuine
        // safety bound (+ a DEV debug_assert), not pure defense.
        let in_max = rs.input_frames_max();
        let in_scratch = vec![0.0f32; in_max];
        // Reuse DriftController with R_in in the role of sr_ctx: target = input_target·R_in
        // (a fill setpoint in R_in-frames) and block_dt = period_frames/D (one process() per
        // wall-clock block, paced at D — identical to the output side). A SMALL target (not hop-2's
        // 30ms `DriftController::new` default) because this ring is pure live-monitor latency, not the
        // lag-tolerant looper/WebView2 path — see INPUT_TARGET_WASAPI/ASIO.
        // The captured stream backend chooses the target: ASIO's tighter, jitter-free callback lets
        // the ring sit lower; WASAPI needs the roomier setpoint. `set_block` then preserves it.
        let input_target = if is_asio {
            INPUT_TARGET_ASIO
        } else {
            INPUT_TARGET_WASAPI
        };
        let ctrl =
            DriftController::with_target(r_in, device_rate, period_frames, input_target);
        Ok(Self {
            rs,
            in_scratch,
            in_max,
            nominal_ratio,
            ctrl,
            primed: false,
        })
    }

    /// P11.3 live buffer: re-block the R_in→D resampler to a new D-block, mid-stream, WITHOUT a
    /// full `::new` — preserves the controller's learned input drift `integ` (via `ctrl.set_block`)
    /// so the buffer change doesn't re-trigger PI re-convergence. `nominal_ratio` (= D/R_in) is
    /// block-independent → reused. Resets `primed`: the fresh resampler re-primes its delay line
    /// over the first few blocks (a larger `input_frames_next`), exactly like a fresh arm, so this
    /// suppresses the spurious partial-starve count that a still-`primed` controller would log
    /// during the re-prime — the cpal ring is NOT flushed (live audio stays), only the resampler is
    /// new. Builds the resampler into a temp first; `self` is only mutated on success. Reallocs
    /// `in_scratch` — outside the rt_alloc guard (caller fires on a config-generation change only).
    pub(super) fn set_block(&mut self, device_rate: f64, period_frames: u32) -> Result<(), String> {
        let block = period_frames as usize;
        let rs = Async::<f32>::new_poly(
            self.nominal_ratio,
            MAX_RESAMPLE_RATIO_RELATIVE,
            PolynomialDegree::Cubic,
            block,
            1,
            FixedAsync::Output,
        )
        .map_err(|e| format!("rubato input set_block failed: {e}"))?;
        let in_max = rs.input_frames_max();
        self.rs = rs;
        self.in_max = in_max;
        self.in_scratch = vec![0.0f32; in_max];
        self.ctrl.set_block(period_frames, device_rate);
        self.primed = false;
        Ok(())
    }

    /// Produce one `block`-frame D-rate mono block into `out[..block]` from the cpal ring. Steps
    /// the drift controller on the pre-drain ring fill, resamples `R_in`→`D`, and zero-fills on
    /// starve (silence, never stale). No heap allocation. `engaged` gates the controller (after the
    /// global warmup); priming additionally holds it off until the ring first carries a full chunk.
    pub(super) fn fill_block(
        &mut self,
        in_rx: &mut Consumer<f32>,
        out: &mut [f32],
        block: usize,
        engaged: bool,
        diag: &ProducerDiag,
    ) {
        let avail = in_rx.slots();
        diag.input_fill.store(avail as u32, Relaxed);
        // Drift trim (once engaged + primed): step on the pre-drain fill, then re-read the input
        // count rubato now wants (set_resample_ratio_relative recomputes needed_input_size).
        let rel = if engaged && self.primed {
            self.ctrl.step(avail as f64)
        } else {
            1.0
        };
        let clamped = rel.clamp(1.0 / (1.0 + MAX_REL_CORR), 1.0 + MAX_REL_CORR);
        // Provably in-band (clamp 1.01 < ctor band 1.02) → never errors; ignore the result.
        let _ = self.rs.set_resample_ratio_relative(clamped, true);
        // need ≤ in_max holds at the real period (see InPipe::new), but the bound is not free
        // (full-interp vs half-interp), so the .min is a real safety floor: a violation would else
        // short-feed process_into_buffer → a per-block validate error → silence. DEV trips loud.
        debug_assert!(
            self.rs.input_frames_next() <= self.in_max,
            "input_frames_next {} exceeded in_max {} — widen the resampler band",
            self.rs.input_frames_next(),
            self.in_max
        );
        let need = self.rs.input_frames_next().min(self.in_max);
        let take = avail.min(need);
        for s in self.in_scratch[..take].iter_mut() {
            *s = in_rx.pop().unwrap_or(0.0);
        }
        for s in self.in_scratch[take..need].iter_mut() {
            *s = 0.0; // starve → silence into the resampler, never stale
        }
        // Prime: until the ring first holds a full chunk, treat as warmup — no starve count, the
        // controller stays disengaged above. cpal fills the ring within a few blocks of arming.
        if !self.primed {
            if avail >= need {
                self.primed = true;
            }
        } else if take < need {
            // Any short feed on a primed pipe, INCLUDING a completely empty ring (the worst dropout):
            // while armed cpal pushes every callback, silence included, and disarm drops the pipe via
            // an `input_gen` bump, so avail==0 here is real starvation, not idle. The disarm race is at
            // most one block.
            diag.input_starves.fetch_add(1, Relaxed);
        }
        diag.input_drift_ppm_bits
            .store(self.ctrl.drift_ppm().to_bits(), Relaxed);
        // Resample R_in→D into out[..block]. FixedAsync::Output ⇒ produced == block; on any
        // adapter/process error, produce 0 and zero the block (silence) rather than panic.
        let produced = match SequentialSlice::new(&self.in_scratch[..need], 1, need) {
            Ok(input) => match SequentialSlice::new_mut(&mut out[..block], 1, block) {
                Ok(mut output) => match self.rs.process_into_buffer(&input, &mut output, None) {
                    Ok((_used, p)) => p,
                    Err(_) => {
                        diag.latch_rt_fault(RtFault::InputResample);
                        0
                    }
                },
                Err(_) => {
                    diag.latch_rt_fault(RtFault::InputResample);
                    0
                }
            },
            Err(_) => {
                diag.latch_rt_fault(RtFault::InputResample);
                0
            }
        };
        for s in out[produced..block].iter_mut() {
            *s = 0.0;
        }
    }
}

/// P11.3: monitor-ring fill setpoint. Small (low monitor latency) yet ≥ a cpal callback period +
/// producer jitter so steady state doesn't underrun. By-ear/measure-tuned later; 20ms is a safe,
/// far-below-branch-2 (~30ms + WebView2) start.
const MONITOR_TARGET_WASAPI: f64 = 0.020;
/// ASIO tier: the cpal-out callback pops ~256-frame (~5.8ms) ASIO buffers (vs WASAPI's ~480), so the
/// monitor ring floor is lower → tighter setpoint = less monitor latency. `OutMonitorPipe::new`
/// receives the actual captured stream backend; a WASAPI fallback uses the roomier 20ms value.
const MONITOR_TARGET_ASIO: f64 = 0.010;

/// P11.3 — the RT→cpal-out MONITOR pipe (branch-1), sibling of `InPipe`. Takes the same D-rate wet
/// mono block `Hop1Pipe` publishes and pushes it toward the native cpal output stream (the low-
/// latency live monitor) on a SECOND ring. Resamples D→`R_out` (the cpal output device rate; PROD
/// D==R_out ⇒ ratio 1.0, only the ~400ppm QPC↔card residual is trimmed; DEV FORCE_DEVICE_RATE makes
/// it a real conversion). A `DriftController` with a SMALL fill setpoint (MONITOR_TARGET_WASAPI/ASIO, low
/// latency) holds the mon ring near that fill — the producer pace stays the master (QPC at D), the
/// monitor only trims its own output ratio, so no third clock. Drop-on-full push (consumer lagging).
/// (Re)built on the RT thread on each monitor arm/disarm/device-swap (`monitor_gen`), OUTSIDE the
/// rt_alloc guard (the rubato build allocates — a one-shot arm cost like `InPipe`). `publish` is
/// strictly alloc-free.
pub(super) struct OutMonitorPipe {
    rs: Async<f32>,
    out_scratch: Vec<f32>,
    out_max: usize,
    nominal_ratio: f64, // R_out / D — block-INDEPENDENT; reused by set_block (P11.3 live buffer)
    ctrl: DriftController,
    cap: usize,
}
impl OutMonitorPipe {
    pub(super) fn new(
        out_rate: u32,
        device_rate: f64,
        period_frames: u32,
        cap: usize,
        is_asio: bool,
    ) -> Result<Self, String> {
        let block = period_frames as usize;
        let r_out = (out_rate as f64).max(1.0);
        // ratio = out/in = R_out/D. FixedAsync::Input → fixed `block` D-frames in, output varies
        // (advance the ring by `produced`). Poly Cubic (ratio retuned every block) — same reasoning
        // as Hop1Pipe (sinc would recompute its anti-alias filters on each change).
        let nominal_ratio = r_out / device_rate;
        let rs = Async::<f32>::new_poly(
            nominal_ratio,
            MAX_RESAMPLE_RATIO_RELATIVE,
            PolynomialDegree::Cubic,
            block,
            1, // mono
            FixedAsync::Input,
        )
        .map_err(|e| format!("rubato monitor Async::new_poly failed: {e}"))?;
        let out_max = rs.output_frames_max();
        let out_scratch = vec![0.0f32; out_max];
        // Setpoint in R_out-frames (the cpal callback pops at R_out); small target = low monitor
        // latency. block_dt = period_frames/D (one publish per wall-clock block, paced at D).
        // The captured stream backend chooses the target; see InPipe::new.
        let monitor_target = if is_asio {
            MONITOR_TARGET_ASIO
        } else {
            MONITOR_TARGET_WASAPI
        };
        let ctrl = DriftController::with_target(
            r_out,
            device_rate,
            period_frames,
            monitor_target,
        );
        Ok(Self {
            rs,
            out_scratch,
            out_max,
            nominal_ratio,
            ctrl,
            cap,
        })
    }

    /// P11.3 live buffer: re-block the D→R_out monitor resampler to a new D-block, mid-stream,
    /// WITHOUT a full `::new` — preserves the learned monitor drift `integ` (via `ctrl.set_block`).
    /// `nominal_ratio` (= R_out/D) is block-independent → reused; `cap` (the mon-ring size) is
    /// preserved. Builds into a temp first; `self` mutated only on success. Reallocs `out_scratch` —
    /// outside the rt_alloc guard (caller fires on a config-generation change only). The mon
    /// ring's ~20ms (MONITOR_TARGET_WASAPI/ASIO) buffer absorbs the few-block resampler re-prime.
    pub(super) fn set_block(&mut self, device_rate: f64, period_frames: u32) -> Result<(), String> {
        let block = period_frames as usize;
        let rs = Async::<f32>::new_poly(
            self.nominal_ratio,
            MAX_RESAMPLE_RATIO_RELATIVE,
            PolynomialDegree::Cubic,
            block,
            1,
            FixedAsync::Input,
        )
        .map_err(|e| format!("rubato monitor set_block failed: {e}"))?;
        let out_max = rs.output_frames_max();
        self.rs = rs;
        self.out_max = out_max;
        self.out_scratch = vec![0.0f32; out_max];
        self.ctrl.set_block(period_frames, device_rate);
        Ok(())
    }

    /// Publish one D-rate wet mono block toward the native monitor: step the drift controller on the
    /// mon-ring fill (producer-side `cap - tx.slots()`), resample D→R_out, drop-on-full push. Silence
    /// on a resampler error (never a panic on the audio thread). No heap allocation.
    pub(super) fn publish(
        &mut self,
        mono: &[f32],
        engaged: bool,
        tx: &mut Producer<f32>,
        diag: &ProducerDiag,
    ) {
        let free = tx.slots();
        let used = self.cap.saturating_sub(free);
        diag.monitor_fill.store(used as u32, Relaxed);
        let rel = if engaged { self.ctrl.step(used as f64) } else { 1.0 };
        let clamped = rel.clamp(1.0 / (1.0 + MAX_REL_CORR), 1.0 + MAX_REL_CORR);
        let _ = self.rs.set_resample_ratio_relative(clamped, true);
        let block = mono.len();
        let produced = match SequentialSlice::new(mono, 1, block) {
            Ok(input) => match SequentialSlice::new_mut(&mut self.out_scratch, 1, self.out_max) {
                Ok(mut output) => match self.rs.process_into_buffer(&input, &mut output, None) {
                    Ok((_in_used, p)) => p,
                    Err(_) => {
                        diag.latch_rt_fault(RtFault::MonitorResample);
                        0
                    }
                },
                Err(_) => {
                    diag.latch_rt_fault(RtFault::MonitorResample);
                    0
                }
            },
            Err(_) => {
                diag.latch_rt_fault(RtFault::MonitorResample);
                0
            }
        };
        // Drop-on-full: push what the ring holds; a lagging consumer loses the surplus. Nothing on the
        // consumer side can see that loss (a FULL ring never underruns ⇒ no starve), so count the
        // dropped frames here — one relaxed add per short push, never per sample.
        let mut pushed = 0usize;
        for &s in &self.out_scratch[..produced] {
            if tx.push(s).is_err() {
                break;
            }
            pushed += 1;
        }
        if pushed < produced {
            diag.monitor_overruns
                .fetch_add((produced - pushed) as u64, Relaxed);
        }
        diag.monitor_drift_ppm_bits
            .store(self.ctrl.drift_ppm().to_bits(), Relaxed);
    }
}
