//! DEV Stage 1 premise spike (`docs/plans/native-engine.md` § Stage 1): does ONE native callback
//! give calibration-free alignment, a low round trip with a plugin in the callback, and an
//! explainable WASAPI round trip? Standalone: `app.exe --probe-engine-spike …` exits before Tauri
//! starts and touches no production path. A child module of `vst3.rs` so it reaches the VST3 host's
//! private load and activation items without widening their visibility.
//!
//! A 64-frame Hann-windowed chirp leaves on output channel `--out` every `EMIT_PERIOD` frames and
//! comes back through a loopback cable on input channel `--in`. The input is recorded on the output
//! callback's frame counter (ASIO: one callback, so one counter) or on its own counter with cpal's
//! QPC stamps (WASAPI), and the chirps are cross-correlated after the run. `--echo` also sends the
//! (plugin-processed) input back out at a gain that keeps the loop well below 1, so each chirp comes
//! round the cable a second time: the echo spacing is the round trip a player hears.
//!
//! ASIO: asio-sys 0.3.0 runs every registered buffer callback, in registration order, inside one
//! bufferSwitch. The input stream is built first and plays first, so its callback copies the block into
//! the engine's handoff and bumps a cycle count; the output callback then sees `sameCycle` when that
//! count is one ahead of its own. cpal gives each ASIO stream its own TimeBase and the rig's driver
//! trips cpal's timestamp overflow, so instants are never compared across streams: inLat and outLat
//! come from deltas within one callback's info, gaps and block times from `Instant` (QPC).
//!
//! WASAPI: each stream has its own cpal thread; the input callback pushes into an rtrb ring and the
//! output callback pops it (the Stage 4 join). Every callback thread is promoted to MMCSS Pro Audio on
//! first entry. Alignment uses cpal's QPC stamps only (both streams share that clock).

use super::*;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

/// The device's own rate, read once before any stream opens. The spike never changes it: the
/// device stays at whatever rate its owner set.
static RATE_HZ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
fn rate() -> u32 {
    RATE_HZ.load(Relaxed)
}
const CHIRP_LEN: usize = 64;
const CHIRP_AMP: f32 = 0.5;
/// Callbacks in the first seconds are excluded from gaps, block time and allocation counts (C1).
const SETTLE_SECONDS: f64 = 5.0;
/// Chirps start after this many output frames, so the streams have settled.
fn emit_start() -> u64 {
    rate() as u64
}
/// The echo gain is measured over this many frames of dry chirps after the first chirp.
fn gain_probe_frames() -> u64 {
    2 * rate() as u64
}
/// The loop gain the echo run aims for (cable gain × monitor gain): one clear echo that dies fast.
const ECHO_LOOP_GAIN: f32 = 0.25;
const MAX_MONITOR_GAIN: f32 = 0.5;
/// Largest plugin block; a WASAPI callback longer than this is processed in slices.
const MAX_PLUGIN_FRAMES: usize = 4096;
/// Block-time histogram: 1 µs bins up to 50 ms; longer blocks land in the last bin.
const HIST_BINS: usize = 50_000;
/// Preallocated per-callback stamp records (WASAPI) — about 20 min at a 10 ms period.
const MARKS_CAP: usize = 200_000;

struct Backend {
    asio: bool,
    /// Chirp spacing and search window, in seconds (the frames follow the device rate).
    period_s: f64,
    window_s: f64,
}

impl Backend {
    fn period(&self) -> u64 {
        (self.period_s * rate() as f64) as u64
    }
    fn window(&self) -> u64 {
        (self.window_s * rate() as f64) as u64
    }
}

const ASIO_BACKEND: Backend = Backend { asio: true, period_s: 0.25, window_s: 0.085 };
const WASAPI_BACKEND: Backend = Backend { asio: false, period_s: 1.0, window_s: 0.5 };

struct Args {
    backend: Backend,
    block: Option<u32>,
    plugin: Option<String>,
    in_ch: usize,
    out_ch: usize,
    seconds: f64,
    echo: bool,
    /// No chirps: the output stays silent (C1 and plugin checks need no cable).
    quiet: bool,
    device: Option<String>,
    /// ASIO: open and close the driver once at another block size first (the same-size relaunch test).
    preopen: bool,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    const USAGE: &str = "usage: --probe-engine-spike <asio|wasapi> <64|128|256|default> [--plugin <file.vst3>] \
        [--in N] [--out N] [--minutes N | --seconds N] [--echo] [--quiet] [--preopen] [--device <name substring, WASAPI>]";
    let backend = match args.first().map(String::as_str) {
        Some("asio") => ASIO_BACKEND,
        Some("wasapi") => WASAPI_BACKEND,
        _ => return Err(USAGE.into()),
    };
    let block = match args.get(1).map(String::as_str) {
        Some("default") => None,
        Some(b @ ("64" | "128" | "256")) => Some(b.parse().unwrap()),
        _ => return Err(USAGE.into()),
    };
    let mut parsed = Args {
        backend,
        block,
        plugin: None,
        in_ch: 1,
        out_ch: 1,
        seconds: 60.0,
        echo: false,
        quiet: false,
        device: None,
        preopen: false,
    };
    let mut rest = args[2..].iter();
    while let Some(flag) = rest.next() {
        let mut value = || rest.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
        let number = |v: String| v.parse::<f64>().map_err(|e| format!("{flag}: {e}"));
        match flag.as_str() {
            "--plugin" => parsed.plugin = Some(value()?),
            "--in" => parsed.in_ch = number(value()?)? as usize,
            "--out" => parsed.out_ch = number(value()?)? as usize,
            "--minutes" => parsed.seconds = number(value()?)? * 60.0,
            "--seconds" => parsed.seconds = number(value()?)?,
            "--device" => parsed.device = Some(value()?),
            "--echo" => parsed.echo = true,
            "--quiet" => parsed.quiet = true,
            "--preopen" => parsed.preopen = true,
            _ => return Err(format!("unknown argument {flag}\n{USAGE}")),
        }
    }
    if parsed.quiet && parsed.echo {
        return Err("--quiet and --echo exclude each other".into());
    }
    if !(3.0..=1800.0).contains(&parsed.seconds) {
        return Err("run length must be 3 s .. 30 min".into());
    }
    Ok(parsed)
}

fn chirp() -> [f32; CHIRP_LEN] {
    // Linear sweep 1 → 16 kHz over the window: a sharp, unambiguous correlation peak.
    let (f0, f1) = (1_000.0f64, 16_000.0f64);
    let t_len = CHIRP_LEN as f64 / rate() as f64;
    let mut c = [0.0f32; CHIRP_LEN];
    for (n, s) in c.iter_mut().enumerate() {
        let t = n as f64 / rate() as f64;
        let phase = 2.0 * std::f64::consts::PI * (f0 * t + (f1 - f0) * t * t / (2.0 * t_len));
        let hann = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / (CHIRP_LEN - 1) as f64).cos();
        *s = (CHIRP_AMP as f64 * hann * phase.sin()) as f32;
    }
    c
}

// ── the plugin, loaded on the main thread and processed in the callback ─────────────────────

/// The VST3 processor with its (empty) event and parameter lists. Built and torn down on the main
/// thread, processed only inside the engine lock by the callback: one thread at a time, which is
/// what the `Rc` inside the lists and the plugin's own threading contract need.
struct PluginUnit {
    processor: ComPtr<IAudioProcessor>,
    _event_list: ComWrapper<RtEventList>,
    event_list_ptr: ComPtr<IEventList>,
    _param_changes: ComWrapper<RtParamChanges>,
    param_changes_ptr: ComPtr<IParameterChanges>,
    in_bufs: Vec<Vec<f32>>,
    out_bufs: Vec<Vec<f32>>,
    in_ptrs: Vec<*mut f32>,
    out_ptrs: Vec<*mut f32>,
    processing: bool,
    process_errors: u64,
}

// SAFETY: see the type's doc — never used by two threads at once (the engine Mutex).
unsafe impl Send for PluginUnit {}

/// The rest of the load, kept for teardown in the order `vst3.rs::teardown` needs.
struct PluginHold {
    module: Vst3Module,
    factory: ComPtr<IPluginFactory>,
    component: ComPtr<IComponent>,
    hostapp: ComWrapper<LfHostApp>,
    host_ctx: ComPtr<FUnknown>,
    name: String,
    activation: Activation,
}

/// Load the bundle's first audio-effect class and activate it at the device rate, as `vst3_owner_main` does
/// (no controller: the spike never opens an editor or moves a parameter).
fn load_plugin(path: &str) -> Result<(PluginUnit, PluginHold), String> {
    let binary = super::super::super::scan::resolve_vst3_binary(std::path::Path::new(path))
        .ok_or_else(|| format!("no loadable VST3 binary inside {path}"))?;
    let wide: Vec<u16> = binary.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: FFI module load + raw FUnknown COM, the same sequence as `vst3_owner_main`; every
    // pointer is valid for its call and every failure after `initialize` goes through `teardown`.
    unsafe {
        let module = Vst3Module::load(PCWSTR(wide.as_ptr()))?;
        let gpf = GetProcAddress(module.handle(), s!("GetPluginFactory"))
            .ok_or_else(|| "GetPluginFactory not exported".to_string())?;
        let get_factory: unsafe extern "system" fn() -> *mut IPluginFactory = std::mem::transmute(gpf);
        let factory = ComPtr::from_raw(get_factory()).ok_or_else(|| "GetPluginFactory returned null".to_string())?;
        let mut component: Option<ComPtr<IComponent>> = None;
        let mut name = String::new();
        for i in 0..factory.countClasses() {
            let mut info: PClassInfo = std::mem::zeroed();
            if factory.getClassInfo(i, &mut info) != kResultOk {
                continue;
            }
            if super::super::super::scan::c_chars_to_string(&info.category) != "Audio Module Class" {
                continue;
            }
            name = super::super::super::scan::c_chars_to_string(&info.name);
            let mut obj: *mut c_void = std::ptr::null_mut();
            if factory.createInstance(info.cid.as_ptr(), vst3::Steinberg::Vst::IComponent_iid.as_ptr(), &mut obj)
                == kResultOk
                && !obj.is_null()
            {
                component = ComPtr::from_raw(obj as *mut IComponent);
            }
            break;
        }
        let component = component.ok_or_else(|| format!("no audio-effect class in {path}"))?;
        let hostapp = ComWrapper::new(LfHostApp);
        let host_ctx = hostapp.to_com_ptr::<FUnknown>().ok_or_else(|| "host app FUnknown failed".to_string())?;
        if component.initialize(host_ctx.as_ptr()) != kResultOk {
            teardown(component, host_ctx, hostapp, factory, module);
            return Err("component.initialize failed".into());
        }
        let rest = (|| -> Result<(ComPtr<IAudioProcessor>, Activation), String> {
            let processor = component
                .cast::<IAudioProcessor>()
                .ok_or_else(|| "plugin has no IAudioProcessor".to_string())?;
            let activation = activate_component(&component, &processor, rate() as f64, MAX_PLUGIN_FRAMES as u32)?;
            if activation.in_channels == 0 {
                return Err("plugin has no audio input".into());
            }
            Ok((processor, activation))
        })();
        let (processor, activation) = match rest {
            Ok(v) => v,
            Err(e) => {
                teardown(component, host_ctx, hostapp, factory, module);
                return Err(e);
            }
        };
        let event_list = ComWrapper::new(RtEventList { inner: Rc::new(EventListInner::new()) });
        let event_list_ptr = event_list.to_com_ptr::<IEventList>().ok_or("event list COM failed")?;
        let param_changes = ComWrapper::new(RtParamChanges {
            inner: Rc::new(ParamChangesInner::new(MAX_PARAM_QUEUES).ok_or("param queues failed")?),
        });
        let param_changes_ptr = param_changes.to_com_ptr::<IParameterChanges>().ok_or("param changes COM failed")?;
        let mut in_bufs = vec![vec![0.0f32; MAX_PLUGIN_FRAMES]; activation.in_channels as usize];
        let mut out_bufs = vec![vec![0.0f32; MAX_PLUGIN_FRAMES]; activation.out_channels.max(1) as usize];
        let in_ptrs = in_bufs.iter_mut().map(|c| c.as_mut_ptr()).collect();
        let out_ptrs = out_bufs.iter_mut().map(|c| c.as_mut_ptr()).collect();
        Ok((
            PluginUnit {
                processor,
                _event_list: event_list,
                event_list_ptr,
                _param_changes: param_changes,
                param_changes_ptr,
                in_bufs,
                out_bufs,
                in_ptrs,
                out_ptrs,
                processing: false,
                process_errors: 0,
            },
            PluginHold { module, factory, component, hostapp, host_ctx, name, activation },
        ))
    }
}

impl PluginUnit {
    /// Process `x` (mono, ≤ MAX_PLUGIN_FRAMES) into `wet` (the plugin's outputs summed to mono).
    fn process(&mut self, x: &[f32], wet: &mut [f32]) {
        let n = x.len();
        for ch in self.in_bufs.iter_mut() {
            ch[..n].copy_from_slice(x);
        }
        for ch in self.out_bufs.iter_mut() {
            ch[..n].fill(0.0);
        }
        // SAFETY: the buffers outlive the call and hold `n` frames per channel; the channel counts
        // are the ones the activation negotiated.
        unsafe {
            if !self.processing {
                // First callback entry: setProcessing on the thread that will call process.
                let _ = self.processor.setProcessing(1);
                self.processing = true;
            }
            let mut out_bus = AudioBusBuffers {
                numChannels: self.out_bufs.len() as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: self.out_ptrs.as_mut_ptr() },
            };
            let mut in_bus = AudioBusBuffers {
                numChannels: self.in_bufs.len() as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: self.in_ptrs.as_mut_ptr() },
            };
            let mut pd: ProcessData = std::mem::zeroed();
            pd.processMode = ProcessModes_::kRealtime;
            pd.symbolicSampleSize = SymbolicSampleSizes_::kSample32;
            pd.numSamples = n as i32;
            pd.numInputs = 1;
            pd.numOutputs = 1;
            pd.inputs = &mut in_bus;
            pd.outputs = &mut out_bus;
            pd.inputEvents = self.event_list_ptr.as_ptr();
            pd.inputParameterChanges = self.param_changes_ptr.as_ptr();
            if self.processor.process(&mut pd) != kResultOk {
                self.process_errors += 1;
            }
        }
        sum_to_mono(&self.out_bufs, wet, n, self.out_bufs.len());
    }
}

fn unload_plugin(unit: PluginUnit, hold: PluginHold) {
    // SAFETY: both streams are dropped, so no callback can reach the processor any more.
    unsafe {
        if unit.processing {
            let _ = unit.processor.setProcessing(0);
        }
    }
    drop(unit);
    let PluginHold { module, factory, component, hostapp, host_ctx, .. } = hold;
    teardown(component, host_ctx, hostapp, factory, module);
}

// ── the engine: everything the callbacks share, behind one Mutex they only try_lock ─────────

#[derive(Default)]
struct Stats {
    callbacks: u64,
    settled_callbacks: u64,
    gaps: u64,
    max_gap_us: u64,
    hist: Vec<u32>,
    block_frames_min: u64,
    block_frames_max: u64,
}

impl Stats {
    fn new() -> Self {
        Stats { hist: vec![0; HIST_BINS], block_frames_min: u64::MAX, ..Default::default() }
    }
    fn percentile_us(&self, p: f64) -> u64 {
        let total: u64 = self.hist.iter().map(|&c| c as u64).sum();
        if total == 0 {
            return 0;
        }
        let want = (total as f64 * p).ceil() as u64;
        let mut acc = 0u64;
        for (us, &c) in self.hist.iter().enumerate() {
            acc += c as u64;
            if acc >= want {
                return us as u64;
            }
        }
        HIST_BINS as u64
    }
    fn max_us(&self) -> u64 {
        self.hist.iter().rposition(|&c| c > 0).unwrap_or(0) as u64
    }
}

#[derive(Clone, Copy, Default)]
struct Mark {
    frame: u64,
    /// Output: playback instant of the first frame. Input: capture instant of the first frame.
    stamp_ns: u128,
    callback_ns: u128,
    /// Output only: frames waiting in the join ring before this callback popped.
    ring_fill: u64,
}

// The ASIO handoff fields are read only by the ASIO callbacks.
#[cfg_attr(not(feature = "asio"), allow(dead_code))]
struct Engine {
    started: Instant,
    chirp: [f32; CHIRP_LEN],
    emit: bool,
    period: u64,
    in_ch: usize,
    out_ch: usize,
    echo: bool,
    // ASIO handoff: the input callback of this cycle fills it, the output callback reads it.
    handoff: Vec<f32>,
    handoff_len: usize,
    in_cycles: u64,
    in_thread: u32,
    out_cycles: u64,
    same_cycle: u64,
    not_same_cycle: u64,
    other_thread: u64,
    // The output frame counter (ASIO: also the input's), the dry input and the rendered mono out.
    frame: u64,
    x: Vec<f32>,
    wet: Vec<f32>,
    y: Vec<f32>,
    rec_in: Vec<f32>,
    // Echo gain: measured from the dry chirps, then held.
    gain_probe_max: f32,
    monitor_gain: f32,
    loud_blocks: u32,
    runaway: bool,
    plugin: Option<PluginUnit>,
    plugin_latency: u32,
    // Delta latencies within one callback's info (ns): min/max over the run.
    in_lat_ns: (u128, u128),
    out_lat_ns: (u128, u128),
    out_stats: Stats,
    last_entry: Option<Instant>,
    period_us: u64,
    // WASAPI join.
    ring_rx: Option<Consumer<f32>>,
    ring_started: bool,
    ring_target: usize,
    starves: u64,
    out_marks: Vec<Mark>,
    mmcss_out: bool,
}

/// WASAPI input side: its own counter, recording and stamps (the output never touches it).
struct InSide {
    frame: u64,
    rec: Vec<f32>,
    marks: Vec<Mark>,
    ring_tx: Producer<f32>,
    overflows: u64,
    in_lat_ns: (u128, u128),
    stats: Stats,
    last_entry: Option<Instant>,
    mmcss: bool,
    frames_min: u64,
    frames_max: u64,
}

fn minmax(acc: &mut (u128, u128), v: u128) {
    if acc.0 == 0 || v < acc.0 {
        acc.0 = v;
    }
    acc.1 = acc.1.max(v);
}

fn record_timing(stats: &mut Stats, last: &mut Option<Instant>, entry: Instant, exit: Instant, period_us: u64, settled: bool, frames: u64) {
    stats.callbacks += 1;
    if let Some(prev) = *last {
        let gap = entry.duration_since(prev).as_micros() as u64;
        if settled {
            stats.max_gap_us = stats.max_gap_us.max(gap);
            if period_us > 0 && gap * 2 > period_us * 3 {
                stats.gaps += 1;
            }
        }
    }
    *last = Some(entry);
    if settled {
        stats.settled_callbacks += 1;
        let us = exit.duration_since(entry).as_micros() as usize;
        stats.hist[us.min(HIST_BINS - 1)] += 1;
        stats.block_frames_min = stats.block_frames_min.min(frames);
        stats.block_frames_max = stats.block_frames_max.max(frames);
    }
}

impl Engine {
    fn settled(&self, now: Instant) -> bool {
        now.duration_since(self.started).as_secs_f64() >= SETTLE_SECONDS
    }

    /// Render `n` frames from the dry input in `self.x[..n]` into `self.y[..n]` (mono), recording
    /// the input at the output frame counter when `record` (ASIO).
    fn render(&mut self, n: usize, record: bool) {
        let f0 = self.frame;
        if record {
            let end = (f0 as usize + n).min(self.rec_in.len());
            if (f0 as usize) < end {
                self.rec_in[f0 as usize..end].copy_from_slice(&self.x[..end - f0 as usize]);
            }
        }
        // The echo gain: loop gain = cable gain × monitor gain, aimed at ECHO_LOOP_GAIN.
        let peak = self.x[..n].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        if self.echo && f0 >= emit_start() && f0 < emit_start() + gain_probe_frames() {
            self.gain_probe_max = self.gain_probe_max.max(peak);
        }
        let monitor_on = self.echo && !self.runaway && f0 >= emit_start() + gain_probe_frames();
        if monitor_on && self.monitor_gain == 0.0 && self.gain_probe_max > 0.0 {
            let cable = self.gain_probe_max / CHIRP_AMP;
            self.monitor_gain = (ECHO_LOOP_GAIN / cable).min(MAX_MONITOR_GAIN);
        }
        // Runaway guard: a hot input for 20 blocks mutes the monitor for the rest of the run.
        if peak > 0.9 {
            self.loud_blocks += 1;
            if self.loud_blocks > 20 {
                self.runaway = true;
            }
        } else {
            self.loud_blocks = 0;
        }
        let mut done = 0;
        while done < n {
            let len = (n - done).min(MAX_PLUGIN_FRAMES);
            match self.plugin.as_mut() {
                Some(p) => p.process(&self.x[done..done + len], &mut self.wet[done..done + len]),
                None => self.wet[done..done + len].copy_from_slice(&self.x[done..done + len]),
            }
            done += len;
        }
        let g = if monitor_on { self.monitor_gain } else { 0.0 };
        for i in 0..n {
            let f = f0 + i as u64;
            let mut s = g * self.wet[i];
            if self.emit && f >= emit_start() {
                let k = ((f - emit_start()) % self.period) as usize;
                if k < CHIRP_LEN {
                    s += self.chirp[k];
                }
            }
            self.y[i] = s;
        }
        self.frame += n as u64;
    }
}

type Shared = Arc<Mutex<Engine>>;

struct Counters {
    lock_miss: AtomicU64,
    /// cpal's non-fatal Xrun reports (C1).
    xruns: AtomicU64,
    /// Every other stream error: the run aborts.
    errors: AtomicU64,
    /// `Engine::runaway`, mirrored so `wait_run` never takes the engine lock under a callback.
    runaway: AtomicBool,
}

fn write_out<T: SizedSample + FromSample<f32>>(data: &mut [T], channels: usize, out_ch: usize, y: &[f32]) {
    for (i, frame) in data.chunks_mut(channels).enumerate() {
        for (c, s) in frame.iter_mut().enumerate() {
            let v = if c == out_ch { y[i].clamp(-1.0, 1.0) } else { 0.0 };
            *s = T::from_sample(v);
        }
    }
}

fn silence<T: SizedSample + FromSample<f32>>(data: &mut [T]) {
    for s in data.iter_mut() {
        *s = T::from_sample(0.0f32);
    }
}

fn instant_delta_ns(later: cpal::StreamInstant, earlier: cpal::StreamInstant) -> Option<u128> {
    later.checked_duration_since(earlier).map(|d| d.as_nanos())
}

#[cfg(feature = "asio")]
fn current_thread_id() -> u32 {
    // SAFETY: no preconditions.
    unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
}

// ── ASIO: one bufferSwitch, input callback first ────────────────────────────────────────────

#[cfg(feature = "asio")]
fn asio_input_cb<T>(engine: Shared, counters: Arc<Counters>, channels: usize)
    -> impl FnMut(&[T], &cpal::InputCallbackInfo) + Send + 'static
where
    T: SizedSample,
    f32: FromSample<T>,
{
    move |data: &[T], info: &cpal::InputCallbackInfo| {
        let Ok(mut e) = engine.try_lock() else {
            counters.lock_miss.fetch_add(1, Relaxed);
            return;
        };
        let e = &mut *e;
        let n = data.len() / channels;
        let n = n.min(e.handoff.len());
        for i in 0..n {
            e.handoff[i] = data[i * channels + e.in_ch].to_sample::<f32>();
        }
        e.handoff_len = n;
        e.in_cycles += 1;
        e.in_thread = current_thread_id();
        if let Some(ns) = instant_delta_ns(info.timestamp().callback, info.timestamp().capture) {
            minmax(&mut e.in_lat_ns, ns);
        }
    }
}

#[cfg(feature = "asio")]
fn asio_output_cb<T>(engine: Shared, counters: Arc<Counters>, channels: usize)
    -> impl FnMut(&mut [T], &cpal::OutputCallbackInfo) + Send + 'static
where
    T: SizedSample + FromSample<f32>,
{
    move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
        let entry = Instant::now();
        let Ok(mut guard) = engine.try_lock() else {
            counters.lock_miss.fetch_add(1, Relaxed);
            silence(data);
            return;
        };
        let e = &mut *guard;
        let settled = e.settled(entry);
        let _alloc_guard = settled.then(crate::host::rt_alloc::guard);
        let n = (data.len() / channels).min(e.x.len());
        e.out_cycles += 1;
        let same = e.in_cycles == e.out_cycles && e.handoff_len == n;
        if e.in_thread != current_thread_id() {
            e.other_thread += 1;
        }
        if same {
            e.same_cycle += 1;
            let (x, h) = (&mut e.x, &e.handoff);
            x[..n].copy_from_slice(&h[..n]);
        } else {
            e.not_same_cycle += 1;
            // Resync the counts so one miss is counted once, not for the rest of the run.
            e.out_cycles = e.in_cycles;
            e.x[..n].fill(0.0);
        }
        if let Some(ns) = instant_delta_ns(info.timestamp().playback, info.timestamp().callback) {
            minmax(&mut e.out_lat_ns, ns);
        }
        e.render(n, true);
        counters.runaway.fetch_or(e.runaway, Relaxed);
        write_out(data, channels, e.out_ch, &e.y[..n]);
        drop(_alloc_guard);
        let (period_us, exit) = (e.period_us, Instant::now());
        record_timing(&mut e.out_stats, &mut e.last_entry, entry, exit, period_us, settled, n as u64);
    }
}

// ── WASAPI: two threads joined by a ring ────────────────────────────────────────────────────

fn wasapi_input_cb<T>(side: Arc<Mutex<InSide>>, counters: Arc<Counters>, channels: usize, in_ch: usize, started: Instant)
    -> impl FnMut(&[T], &cpal::InputCallbackInfo) + Send + 'static
where
    T: SizedSample,
    f32: FromSample<T>,
{
    move |data: &[T], info: &cpal::InputCallbackInfo| {
        let entry = Instant::now();
        let Ok(mut guard) = side.try_lock() else {
            counters.lock_miss.fetch_add(1, Relaxed);
            return;
        };
        let s = &mut *guard;
        if !s.mmcss {
            s.mmcss = true;
            let _ = super::super::promote_pro_audio();
        }
        let settled = entry.duration_since(started).as_secs_f64() >= SETTLE_SECONDS;
        let _alloc_guard = settled.then(crate::host::rt_alloc::guard);
        let n = data.len() / channels;
        let ts = info.timestamp();
        if s.marks.len() < s.marks.capacity() {
            s.marks.push(Mark {
                frame: s.frame,
                stamp_ns: ts.capture.as_nanos(),
                callback_ns: ts.callback.as_nanos(),
                ring_fill: 0,
            });
        }
        if let Some(ns) = instant_delta_ns(ts.callback, ts.capture) {
            minmax(&mut s.in_lat_ns, ns);
        }
        let f0 = s.frame as usize;
        for i in 0..n {
            let v = data[i * channels + in_ch].to_sample::<f32>();
            if f0 + i < s.rec.len() {
                s.rec[f0 + i] = v;
            }
            if s.ring_tx.push(v).is_err() {
                s.overflows += 1;
            }
        }
        s.frame += n as u64;
        s.frames_min = s.frames_min.min(n as u64);
        s.frames_max = s.frames_max.max(n as u64);
        drop(_alloc_guard);
        let exit = Instant::now();
        let (stats, last) = (&mut s.stats, &mut s.last_entry);
        record_timing(stats, last, entry, exit, 0, settled, n as u64);
    }
}

fn wasapi_output_cb<T>(engine: Shared, counters: Arc<Counters>, channels: usize)
    -> impl FnMut(&mut [T], &cpal::OutputCallbackInfo) + Send + 'static
where
    T: SizedSample + FromSample<f32>,
{
    move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
        let entry = Instant::now();
        let Ok(mut guard) = engine.try_lock() else {
            counters.lock_miss.fetch_add(1, Relaxed);
            silence(data);
            return;
        };
        let e = &mut *guard;
        if !e.mmcss_out {
            e.mmcss_out = true;
            let _ = super::super::promote_pro_audio();
        }
        let settled = e.settled(entry);
        let _alloc_guard = settled.then(crate::host::rt_alloc::guard);
        let n = (data.len() / channels).min(e.x.len());
        let ts = info.timestamp();
        let rx = e.ring_rx.as_mut().expect("WASAPI engine has a ring");
        let fill = rx.slots();
        if !e.ring_started && fill >= e.ring_target {
            // The input ran before the output opened: drop that backlog down to the target, or it
            // rides along as latency for the whole run.
            for _ in e.ring_target..fill {
                let _ = rx.pop();
            }
            e.ring_started = true;
        }
        let mut got = 0;
        if e.ring_started {
            while got < n {
                match rx.pop() {
                    Ok(v) => {
                        e.x[got] = v;
                        got += 1;
                    }
                    Err(_) => break,
                }
            }
            if got < n {
                e.starves += 1;
            }
        }
        e.x[got..n].fill(0.0);
        if e.out_marks.len() < e.out_marks.capacity() {
            let frame = e.frame;
            e.out_marks.push(Mark {
                frame,
                stamp_ns: ts.playback.as_nanos(),
                callback_ns: ts.callback.as_nanos(),
                ring_fill: fill as u64,
            });
        }
        if let Some(ns) = instant_delta_ns(ts.playback, ts.callback) {
            minmax(&mut e.out_lat_ns, ns);
        }
        e.render(n, false);
        counters.runaway.fetch_or(e.runaway, Relaxed);
        write_out(data, channels, e.out_ch, &e.y[..n]);
        drop(_alloc_guard);
        let exit = Instant::now();
        record_timing(&mut e.out_stats, &mut e.last_entry, entry, exit, e.period_us, settled, n as u64);
    }
}

macro_rules! build_input {
    ($device:expr, $cfg:expr, $fmt:expr, $counters:expr, $make:ident ( $($arg:expr),* )) => {{
        let counters = $counters.clone();
        let err = move |e: cpal::Error| {
            if e.kind() == cpal::ErrorKind::Xrun {
                counters.xruns.fetch_add(1, SeqCst);
            } else {
                counters.errors.fetch_add(1, SeqCst);
            }
        };
        match $fmt {
            cpal::SampleFormat::I32 => $device.build_input_stream::<i32, _, _>($cfg, $make::<i32>($($arg),*), err, None),
            cpal::SampleFormat::I16 => $device.build_input_stream::<i16, _, _>($cfg, $make::<i16>($($arg),*), err, None),
            cpal::SampleFormat::F32 => $device.build_input_stream::<f32, _, _>($cfg, $make::<f32>($($arg),*), err, None),
            other => return Err(format!("unsupported input sample format {other:?}")),
        }
        .map_err(|e| format!("build input stream: {e}"))?
    }};
}

macro_rules! build_output {
    ($device:expr, $cfg:expr, $fmt:expr, $counters:expr, $make:ident ( $($arg:expr),* )) => {{
        let counters = $counters.clone();
        let err = move |e: cpal::Error| {
            if e.kind() == cpal::ErrorKind::Xrun {
                counters.xruns.fetch_add(1, SeqCst);
            } else {
                counters.errors.fetch_add(1, SeqCst);
            }
        };
        match $fmt {
            cpal::SampleFormat::I32 => $device.build_output_stream::<i32, _, _>($cfg, $make::<i32>($($arg),*), err, None),
            cpal::SampleFormat::I16 => $device.build_output_stream::<i16, _, _>($cfg, $make::<i16>($($arg),*), err, None),
            cpal::SampleFormat::F32 => $device.build_output_stream::<f32, _, _>($cfg, $make::<f32>($($arg),*), err, None),
            other => return Err(format!("unsupported output sample format {other:?}")),
        }
        .map_err(|e| format!("build output stream: {e}"))?
    }};
}

// ── analysis (after the run, on the main thread) ────────────────────────────────────────────

struct Hit {
    pos: f64,
    ncc: f64,
}

/// Normalised cross-correlation of `tmpl` against `sig` at integer offsets `from..to`; the best
/// |ncc| refined to a sub-frame position by a parabola through its neighbours.
fn xcorr_peak(sig: &[f32], tmpl: &[f32], from: i64, to: i64) -> Option<Hit> {
    let t_energy: f64 = tmpl.iter().map(|&v| (v as f64) * (v as f64)).sum();
    let corr = |off: i64| -> Option<(f64, f64)> {
        if off < 0 || off as usize + tmpl.len() > sig.len() {
            return None;
        }
        let seg = &sig[off as usize..off as usize + tmpl.len()];
        let dot: f64 = seg.iter().zip(tmpl).map(|(&a, &b)| a as f64 * b as f64).sum();
        let e: f64 = seg.iter().map(|&a| a as f64 * a as f64).sum();
        Some((dot, if e > 0.0 { dot / (e * t_energy).sqrt() } else { 0.0 }))
    };
    let mut best: Option<(i64, f64, f64)> = None;
    for off in from..to {
        if let Some((dot, ncc)) = corr(off) {
            if best.is_none_or(|b| dot.abs() > b.1.abs()) {
                best = Some((off, dot, ncc));
            }
        }
    }
    let (off, dot, ncc) = best?;
    let (l, r) = (corr(off - 1).map_or(dot, |c| c.0), corr(off + 1).map_or(dot, |c| c.0));
    let (l, c, r) = (l * dot.signum(), dot.abs(), r * dot.signum());
    let denom = l - 2.0 * c + r;
    let delta = if denom.abs() > 1e-12 { (0.5 * (l - r) / denom).clamp(-0.5, 0.5) } else { 0.0 };
    Some(Hit { pos: off as f64 + delta, ncc: ncc.abs() })
}

/// The first strong arrival in `sig[from..to]` (direct), then — for echo runs — the strongest one
/// after it has rung out (the echo). Positions are absolute indices into `sig`.
fn find_arrivals(sig: &[f32], tmpl: &[f32], from: usize, to: usize, echo: bool) -> (Option<Hit>, Option<Hit>) {
    let to = to.min(sig.len());
    if from + CHIRP_LEN >= to {
        return (None, None);
    }
    let win = &sig[from..to];
    let max = win.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if max <= 0.0 {
        return (None, None);
    }
    let first = win.iter().position(|v| v.abs() >= 0.5 * max).unwrap() + from;
    let direct = xcorr_peak(sig, tmpl, first as i64 - CHIRP_LEN as i64, first as i64 + 8);
    if !echo {
        return (direct, None);
    }
    let Some(d) = direct.as_ref() else { return (None, None) };
    // The direct chirp has rung out 64 frames + 3 ms after its start.
    let after = (d.pos as usize + CHIRP_LEN + 144).min(to);
    let tail = &sig[after..to];
    let Some((i, _)) = tail.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())) else {
        return (direct, None);
    };
    let at = (after + i) as i64;
    let echo_hit = xcorr_peak(sig, tmpl, at - CHIRP_LEN as i64, at + 8);
    (direct, echo_hit)
}

fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let mut s = xs.to_vec();
    s.sort_by(f64::total_cmp);
    let m = s.len() / 2;
    if s.len() % 2 == 1 { s[m] } else { (s[m - 1] + s[m]) / 2.0 }
}

fn spread(xs: &[f64]) -> f64 {
    let (lo, hi) = xs.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    if xs.is_empty() { f64::NAN } else { hi - lo }
}

/// Least-squares slope of `ys` over `xs`.
fn slope(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len() as f64;
    if xs.len() < 2 {
        return f64::NAN;
    }
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let num: f64 = xs.iter().zip(ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let den: f64 = xs.iter().map(|x| (x - mx) * (x - mx)).sum();
    if den > 0.0 { num / den } else { f64::NAN }
}

fn ms(frames: f64) -> f64 {
    frames * 1000.0 / rate() as f64
}

fn verdict(pass: bool, id: &str, value: String, bar: &str) {
    println!("[engine-spike] {} {id} {value} {bar}", if pass { "PASS" } else { "FAIL" });
}

// ── the run ─────────────────────────────────────────────────────────────────────────────────

pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let a = parse_args(args)?;
    RATE_HZ.store(device_rate(&a)?, Relaxed);
    let (unit, hold) = match a.plugin.as_deref() {
        Some(p) => {
            let (u, h) = load_plugin(p)?;
            (Some(u), Some(h))
        }
        None => (None, None),
    };
    let total_frames = ((a.seconds + 2.0) * rate() as f64) as usize;
    let chirp = chirp();
    let engine = Arc::new(Mutex::new(Engine {
        started: Instant::now(),
        chirp,
        emit: !a.quiet,
        period: a.backend.period(),
        in_ch: a.in_ch,
        out_ch: a.out_ch,
        echo: a.echo,
        handoff: vec![0.0; 16_384],
        handoff_len: 0,
        in_cycles: 0,
        in_thread: 0,
        out_cycles: 0,
        same_cycle: 0,
        not_same_cycle: 0,
        other_thread: 0,
        frame: 0,
        x: vec![0.0; 16_384],
        wet: vec![0.0; 16_384],
        y: vec![0.0; 16_384],
        rec_in: if a.backend.asio { vec![0.0; total_frames] } else { Vec::new() },
        gain_probe_max: 0.0,
        monitor_gain: 0.0,
        loud_blocks: 0,
        runaway: false,
        plugin: unit,
        plugin_latency: hold.as_ref().map_or(0, |h| h.activation.latency_frames),
        in_lat_ns: (0, 0),
        out_lat_ns: (0, 0),
        out_stats: Stats::new(),
        last_entry: None,
        period_us: 0,
        ring_rx: None,
        ring_started: false,
        ring_target: 0,
        starves: 0,
        out_marks: if a.backend.asio { Vec::new() } else { Vec::with_capacity(MARKS_CAP) },
        mmcss_out: false,
    }));
    let counters = Arc::new(Counters { lock_miss: AtomicU64::new(0), xruns: AtomicU64::new(0), errors: AtomicU64::new(0), runaway: AtomicBool::new(false) });
    let result = if a.backend.asio {
        run_asio(&a, &engine, &counters)
    } else {
        run_wasapi(&a, &engine, &counters, total_frames)
    };
    // Streams are dropped inside the run fns; the plugin comes back out of the engine last.
    let unit = engine.lock().map_err(|_| "engine lock poisoned")?.plugin.take();
    if let (Some(unit), Some(hold)) = (unit, hold) {
        let (name, lat) = (hold.name.clone(), hold.activation.latency_frames);
        unload_plugin(unit, hold);
        println!("[engine-spike] plugin unloaded: {name} (latency {lat} frames)");
    }
    result
}

/// The output device's current rate (ASIO: the driver's; WASAPI: the shared-mode mix rate). Read
/// before the plugin activates, since it runs at the same rate.
fn device_rate(a: &Args) -> Result<u32, String> {
    let host_id = if a.backend.asio {
        #[cfg(feature = "asio")]
        {
            cpal::HostId::Asio
        }
        #[cfg(not(feature = "asio"))]
        return Err("this build has no `asio` feature — rebuild with `--features asio`".into());
    } else {
        cpal::HostId::Wasapi
    };
    let host = cpal::host_from_id(host_id).map_err(|e| format!("host: {e}"))?;
    let device = if a.backend.asio {
        host.default_output_device().ok_or("no ASIO device")?
    } else {
        pick_wasapi(&host, false, a.device.as_deref())?
    };
    let rate = device.default_output_config().map_err(|e| format!("output config: {e}"))?.sample_rate();
    if rate == 0 {
        return Err("device reports a zero sample rate".into());
    }
    Ok(rate)
}

fn resolve_config(device: &cpal::Device, input: bool, block: Option<u32>) -> Result<(cpal::StreamConfig, cpal::SampleFormat), String> {
    let c = if input { device.default_input_config() } else { device.default_output_config() }
        .map_err(|e| format!("default {} config: {e}", if input { "input" } else { "output" }))?;
    let mut cfg = c.config();
    if cfg.sample_rate != rate() {
        return Err(format!("{} runs at {} Hz, not the device rate {} Hz", if input { "input" } else { "output" }, cfg.sample_rate, rate()));
    }
    if let Some(b) = block {
        cfg.buffer_size = cpal::BufferSize::Fixed(b);
    }
    Ok((cfg, c.sample_format()))
}

#[cfg(feature = "asio")]
fn run_asio(a: &Args, engine: &Shared, counters: &Arc<Counters>) -> Result<(), String> {
    let host = cpal::host_from_id(cpal::HostId::Asio).map_err(|e| format!("ASIO host: {e}"))?;
    if let (true, Some(block)) = (a.preopen, a.block) {
        preopen_asio(&host, if block == 256 { 128 } else { 256 })?;
    }
    let device = host.default_output_device().ok_or("no ASIO device")?;
    let name = device.description().map(|d| d.to_string()).unwrap_or_default();
    // Both configs while the driver is free: it can't be re-queried once a stream holds it.
    let (in_cfg, in_fmt) = resolve_config(&device, true, a.block)?;
    let (out_cfg, out_fmt) = resolve_config(&device, false, a.block)?;
    if a.in_ch >= in_cfg.channels as usize || a.out_ch >= out_cfg.channels as usize {
        return Err(format!("channel out of range: in {} of {}, out {} of {}", a.in_ch, in_cfg.channels, a.out_ch, out_cfg.channels));
    }
    let (in_chans, out_chans) = (in_cfg.channels as usize, out_cfg.channels as usize);
    // Input first: its callback registers first, so it runs first in every bufferSwitch. Neither plays
    // until both are built: the output build stops the driver under cpal's stream mutex, which a
    // playing input's callback takes (`engine_io::cpal_driver`'s `start`).
    let input = build_input!(device, in_cfg, in_fmt, counters, asio_input_cb(engine.clone(), counters.clone(), in_chans));
    let output = build_output!(device, out_cfg, out_fmt, counters, asio_output_cb(engine.clone(), counters.clone(), out_chans));
    let block = output.buffer_size().unwrap_or(0) as u64;
    {
        let mut e = engine.lock().map_err(|_| "engine lock poisoned")?;
        e.period_us = block * 1_000_000 / rate() as u64;
        e.started = Instant::now();
    }
    input.play().map_err(|e| format!("input play: {e}"))?;
    output.play().map_err(|e| format!("output play: {e}"))?;
    println!("[engine-spike] started asio device=\"{name}\" block={block} in={in_fmt:?}x{in_chans} out={out_fmt:?}x{out_chans} seconds={}", a.seconds);
    let aborted = wait_run(a.seconds, counters);
    drop(output);
    drop(input);
    let e = engine.lock().map_err(|_| "engine lock poisoned")?;
    report_asio(a, &e, counters, &name, block, aborted)
}

/// Open the driver at `block`, run it briefly silent, and drop the device so the driver is destroyed
/// (stop, dispose, exit) before the measured open.
#[cfg(feature = "asio")]
fn preopen_asio(host: &cpal::Host, block: u32) -> Result<(), String> {
    let device = host.default_output_device().ok_or("no ASIO device (preopen)")?;
    let (in_cfg, in_fmt) = resolve_config(&device, true, Some(block))?;
    let (out_cfg, out_fmt) = resolve_config(&device, false, Some(block))?;
    let input = device
        .build_input_stream_raw(in_cfg, in_fmt, |_: &cpal::Data, _: &cpal::InputCallbackInfo| {}, |_| {}, None)
        .map_err(|e| format!("preopen input: {e}"))?;
    let output = device
        .build_output_stream_raw(
            out_cfg,
            out_fmt,
            |data: &mut cpal::Data, _: &cpal::OutputCallbackInfo| data.bytes_mut().fill(0),
            |_| {},
            None,
        )
        .map_err(|e| format!("preopen output: {e}"))?;
    input.play().map_err(|e| format!("preopen input play: {e}"))?;
    output.play().map_err(|e| format!("preopen output play: {e}"))?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    drop(output);
    drop(input);
    drop(device);
    println!("[engine-spike] preopen at block {block} done");
    Ok(())
}

#[cfg(not(feature = "asio"))]
fn run_asio(_: &Args, _: &Shared, _: &Arc<Counters>) -> Result<(), String> {
    Err("this build has no `asio` feature — rebuild with `--features asio`".into())
}

/// Sleep out the run, polling for an abort (stream error or runaway echo). Returns the reason.
fn wait_run(seconds: f64, counters: &Counters) -> Option<String> {
    let end = Instant::now() + Duration::from_secs_f64(seconds + emit_start() as f64 / rate() as f64);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(200));
        if counters.errors.load(SeqCst) > 0 {
            return Some("stream error".into());
        }
        if counters.runaway.load(Relaxed) {
            return Some("runaway echo: monitor muted".into());
        }
    }
    None
}

struct Arrivals {
    lags: Vec<f64>,
    times: Vec<f64>,
    echoes: Vec<f64>,
    invalid: usize,
    emitted: usize,
}

fn collect(sig: &[f32], chirp: &[f32], period: u64, window: u64, end_frame: u64, echo: bool, map: impl Fn(u64) -> Option<(usize, f64)>) -> Arrivals {
    let mut r = Arrivals { lags: vec![], times: vec![], echoes: vec![], invalid: 0, emitted: 0 };
    let mut e = emit_start();
    while e + window < end_frame {
        r.emitted += 1;
        if let Some((from, base)) = map(e) {
            let (d, ec) = find_arrivals(sig, chirp, from, from + window as usize, echo);
            match d {
                Some(h) if h.ncc >= 0.8 => {
                    r.lags.push(h.pos - base);
                    r.times.push(e as f64 / rate() as f64);
                    if let Some(x) = ec.filter(|x| x.ncc >= 0.8) {
                        if e >= emit_start() + gain_probe_frames() {
                            r.echoes.push(x.pos - h.pos);
                        }
                    }
                }
                _ => r.invalid += 1,
            }
        } else {
            r.invalid += 1;
        }
        e += period;
    }
    r
}

fn timing_json(s: &Stats, period_frames: u64) -> serde_json::Value {
    let period_us = period_frames as f64 * 1e6 / rate() as f64;
    serde_json::json!({
        "callbacks": s.callbacks, "settledCallbacks": s.settled_callbacks,
        "gaps": s.gaps, "maxGapUs": s.max_gap_us,
        "blockUsP50": s.percentile_us(0.5), "blockUsP999": s.percentile_us(0.999), "blockUsMax": s.max_us(),
        "blockP999Pct": s.percentile_us(0.999) as f64 / period_us * 100.0,
        "blockMaxPct": s.max_us() as f64 / period_us * 100.0,
        "framesMin": s.block_frames_min, "framesMax": s.block_frames_max,
    })
}

fn report_c1(a: &Args, s: &Stats, errors: u64, allocs: u64, period_frames: u64) {
    if a.plugin.is_none() || a.echo {
        return;
    }
    let period_us = period_frames as f64 * 1e6 / rate() as f64;
    let (p999, max) = (s.percentile_us(0.999) as f64, s.max_us() as f64);
    let ok = s.gaps == 0 && errors == 0 && allocs == 0 && p999 <= 0.5 * period_us && max < 0.9 * period_us;
    verdict(
        ok && a.seconds >= 120.0,
        "C1",
        format!("gaps={} xruns={errors} allocs={allocs} p99.9={:.0}% max={:.0}% run={}s", s.gaps, p999 / period_us * 100.0, max / period_us * 100.0, a.seconds),
        "0 gaps, 0 xruns, 0 allocs, p99.9<=50% max<90% of the period, run>=120s",
    );
}

#[cfg(feature = "asio")]
fn report_asio(a: &Args, e: &Engine, counters: &Counters, name: &str, block: u64, aborted: Option<String>) -> Result<(), String> {
    let end = e.frame.min(e.rec_in.len() as u64);
    let arr = collect(&e.rec_in, &e.chirp, e.period, a.backend.window(), end, a.echo, |f| Some((f as usize, f as f64)));
    let in_lat = e.in_lat_ns.1 as f64 * rate() as f64 / 1e9;
    let out_lat = e.out_lat_ns.1 as f64 * rate() as f64 / 1e9;
    let expected = in_lat + out_lat;
    let lag = median(&arr.lags);
    let spread_f = spread(&arr.lags);
    let drift_per_10min = slope(&arr.times, &arr.lags) * 600.0;
    let rt = median(&arr.echoes);
    let allocs = crate::host::rt_alloc::RT_ALLOCS.load(Relaxed);
    let (errors, xruns) = (counters.errors.load(SeqCst), counters.xruns.load(SeqCst));
    println!("[engine-spike] {}", serde_json::json!({
        "phase": if a.echo { "asio-echo" } else { "asio" }, "device": name, "block": block, "rate": rate(),
        "seconds": a.seconds, "in": a.in_ch, "out": a.out_ch, "plugin": a.plugin,
        "pluginLatencyFrames": e.plugin_latency,
        "inputPeak": e.rec_in[..end as usize].iter().fold(0.0f32, |m, v| m.max(v.abs())),
        "aborted": aborted,
        "inLatFrames": [e.in_lat_ns.0 as f64 * rate() as f64 / 1e9, in_lat],
        "outLatFrames": [e.out_lat_ns.0 as f64 * rate() as f64 / 1e9, out_lat],
        "sameCycle": e.same_cycle, "notSameCycle": e.not_same_cycle, "otherThread": e.other_thread,
        "lockMiss": counters.lock_miss.load(Relaxed), "streamErrors": errors,
        "xruns": xruns, "rtAllocs": allocs,
        "emitted": arr.emitted, "found": arr.lags.len(), "invalid": arr.invalid,
        "lagMedianFrames": lag, "lagMedianMs": ms(lag), "lagSpreadFrames": spread_f,
        "expectedFrames": expected, "residualMs": ms(lag - expected),
        "driftFramesPer10Min": drift_per_10min,
        "monitorGain": e.monitor_gain, "cableGain": e.gain_probe_max / CHIRP_AMP, "runaway": e.runaway,
        "echoes": arr.echoes.len(), "rtMedianFrames": rt, "rtMs": ms(rt), "rtSpreadFrames": spread(&arr.echoes),
        "pluginProcessErrors": e.plugin.as_ref().map(|p| p.process_errors),
        "timing": timing_json(&e.out_stats, block),
    }));
    // C1 needs no cable: judged before the chirp checks.
    report_c1(a, &e.out_stats, errors + xruns, allocs, block);
    if a.quiet {
        return Ok(());
    }
    if arr.lags.is_empty() || arr.invalid * 10 > arr.emitted {
        println!("[engine-spike] INVALID {} of {} chirps below xcorr 0.8 — check the cable and --in/--out", arr.invalid, arr.emitted);
        return Err("invalid run".into());
    }
    verdict(e.not_same_cycle == 0 && e.other_thread == 0 && e.same_cycle > 0, "A1",
        format!("same={} miss={} otherThread={}", e.same_cycle, e.not_same_cycle, e.other_thread), "sameCycle on every cycle");
    if !a.echo {
        verdict((ms(lag - expected)).abs() <= 1.0, "A2",
            format!("{:+.3}ms (lag {lag:.2}f, inLat+outLat {expected:.0}f)", ms(lag - expected)), "|median lag - (inLat+outLat)| <= 1.0 ms, dry");
        verdict(spread_f <= 1.0, "A3.spread", format!("{spread_f:.2}f"), "spread <= 1 frame per run (across launches: the runner)");
        if a.seconds >= 600.0 {
            verdict(drift_per_10min.abs() <= 1.0, "A4", format!("{drift_per_10min:+.3}f/10min"), "drift <= 1 frame over 10 min");
        }
    } else if arr.echoes.is_empty() {
        println!("[engine-spike] INVALID no echo found — monitor gain {:.3}, cable gain {:.3}", e.monitor_gain, e.gain_probe_max / CHIRP_AMP);
        return Err("invalid run".into());
    } else {
        let pl = e.plugin_latency as f64;
        verdict((rt - (lag + pl)).abs() <= 1.0, "R1",
            format!("RT {rt:.2}f vs lag {lag:.2}f + plugin {pl:.0}f"), "|RT - (lag + plugin latency)| <= 1 frame");
        verdict(ms(rt) <= 22.2, "R2", format!("{:.2}ms at {block}", ms(rt)), "RT <= 22.2 ms at 256 (other blocks: informative)");
    }
    Ok(())
}

fn pick_wasapi(host: &cpal::Host, input: bool, want: Option<&str>) -> Result<cpal::Device, String> {
    let default = if input { host.default_input_device() } else { host.default_output_device() };
    let Some(want) = want else {
        return default.ok_or_else(|| "no default device".to_string());
    };
    let devices = if input { host.input_devices() } else { host.output_devices() }.map_err(|e| e.to_string())?;
    for d in devices {
        let name = d.description().map(|x| x.to_string()).unwrap_or_default();
        if name.to_lowercase().contains(&want.to_lowercase()) {
            return Ok(d);
        }
    }
    Err(format!("no WASAPI {} device matching \"{want}\"", if input { "input" } else { "output" }))
}

fn run_wasapi(a: &Args, engine: &Shared, counters: &Arc<Counters>, total_frames: usize) -> Result<(), String> {
    let host = cpal::host_from_id(cpal::HostId::Wasapi).map_err(|e| format!("WASAPI host: {e}"))?;
    let in_dev = pick_wasapi(&host, true, a.device.as_deref())?;
    let out_dev = pick_wasapi(&host, false, a.device.as_deref())?;
    let in_name = in_dev.description().map(|d| d.to_string()).unwrap_or_default();
    let out_name = out_dev.description().map(|d| d.to_string()).unwrap_or_default();
    let (in_cfg, in_fmt) = resolve_config(&in_dev, true, a.block)?;
    let (out_cfg, out_fmt) = resolve_config(&out_dev, false, a.block)?;
    let (in_chans, out_chans) = (in_cfg.channels as usize, out_cfg.channels as usize);
    if a.in_ch >= in_chans || a.out_ch >= out_chans {
        return Err(format!("channel out of range: in {} of {in_chans}, out {} of {out_chans}", a.in_ch, a.out_ch));
    }
    let (tx, rx) = rtrb::RingBuffer::<f32>::new(rate() as usize);
    let started = engine.lock().map_err(|_| "engine lock poisoned")?.started;
    let side = Arc::new(Mutex::new(InSide {
        frame: 0,
        rec: vec![0.0; total_frames],
        marks: Vec::with_capacity(MARKS_CAP),
        ring_tx: tx,
        overflows: 0,
        in_lat_ns: (0, 0),
        stats: Stats::new(),
        last_entry: None,
        mmcss: false,
        frames_min: u64::MAX,
        frames_max: 0,
    }));
    {
        let mut e = engine.lock().map_err(|_| "engine lock poisoned")?;
        e.ring_rx = Some(rx);
    }
    let input = build_input!(in_dev, in_cfg, in_fmt, counters, wasapi_input_cb(side.clone(), counters.clone(), in_chans, a.in_ch, started));
    input.play().map_err(|e| format!("input play: {e}"))?;
    // Let the input settle and learn its packet size before the output starts popping.
    std::thread::sleep(Duration::from_millis(300));
    let in_packet = side.lock().map(|s| s.frames_max).unwrap_or(0).max(1);
    let output = build_output!(out_dev, out_cfg, out_fmt, counters, wasapi_output_cb(engine.clone(), counters.clone(), out_chans));
    let out_block = output.buffer_size().unwrap_or(0) as u64;
    {
        let mut e = engine.lock().map_err(|_| "engine lock poisoned")?;
        // Start popping once one input packet plus one output period is queued: the least that
        // rides out the two threads' phase without starving.
        e.ring_target = (in_packet + out_block.max(1)) as usize;
        e.period_us = out_block * 1_000_000 / rate() as u64;
    }
    output.play().map_err(|e| format!("output play: {e}"))?;
    println!("[engine-spike] started wasapi in=\"{in_name}\" out=\"{out_name}\" inPacket={in_packet} outBlock={out_block} in={in_fmt:?}x{in_chans} out={out_fmt:?}x{out_chans} seconds={}", a.seconds);
    let aborted = wait_run(a.seconds, counters);
    drop(output);
    drop(input);
    let e = engine.lock().map_err(|_| "engine lock poisoned")?;
    let s = side.lock().map_err(|_| "input lock poisoned")?;
    report_wasapi(a, &e, &s, counters, &in_name, &out_name, out_block, aborted)
}

/// The time (ns) of frame `f` on a stamp track: the last mark at or before it, plus frames.
fn stamp_at(marks: &[Mark], f: u64) -> Option<u128> {
    let i = marks.partition_point(|m| m.frame <= f).checked_sub(1)?;
    let m = marks[i];
    Some(m.stamp_ns + ((f - m.frame) as u128 * 1_000_000_000 / rate() as u128))
}

/// The (fractional) frame at time `t` on a stamp track.
fn frame_at(marks: &[Mark], t: u128) -> Option<f64> {
    let i = marks.partition_point(|m| m.stamp_ns <= t).checked_sub(1)?;
    let m = marks[i];
    Some(m.frame as f64 + (t - m.stamp_ns) as f64 * rate() as f64 / 1e9)
}

#[allow(clippy::too_many_arguments)]
fn report_wasapi(a: &Args, e: &Engine, s: &InSide, counters: &Counters, in_name: &str, out_name: &str, out_block: u64, aborted: Option<String>) -> Result<(), String> {
    // Residual = capture time of the arrival − playback time of the emitted frame, both from cpal's
    // QPC stamps. Found in the input's own frame domain around the frame captured at playback.
    let window = a.backend.window();
    let arr = collect(&s.rec, &e.chirp, e.period, window, e.frame, a.echo, |f| {
        let t_play = stamp_at(&e.out_marks, f)?;
        let at = frame_at(&s.marks, t_play)?;
        let from = (at - 0.05 * rate() as f64).max(0.0);
        // The lag is measured in input frames from `at` (the frame captured at playback time).
        Some((from as usize, at))
    });
    let residual = median(&arr.lags);
    let res_spread = spread(&arr.lags);
    let rt = median(&arr.echoes);
    let med_ns = |xs: Vec<u128>| median(&xs.into_iter().map(|v| v as f64).collect::<Vec<_>>()) / 1e6;
    let out_lat_ms = med_ns(e.out_marks.iter().map(|m| m.stamp_ns.saturating_sub(m.callback_ns)).collect());
    let in_age_ms = med_ns(s.marks.iter().map(|m| m.callback_ns.saturating_sub(m.stamp_ns)).collect());
    let ring_ms = median(&e.out_marks.iter().skip(100).map(|m| m.ring_fill as f64).collect::<Vec<_>>()) * 1000.0 / rate() as f64;
    let parts = in_age_ms + ring_ms + out_lat_ms;
    let allocs = crate::host::rt_alloc::RT_ALLOCS.load(Relaxed);
    let (errors, xruns) = (counters.errors.load(SeqCst), counters.xruns.load(SeqCst));
    println!("[engine-spike] {}", serde_json::json!({
        "phase": if a.echo { "wasapi-echo" } else { "wasapi" }, "in": in_name, "out": out_name, "rate": rate(),
        "seconds": a.seconds, "inCh": a.in_ch, "outCh": a.out_ch, "plugin": a.plugin, "aborted": aborted,
        "inPacketFrames": [s.frames_min, s.frames_max], "outBlock": out_block,
        "lockMiss": counters.lock_miss.load(Relaxed), "streamErrors": errors,
        "xruns": xruns, "rtAllocs": allocs,
        "starves": e.starves, "overflows": s.overflows, "ringTarget": e.ring_target,
        "emitted": arr.emitted, "found": arr.lags.len(), "invalid": arr.invalid,
        "residualMedianMs": ms(residual), "residualSpreadMs": ms(res_spread),
        "residualDriftMsPer10Min": ms(slope(&arr.times, &arr.lags) * 600.0),
        "monitorGain": e.monitor_gain, "cableGain": e.gain_probe_max / CHIRP_AMP, "runaway": e.runaway,
        "echoes": arr.echoes.len(), "rtMs": ms(rt), "rtSpreadMs": ms(spread(&arr.echoes)),
        "rtParts": { "inputAgeMs": in_age_ms, "ringMs": ring_ms, "outputLatencyMs": out_lat_ms, "sumMs": parts,
            "devicePeriodMs": { "in": s.frames_max as f64 * 1000.0 / rate() as f64, "out": out_block as f64 * 1000.0 / rate() as f64 } },
        "timingOut": timing_json(&e.out_stats, out_block),
        "timingIn": timing_json(&s.stats, s.frames_max),
    }));
    report_c1(a, &e.out_stats, errors + xruns, allocs, out_block);
    if a.quiet {
        return Ok(());
    }
    if arr.lags.is_empty() || arr.invalid * 10 > arr.emitted {
        println!("[engine-spike] INVALID {} of {} chirps below xcorr 0.8 — check the cable, --device, --in/--out", arr.invalid, arr.emitted);
        return Err("invalid run".into());
    }
    if !a.echo {
        verdict(ms(residual).abs() <= 2.0 && ms(res_spread) <= 1.0, "W1",
            format!("median {:+.3}ms spread {:.3}ms", ms(residual), ms(res_spread)), "|median residual| <= 2.0 ms, spread <= 1.0 ms");
    } else if !arr.echoes.is_empty() {
        let remainder = ms(rt) - parts;
        let explained = if remainder.abs() > 5.0 { format!("unknown {remainder:+.1}ms") } else { format!("{remainder:+.1}ms") };
        println!("[engine-spike] INFO W2 RT={:.1}ms = inputAge {in_age_ms:.1} + ring {ring_ms:.1} + outputLatency {out_lat_ms:.1} (sum {parts:.1}), remainder {explained}", ms(rt));
    }
    Ok(())
}
