//! Stage 1 silent-share probe (`docs/plans/native-engine.md` § Stage 1, criterion S1).
//!
//! `app.exe --probe-share <open|mute|vol0|zeros|dual|all>` (DEV, debug build, exits before Tauri).
//! The parent spawns a child `app.exe --probe-share-child <mode>` that renders a tone on the
//! default render endpoint through raw WASAPI shared mode, and captures it at the same time through
//! PROCESS loopback (the child's process tree, what an app capture of BleepLoop would get) and plain
//! ENDPOINT loopback on the default render endpoint (what the room hears and what a browser
//! "share system audio" sees).
//!
//! Signal: a 10 ms Hann-gated 997 Hz burst every 100 ms (4800 frames at 48 kHz), 3 s long. A steady
//! tone cannot show a copy 20 ms late (the sum is the same sine at another phase); the gated burst's
//! matched-filter response lasts ±10 ms, so a copy 20 ms later (`dual`) lands as a separate peak.
//! Levels come from a synchronous average: the capture between 0.3 s and 2.7 s after "go" is folded
//! at the 100 ms period, then correlated with the reference burst (complex, so phase does not
//! matter). Unrelated system audio averages down; the burst does not. Level convention: dBFS where a
//! full-scale sine reads 0 (tone = burst amplitude; floor = broadband RMS × √2 outside the burst
//! windows). −200 means digital zero.
//!
//! Every child stream is opened with AUDCLNT_STREAMFLAGS_NOPERSIST and its own session GUID, so the
//! probe's mute/volume never touches app.exe's default session or persists into the real app.
#![cfg(all(windows, debug_assertions))]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use windows::core::{implement, Interface, Ref, GUID, HRESULT, IUnknown, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_NOPERSIST,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

const TAG: &str = "[share-probe]";
const RATE: u32 = 48_000;
const PERIOD: usize = 4_800; // 100 ms burst period
const BURST: usize = 480; // 10 ms Hann-gated burst
const FREQ: f64 = 997.0;
const RENDER_FRAMES: usize = 3 * RATE as usize; // 3 s per phase (keeps the audible phases short)
const COPY_DELAY: i64 = 960; // `dual`: the muted copy runs 20 ms behind the audible stream
const WINDOW_START: Duration = Duration::from_millis(300);
const WINDOW_END: Duration = Duration::from_millis(2_700);
const MAX_PERIODS: usize = 24;
const MIN_PERIODS: usize = 5;
const MAIN_GUARD: usize = 576; // ±12 ms around the main peak is its own response, not a second peak
const FLOOR_MARGIN: usize = 240; // 5 ms either side of a burst is excluded from the floor
const DB_ZERO: f64 = -200.0;
const HNS_100MS: i64 = 1_000_000;
const HNS_200MS: i64 = 2_000_000;
const SESSION_MAIN: GUID = GUID::from_u128(0x5e3b_0d1c_7a42_4c8e_9b1f_2d6e_8a90_0001);
const SESSION_COPY: GUID = GUID::from_u128(0x5e3b_0d1c_7a42_4c8e_9b1f_2d6e_8a90_0002);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Open,
    Mute,
    Vol0,
    Zeros,
    Dual,
}

impl Mode {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => Self::Open,
            "mute" => Self::Mute,
            "vol0" => Self::Vol0,
            "zeros" => Self::Zeros,
            "dual" => Self::Dual,
            _ => return None,
        })
    }
    fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Mute => "mute",
            Self::Vol0 => "vol0",
            Self::Zeros => "zeros",
            Self::Dual => "dual",
        }
    }
    /// Rendered burst amplitude in dBFS; `None` renders digital silence. Audible phases stay at
    /// −30 dBFS; the silenced ones render louder so a leak into the room would show.
    fn render_dbfs(self) -> Option<f64> {
        match self {
            Self::Open | Self::Dual => Some(-30.0),
            Self::Mute | Self::Vol0 => Some(-12.0),
            Self::Zeros => None,
        }
    }
}

const USAGE: &str = "usage: --probe-share <open|mute|vol0|zeros|dual|all>";

/// Entry from `lib.rs`: `child` selects the hidden `--probe-share-child <mode>` role.
pub(crate) fn run(child: bool, args: &[String]) -> Result<(), String> {
    let arg = args.first().map(String::as_str).unwrap_or_default();
    if child {
        return child_main(Mode::parse(arg).ok_or(USAGE)?);
    }
    let modes = if arg == "all" {
        vec![Mode::Open, Mode::Mute, Mode::Vol0, Mode::Zeros, Mode::Dual]
    } else {
        vec![Mode::parse(arg).ok_or(USAGE)?]
    };
    let mut results = Vec::new();
    for mode in modes {
        let phase = run_phase(mode).map_err(|e| format!("phase {}: {e}", mode.name()))?;
        println!("{TAG} {}", phase.to_json());
        results.push(phase);
    }
    verdict(&results);
    Ok(())
}

// ── COM helpers ─────────────────────────────────────────────────────────────────────────────────

/// MTA on the calling thread; `true` if this call owns a CoUninitialize.
fn com_init() -> Result<bool, String> {
    // SAFETY: plain COM init on the calling thread.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() && hr != RPC_E_CHANGED_MODE {
        return Err(format!("CoInitializeEx: {hr:?}"));
    }
    Ok(hr.is_ok())
}

fn com_uninit(owned: bool) {
    if owned {
        // SAFETY: balances a successful CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
}

/// 48 kHz stereo float32. Process loopback has no mix format (GetMixFormat is unsupported on it),
/// so every client here requests this format and lets AUTOCONVERTPCM adapt to the engine.
fn format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: 2,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

fn default_render_device() -> Result<IMMDevice, String> {
    // SAFETY: COM is initialised on this thread by the caller.
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| format!("CoCreateInstance(MMDeviceEnumerator): {e}"))?;
        enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| format!("GetDefaultAudioEndpoint: {e}"))
    }
}

fn db(amplitude: f64) -> f64 {
    if amplitude > 1e-10 {
        20.0 * amplitude.log10()
    } else {
        DB_ZERO
    }
}

/// Unit-amplitude burst: Hann(10 ms) × sin(997 Hz), phase 0 at the burst start.
fn burst_table() -> Vec<f64> {
    (0..BURST)
        .map(|n| {
            let w = 0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / BURST as f64).cos();
            w * (std::f64::consts::TAU * FREQ * n as f64 / RATE as f64).sin()
        })
        .collect()
}

// ── child: the renderer ─────────────────────────────────────────────────────────────────────────

struct RenderStream {
    client: IAudioClient,
    render: IAudioRenderClient,
    buffer_frames: u32,
    amplitude: f32,
    delay: i64,
    written: usize,
}

impl RenderStream {
    /// Fill every free frame (up to the phase length) with the burst train.
    fn fill(&mut self, table: &[f64]) -> Result<(), String> {
        // SAFETY: GetBuffer returns `avail` writable stereo f32 frames until ReleaseBuffer.
        unsafe {
            let padding = self.client.GetCurrentPadding().map_err(|e| format!("GetCurrentPadding: {e}"))?;
            let avail = (self.buffer_frames - padding).min((RENDER_FRAMES - self.written) as u32);
            if avail == 0 {
                return Ok(());
            }
            let ptr = self.render.GetBuffer(avail).map_err(|e| format!("GetBuffer: {e}"))? as *mut f32;
            let out = std::slice::from_raw_parts_mut(ptr, avail as usize * 2);
            for (i, frame) in out.chunks_exact_mut(2).enumerate() {
                let n = (self.written + i) as i64 - self.delay;
                let p = n.rem_euclid(PERIOD as i64) as usize;
                let s = if n >= 0 && p < BURST { self.amplitude * table[p] as f32 } else { 0.0 };
                frame[0] = s;
                frame[1] = s;
            }
            self.render.ReleaseBuffer(avail, 0).map_err(|e| format!("ReleaseBuffer: {e}"))?;
            self.written += avail as usize;
        }
        Ok(())
    }
}

enum Session {
    Unmuted,
    Muted,
    VolumeZero,
}

fn open_render(
    device: &IMMDevice,
    session: Session,
    guid: GUID,
    amplitude: f32,
    delay: i64,
) -> Result<(RenderStream, Value), String> {
    let fmt = format();
    // SAFETY: COM objects are owned; `fmt` and `guid` outlive Initialize.
    unsafe {
        let client: IAudioClient =
            device.Activate(CLSCTX_ALL, None).map_err(|e| format!("Activate(IAudioClient): {e}"))?;
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
            | AUDCLNT_STREAMFLAGS_NOPERSIST;
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, HNS_100MS, 0, &fmt, Some(&guid))
            .map_err(|e| format!("render Initialize: {e}"))?;
        let buffer_frames = client.GetBufferSize().map_err(|e| format!("GetBufferSize: {e}"))?;
        let render: IAudioRenderClient =
            client.GetService().map_err(|e| format!("GetService(IAudioRenderClient): {e}"))?;
        let volume: ISimpleAudioVolume =
            client.GetService().map_err(|e| format!("GetService(ISimpleAudioVolume): {e}"))?;
        match session {
            Session::Unmuted => {}
            Session::Muted => volume.SetMute(true, std::ptr::null()).map_err(|e| format!("SetMute: {e}"))?,
            Session::VolumeZero => volume
                .SetMasterVolume(0.0, std::ptr::null())
                .map_err(|e| format!("SetMasterVolume: {e}"))?,
        }
        let muted = volume.GetMute().map(|b| json!(b.as_bool())).unwrap_or(json!("unknown"));
        let level = volume.GetMasterVolume().map(|v| json!(v)).unwrap_or(json!("unknown"));
        let info = json!({
            "session": format!("{guid:?}"), "muted": muted, "sessionVolume": level,
            "renderDbfs": db(amplitude as f64), "delayFrames": delay, "bufferFrames": buffer_frames,
        });
        Ok((RenderStream { client, render, buffer_frames, amplitude, delay, written: 0 }, info))
    }
}

fn child_main(mode: Mode) -> Result<(), String> {
    let com = com_init()?;
    let result = child_render(mode);
    com_uninit(com);
    result
}

fn child_render(mode: Mode) -> Result<(), String> {
    let device = default_render_device()?;
    let amplitude = mode.render_dbfs().map_or(0.0, |d| 10f64.powf(d / 20.0) as f32);
    let plan: Vec<(Session, GUID, i64)> = match mode {
        Mode::Open | Mode::Zeros => vec![(Session::Unmuted, SESSION_MAIN, 0)],
        Mode::Mute => vec![(Session::Muted, SESSION_MAIN, 0)],
        Mode::Vol0 => vec![(Session::VolumeZero, SESSION_MAIN, 0)],
        Mode::Dual => vec![(Session::Unmuted, SESSION_MAIN, 0), (Session::Muted, SESSION_COPY, COPY_DELAY)],
    };
    let mut streams = Vec::new();
    let mut infos = Vec::new();
    for (session, guid, delay) in plan {
        let (stream, info) = open_render(&device, session, guid, amplitude, delay)?;
        streams.push(stream);
        infos.push(info);
    }
    println!("ready {}", json!({ "pid": std::process::id(), "streams": infos }));
    let _ = std::io::stdout().flush();

    // Wait for the parent's "go" (its captures are running by then); EOF or silence aborts.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        let _ = tx.send(line);
    });
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(line) if line.trim() == "go" => {}
        Ok(line) => return Err(format!("expected 'go' on stdin, got {:?}", line.trim())),
        Err(_) => return Err("no 'go' from the parent within 20 s".into()),
    }

    let table = burst_table();
    for s in &mut streams {
        s.fill(&table)?;
    }
    // SAFETY: Initialize succeeded on every client.
    for s in &streams {
        unsafe { s.client.Start() }.map_err(|e| format!("Start: {e}"))?;
    }
    let started = Instant::now();
    let deadline = started + Duration::from_secs(8);
    while streams.iter().any(|s| s.written < RENDER_FRAMES) {
        if Instant::now() > deadline {
            return Err("render did not finish within 8 s".into());
        }
        thread::sleep(Duration::from_millis(5));
        for s in &mut streams {
            s.fill(&table)?;
        }
    }
    // Let the queued frames play out before stopping.
    while streams.iter().any(|s| unsafe { s.client.GetCurrentPadding() }.unwrap_or(0) > 0) {
        if Instant::now() > deadline {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    thread::sleep(Duration::from_millis(50));
    for s in &streams {
        // SAFETY: stopping a started client.
        let _ = unsafe { s.client.Stop() };
    }
    let written: Vec<usize> = streams.iter().map(|s| s.written).collect();
    println!("done {}", json!({ "framesWritten": written, "renderMs": started.elapsed().as_millis() as u64 }));
    let _ = std::io::stdout().flush();
    Ok(())
}

// ── parent: the captures ────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum Target {
    Process(u32),
    Endpoint,
}

struct Packet {
    fetched: Instant,
    device_position: u64,
    flags: u32,
    mono: Vec<f32>,
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activation(mpsc::Sender<windows::core::Result<IUnknown>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activation_Impl {
    fn ActivateCompleted(&self, operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let result = operation.ok().and_then(|op| {
            let mut hr = HRESULT::default();
            let mut interface: Option<IUnknown> = None;
            // SAFETY: both out-pointers are valid locals.
            unsafe { op.GetActivateResult(&mut hr, &mut interface)? };
            hr.ok()?;
            interface.ok_or_else(|| windows::core::Error::new(E_FAIL, "activation returned no interface"))
        });
        let _ = self.0.send(result);
        Ok(())
    }
}

fn activate_process_loopback(pid: u32) -> Result<IAudioClient, String> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // ManuallyDrop: the windows crate's PROPVARIANT Drop runs PropVariantClear, which would free
    // the borrowed stack blob (heap corruption, 0xC0000374, seen on the first run).
    let mut prop = std::mem::ManuallyDrop::new(PROPVARIANT::default());
    // SAFETY: a VT_BLOB PROPVARIANT pointing at `params`, which outlives the activation wait below.
    unsafe {
        let inner = &mut *prop.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &mut params as *mut _ as *mut u8,
        };
    }
    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activation(tx).into();
    // SAFETY: all arguments are valid for the call; `_operation` and `handler` stay alive until the
    // completion arrives or the wait times out.
    let _operation = unsafe {
        ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*prop), &handler)
    }
    .map_err(|e| format!("ActivateAudioInterfaceAsync(process loopback): {e}"))?;
    let unknown = rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "process-loopback activation timed out after 5 s".to_string())?
        .map_err(|e| format!("process-loopback activation failed: {e}"))?;
    unknown.cast().map_err(|e| format!("process loopback: cast to IAudioClient: {e}"))
}

struct Capture {
    packets: Vec<Packet>,
    event_driven: bool,
}

fn capture_thread(
    target: Target,
    stop: Arc<AtomicBool>,
    started: mpsc::Sender<Result<(), String>>,
) -> Result<Capture, String> {
    let com = com_init();
    let com = match com {
        Ok(c) => c,
        Err(e) => {
            let _ = started.send(Err(e.clone()));
            return Err(e);
        }
    };
    let result = capture_body(target, &stop, &started);
    if let Err(e) = &result {
        let _ = started.send(Err(e.clone())); // no-op if Ok(()) was already sent and read
    }
    com_uninit(com);
    result
}

fn capture_body(target: Target, stop: &AtomicBool, started: &mpsc::Sender<Result<(), String>>) -> Result<Capture, String> {
    let fmt = format();
    let (client, event_driven) = match target {
        Target::Process(pid) => (activate_process_loopback(pid)?, true),
        Target::Endpoint => {
            let device = default_render_device()?;
            // SAFETY: COM initialised on this thread.
            let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
                .map_err(|e| format!("endpoint Activate(IAudioClient): {e}"))?;
            (client, false)
        }
    };
    let mut flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
    if event_driven {
        flags |= AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    } else {
        flags |= AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    }
    // SAFETY: `fmt` outlives Initialize; the event handle is closed after Stop.
    let event: Option<HANDLE> = unsafe {
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, HNS_200MS, 0, &fmt, None)
            .map_err(|e| format!("{target:?} loopback Initialize: {e}"))?;
        if event_driven {
            let h = CreateEventW(None, false, false, PCWSTR::null()).map_err(|e| format!("CreateEventW: {e}"))?;
            client.SetEventHandle(h).map_err(|e| format!("SetEventHandle: {e}"))?;
            Some(h)
        } else {
            None
        }
    };
    let result = (|| {
        // SAFETY: Initialize succeeded.
        let capture: IAudioCaptureClient = unsafe { client.GetService() }
            .map_err(|e| format!("GetService(IAudioCaptureClient): {e}"))?;
        unsafe { client.Start() }.map_err(|e| format!("{target:?} loopback Start: {e}"))?;
        let _ = started.send(Ok(()));
        let mut packets = Vec::new();
        let hard_stop = Instant::now() + Duration::from_secs(30);
        while !stop.load(Ordering::Relaxed) && Instant::now() < hard_stop {
            match event {
                // SAFETY: a live event handle; the 10 ms timeout doubles as the poll interval.
                Some(h) => unsafe {
                    WaitForSingleObject(h, 10);
                },
                None => thread::sleep(Duration::from_millis(10)),
            }
            drain(&capture, &mut packets)?;
        }
        // SAFETY: stopping a started client.
        let _ = unsafe { client.Stop() };
        Ok(Capture { packets, event_driven })
    })();
    if let Some(h) = event {
        // SAFETY: our own event handle; the client is stopped.
        let _ = unsafe { CloseHandle(h) };
    }
    result
}

fn drain(capture: &IAudioCaptureClient, packets: &mut Vec<Packet>) -> Result<(), String> {
    // SAFETY: GetBuffer yields `frames` readable stereo f32 frames until ReleaseBuffer.
    unsafe {
        loop {
            let size = capture.GetNextPacketSize().map_err(|e| format!("GetNextPacketSize: {e}"))?;
            if size == 0 {
                return Ok(());
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut device_position = 0u64;
            capture
                .GetBuffer(&mut data, &mut frames, &mut flags, Some(&mut device_position), None)
                .map_err(|e| format!("capture GetBuffer: {e}"))?;
            let mono = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                vec![0.0; frames as usize]
            } else {
                std::slice::from_raw_parts(data as *const f32, frames as usize * 2)
                    .chunks_exact(2)
                    .map(|f| 0.5 * (f[0] + f[1]))
                    .collect()
            };
            capture.ReleaseBuffer(frames).map_err(|e| format!("capture ReleaseBuffer: {e}"))?;
            packets.push(Packet { fetched: Instant::now(), device_position, flags, mono });
        }
    }
}

// ── analysis ────────────────────────────────────────────────────────────────────────────────────

struct Levels {
    tone_dbfs: f64,
    floor_dbfs: f64,
    peak_dbfs: f64,
    second_rel_db: f64,
    second_lag_ms: f64,
    at20_rel_db: f64,
    periods: usize,
}

struct CaptureReport {
    levels: Option<Levels>,
    why_unknown: Option<String>,
    packets: usize,
    silent_packets: usize,
    discontinuities: usize,
    gap_frames: u64,
    positions: &'static str,
    event_driven: bool,
}

impl CaptureReport {
    fn tone(&self) -> Option<f64> {
        self.levels.as_ref().map(|l| l.tone_dbfs)
    }
    fn snr(&self) -> Option<f64> {
        self.levels.as_ref().map(|l| l.tone_dbfs - l.floor_dbfs)
    }
    fn to_json(&self, render_dbfs: Option<f64>) -> Value {
        let r = |x: f64| json!((x * 10.0).round() / 10.0);
        let mut v = match &self.levels {
            Some(l) => json!({
                "toneDbfs": r(l.tone_dbfs), "floorDbfs": r(l.floor_dbfs), "snrDb": r(l.tone_dbfs - l.floor_dbfs),
                "peakDbfs": r(l.peak_dbfs),
                "gainDb": render_dbfs.map_or(json!("unknown"), |d| r(l.tone_dbfs - d)),
                "secondPeakRelDb": r(l.second_rel_db), "secondPeakLagMs": r(l.second_lag_ms),
                "at20msRelDb": r(l.at20_rel_db), "periodsAveraged": l.periods,
            }),
            None => json!({ "toneDbfs": "unknown", "why": self.why_unknown }),
        };
        let o = v.as_object_mut().unwrap();
        o.insert("packets".into(), json!(self.packets));
        o.insert("silentPackets".into(), json!(self.silent_packets));
        o.insert("discontinuities".into(), json!(self.discontinuities));
        o.insert("gapFrames".into(), json!(self.gap_frames));
        o.insert("positions".into(), json!(self.positions));
        o.insert("eventDriven".into(), json!(self.event_driven));
        v
    }
}

fn analyze_capture(capture: &Capture, go: Instant) -> CaptureReport {
    let packets = &capture.packets;
    let silent_packets = packets.iter().filter(|p| p.flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0).count();
    let discontinuities =
        packets.iter().filter(|p| p.flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0).count();
    // Timeline: place packets by device position when those are consistent (gaps zero-filled),
    // else concatenate in arrival order.
    let consistent = packets.windows(2).all(|w| {
        let expected = w[0].device_position + w[0].mono.len() as u64;
        w[1].device_position >= expected && w[1].device_position - expected < 10 * RATE as u64
    });
    let mut timeline: Vec<f32> = Vec::new();
    let mut marks: Vec<(Instant, usize, usize)> = Vec::new();
    let mut gap_frames = 0u64;
    let base = packets.first().map_or(0, |p| p.device_position);
    for p in packets {
        if consistent {
            let at = (p.device_position - base) as usize;
            if at > timeline.len() {
                gap_frames += (at - timeline.len()) as u64;
                timeline.resize(at, 0.0);
            }
        }
        marks.push((p.fetched, timeline.len(), p.mono.len()));
        timeline.extend_from_slice(&p.mono);
    }
    let mut report = CaptureReport {
        levels: None,
        why_unknown: None,
        packets: packets.len(),
        silent_packets,
        discontinuities,
        gap_frames,
        positions: if consistent { "device" } else { "concatenated" },
        event_driven: capture.event_driven,
    };
    let start = marks.iter().find(|m| m.0 >= go + WINDOW_START).map(|m| m.1);
    let end = marks.iter().rev().find(|m| m.0 <= go + WINDOW_END).map(|m| m.1 + m.2);
    let periods = match (start, end) {
        (Some(s), Some(e)) if e > s => ((e - s) / PERIOD).min(MAX_PERIODS),
        _ => 0,
    };
    if periods < MIN_PERIODS {
        report.why_unknown = Some(format!(
            "{periods} whole periods captured between {} and {} ms after go (need {MIN_PERIODS})",
            WINDOW_START.as_millis(),
            WINDOW_END.as_millis()
        ));
        return report;
    }
    report.levels = Some(levels(&timeline[start.unwrap()..start.unwrap() + periods * PERIOD], periods));
    report
}

fn levels(x: &[f32], periods: usize) -> Levels {
    // Synchronous average at the burst period.
    let mut fold = vec![0.0f64; PERIOD];
    for chunk in x.chunks_exact(PERIOD) {
        for (f, s) in fold.iter_mut().zip(chunk) {
            *f += *s as f64;
        }
    }
    fold.iter_mut().for_each(|f| *f /= periods as f64);
    // Complex matched filter against the Hann-windowed 997 Hz reference; normalised so a burst of
    // amplitude A reads A.
    let window: Vec<f64> =
        (0..BURST).map(|n| 0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / BURST as f64).cos()).collect();
    let norm = 2.0 / window.iter().map(|w| w * w).sum::<f64>();
    let (cos_ref, sin_ref): (Vec<f64>, Vec<f64>) = (0..BURST)
        .map(|n| {
            let ph = std::f64::consts::TAU * FREQ * n as f64 / RATE as f64;
            (window[n] * ph.cos(), window[n] * ph.sin())
        })
        .unzip();
    let envelope: Vec<f64> = (0..PERIOD)
        .map(|k| {
            let (mut c, mut s) = (0.0, 0.0);
            for n in 0..BURST {
                let v = fold[(k + n) % PERIOD];
                c += v * cos_ref[n];
                s += v * sin_ref[n];
            }
            norm * (c * c + s * s).sqrt()
        })
        .collect();
    let k0 = (0..PERIOD).max_by(|&a, &b| envelope[a].total_cmp(&envelope[b])).unwrap();
    let circ = |k: usize, from: usize| (k + PERIOD - from) % PERIOD;
    let k2 = (0..PERIOD)
        .filter(|&k| {
            let d = circ(k, k0);
            d > MAIN_GUARD && d < PERIOD - MAIN_GUARD
        })
        .max_by(|&a, &b| envelope[a].total_cmp(&envelope[b]))
        .unwrap();
    let lag = {
        let d = circ(k2, k0) as f64;
        if d > PERIOD as f64 / 2.0 { d - PERIOD as f64 } else { d }
    };
    let main_db = db(envelope[k0]);
    // Broadband floor on the raw capture, outside the main and strongest-second burst windows.
    let in_burst = |i: usize, at: usize| {
        let d = circ(i % PERIOD, at);
        !(BURST + FLOOR_MARGIN..PERIOD - FLOOR_MARGIN).contains(&d)
    };
    let (mut sum, mut count) = (0.0f64, 0usize);
    let mut peak = 0.0f64;
    for (i, s) in x.iter().enumerate() {
        peak = peak.max(s.abs() as f64);
        if !in_burst(i, k0) && !in_burst(i, k2) {
            sum += (*s as f64) * (*s as f64);
            count += 1;
        }
    }
    let rms = if count > 0 { (sum / count as f64).sqrt() } else { 0.0 };
    Levels {
        tone_dbfs: main_db,
        floor_dbfs: db(rms * std::f64::consts::SQRT_2),
        peak_dbfs: db(peak),
        second_rel_db: db(envelope[k2]) - main_db,
        second_lag_ms: lag * 1000.0 / RATE as f64,
        at20_rel_db: db(envelope[(k0 + COPY_DELAY as usize) % PERIOD]) - main_db,
        periods,
    }
}

// ── parent: one phase ───────────────────────────────────────────────────────────────────────────

/// Kills and reaps the child on every exit path.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

struct Phase {
    mode: Mode,
    child_ready: Value,
    child_done: Value,
    process: CaptureReport,
    endpoint: CaptureReport,
}

impl Phase {
    fn to_json(&self) -> Value {
        let render = self.mode.render_dbfs();
        json!({
            "phase": self.mode.name(),
            "renderDbfs": render.map_or(json!("zeros"), |d| json!(d)),
            "child": { "ready": self.child_ready, "done": self.child_done },
            "processLoopback": self.process.to_json(render),
            "endpointLoopback": self.endpoint.to_json(render),
        })
    }
    fn gain(&self) -> Option<f64> {
        Some(self.process.tone()? - self.mode.render_dbfs()?)
    }
}

fn expect_line(rx: &mpsc::Receiver<String>, prefix: &str, timeout: Duration) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = rx
            .recv_timeout(left)
            .map_err(|_| format!("child sent no '{prefix}' line within {} s", timeout.as_secs()))?;
        if let Some(rest) = line.strip_prefix(prefix) {
            return serde_json::from_str(rest.trim()).map_err(|e| format!("child '{prefix}' line: {e}"));
        }
        eprintln!("{TAG} child: {line}");
    }
}

fn run_phase(mode: Mode) -> Result<Phase, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let child = Command::new(exe)
        .args(["--probe-share-child", mode.name()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawn child: {e}"))?;
    let mut child = ChildGuard(child);
    let pid = child.0.id();
    let mut stdin: ChildStdin = child.0.stdin.take().ok_or("child stdin")?;
    let stdout = child.0.stdout.take().ok_or("child stdout")?;
    let (line_tx, line_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let child_ready = expect_line(&line_rx, "ready ", Duration::from_secs(10))?;

    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for target in [Target::Process(pid), Target::Endpoint] {
        let (tx, rx) = mpsc::channel();
        let flag = stop.clone();
        let handle = thread::spawn(move || capture_thread(target, flag, tx));
        let started = rx.recv_timeout(Duration::from_secs(10));
        match started {
            Ok(Ok(())) => threads.push(handle),
            Ok(Err(e)) => {
                stop.store(true, Ordering::Relaxed);
                return Err(e);
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                return Err(format!("{target:?} capture did not start within 10 s"));
            }
        }
    }
    thread::sleep(Duration::from_millis(200));
    let go = Instant::now();
    let sent = writeln!(stdin, "go").and_then(|_| stdin.flush());
    let done = sent
        .map_err(|e| format!("write 'go' to child: {e}"))
        .and_then(|_| expect_line(&line_rx, "done ", Duration::from_secs(12)));
    let exit_deadline = Instant::now() + Duration::from_secs(3);
    while matches!(child.0.try_wait(), Ok(None)) && Instant::now() < exit_deadline {
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    let mut captures = Vec::new();
    for handle in threads {
        captures.push(handle.join().map_err(|_| "capture thread panicked".to_string())??);
    }
    let child_done = done?;
    let endpoint = analyze_capture(&captures[1], go);
    let process = analyze_capture(&captures[0], go);
    Ok(Phase { mode, child_ready, child_done, process, endpoint })
}

// ── verdict ─────────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Grade {
    Pass,
    Fail,
    Unknown,
    Invalid,
}

fn line(grade: Grade, id: &str, value: Option<f64>, unit: &str, bar: &str) -> Grade {
    let word = match grade {
        Grade::Pass => "PASS",
        Grade::Fail => "FAIL",
        Grade::Unknown => "UNKNOWN",
        Grade::Invalid => "INVALID",
    };
    let value = value.map_or("unknown".to_string(), |v| format!("{v:.1}{unit}"));
    println!("{TAG} {word} {id} {value} {bar}");
    grade
}

fn check(id: &str, value: Option<f64>, unit: &str, bar: &str, ok: impl Fn(f64) -> bool) -> Grade {
    let grade = match value {
        Some(v) if ok(v) => Grade::Pass,
        Some(_) => Grade::Fail,
        None => Grade::Unknown,
    };
    line(grade, id, value, unit, bar)
}

fn all_pass(grades: &[Grade]) -> Grade {
    if grades.iter().all(|g| *g == Grade::Pass) {
        Grade::Pass
    } else if grades.contains(&Grade::Invalid) {
        Grade::Invalid
    } else if grades.contains(&Grade::Fail) {
        Grade::Fail
    } else {
        Grade::Unknown
    }
}

fn verdict(phases: &[Phase]) {
    let find = |m: Mode| phases.iter().find(|p| p.mode == m);
    let open = find(Mode::Open);
    // Positive controls: process loopback hears the audible tone clearly, and endpoint loopback
    // sees it at all (so a ≤ −80 dBFS endpoint reading elsewhere means silence, not a wrong device).
    let control = open.map(|o| {
        let p = check("S1.control.process", o.process.snr(), "dB", ">=30dB_over_floor", |v| v >= 30.0);
        let e = check("S1.control.endpoint", o.endpoint.tone(), "dBFS", ">=-60dBFS", |v| v >= -60.0);
        match (p, e) {
            (Grade::Pass, Grade::Pass) => Grade::Pass,
            _ => Grade::Invalid,
        }
    });
    if control == Some(Grade::Invalid) {
        println!("{TAG} INVALID S1 the open positive control failed (setup fault)");
        return;
    }
    let mut silent = Vec::new();
    for mode in [Mode::Mute, Mode::Vol0] {
        let Some(p) = find(mode) else { continue };
        let n = mode.name();
        let delta = match (p.gain(), open.and_then(Phase::gain)) {
            (Some(g), Some(o)) => Some(g - o),
            _ => None,
        };
        let grades = [
            check(&format!("S1.{n}.level"), delta, "dB", "|gain-open|<=3dB", |v| v.abs() <= 3.0),
            check(&format!("S1.{n}.snr"), p.process.snr(), "dB", ">=30dB_over_floor", |v| v >= 30.0),
            check(&format!("S1.{n}.room"), p.endpoint.tone(), "dBFS", "<=-80dBFS", |v| v <= -80.0),
        ];
        silent.push(all_pass(&grades));
    }
    let zeros = find(Mode::Zeros).map(|p| {
        all_pass(&[
            check("S1.zeros.process", p.process.tone(), "dBFS", "<=-80dBFS", |v| v <= -80.0),
            check("S1.zeros.endpoint", p.endpoint.tone(), "dBFS", "<=-80dBFS", |v| v <= -80.0),
        ])
    });
    let dual = find(Mode::Dual).map(|p| {
        if p.process.snr().is_some_and(|v| v < 30.0) {
            line(Grade::Invalid, "S1.dual", p.process.snr(), "dB", "main_burst_>=30dB_over_floor");
            return Grade::Invalid;
        }
        let second = p.process.levels.as_ref().map(|l| l.second_rel_db);
        check("S1.dual", second, "dB", "second_peak<-12dB", |v| v < -12.0)
    });
    if phases.len() < 5 {
        return; // single-mode run: part lines only; the full verdict needs `all`
    }
    let silent_any = if silent.contains(&Grade::Pass) { Grade::Pass } else { all_pass(&silent) };
    let parts = [silent_any, zeros.unwrap_or(Grade::Unknown), dual.unwrap_or(Grade::Unknown)];
    let word = match all_pass(&parts) {
        Grade::Pass => "PASS",
        Grade::Fail => "FAIL",
        Grade::Invalid => "INVALID",
        Grade::Unknown => "UNKNOWN",
    };
    println!("{TAG} {word} S1 (mute|vol0)&zeros&dual");
}
