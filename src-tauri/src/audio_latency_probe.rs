//! Silent DEV probe of the production output callback's reported device latency.
//! This measures driver timestamps, not the physical DAC or an analogue loopback.
#![cfg(all(windows, debug_assertions))]

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static ENABLED: AtomicBool = AtomicBool::new(false);
static BLOCK: AtomicU32 = AtomicU32::new(0);
static COUNT: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static NS_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
static NS_MAX: AtomicU64 = AtomicU64::new(0);
static NS_SUM: AtomicU64 = AtomicU64::new(0);
static FRAMES_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
static FRAMES_MAX: AtomicU64 = AtomicU64::new(0);

pub(crate) fn config(mut config: cpal::StreamConfig) -> cpal::StreamConfig {
    if ENABLED.load(Relaxed) {
        let frames = BLOCK.load(Relaxed);
        if frames != 0 {
            config.buffer_size = cpal::BufferSize::Fixed(frames);
        }
    }
    config
}

pub(crate) fn observe(frames: usize, info: &cpal::OutputCallbackInfo) {
    if !ENABLED.load(Relaxed) {
        return;
    }
    let timestamp = info.timestamp();
    // checked_duration_since catches backward timestamps instead of treating them as zero.
    // ASIO constructs playback = callback + driver latency AFTER its epoch conversion. Their
    // difference uses StreamInstant's u128 arithmetic and does not inherit a wrapped u64 epoch.
    let Some(delta) = timestamp.playback.checked_duration_since(timestamp.callback) else {
        INVALID.fetch_add(1, Relaxed);
        return;
    };
    if delta > Duration::from_secs(1) {
        INVALID.fetch_add(1, Relaxed);
        return;
    }
    let ns = delta.as_nanos() as u64;
    NS_MIN.fetch_min(ns, Relaxed);
    NS_MAX.fetch_max(ns, Relaxed);
    NS_SUM.fetch_add(ns, Relaxed);
    FRAMES_MIN.fetch_min(frames as u64, Relaxed);
    FRAMES_MAX.fetch_max(frames as u64, Relaxed);
    COUNT.fetch_add(1, Relaxed);
}

pub(crate) fn run(args: &[String]) -> Result<(), String> {
    use crate::audio_output::{self, AudioBackend};
    let backend = match args.first().map(String::as_str) {
        Some("asio") => AudioBackend::Asio,
        Some("wasapi") => AudioBackend::Wasapi,
        _ => return Err("usage: --probe-output-latency <asio|wasapi> [default|128|256] [seconds]".into()),
    };
    let block = match args.get(1).map(String::as_str).unwrap_or("default") {
        "default" => 0,
        "128" => 128,
        "256" => 256,
        _ => return Err("probe block must be default, 128 or 256".into()),
    };
    let seconds = args.get(2).map(|s| s.parse::<u64>()).transpose()
        .map_err(|e| format!("invalid duration: {e}"))?.unwrap_or(4).clamp(1, 45);
    #[cfg(feature = "asio")]
    if backend.is_asio() {
        audio_output::cache_asio();
        println!("[output-latency-probe] asioDeviceInfo={}", serde_json::to_string(&crate::host::plugin_asio_device_info()).unwrap());
    }
    BLOCK.store(block, Relaxed);
    ENABLED.store(true, Relaxed);
    // An empty production monitor ring produces silence, including through its normal gain/fade
    // logic. No plugin, input capture, WebView or frontend is started by this command.
    let (_producer, consumer) = rtrb::RingBuffer::<f32>::new(1024);
    let fault = Arc::new(AtomicBool::new(false));
    let output_latency_ns = Arc::new(AtomicU64::new(0));
    let (stream, rate) = audio_output::open_output_stream(
        backend, None, Arc::new(Mutex::new(consumer)),
        Arc::new(AtomicU32::new(0.0f32.to_bits())), Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU32::new(0.0f32.to_bits())), Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU32::new(0)), output_latency_ns.clone(), fault.clone(),
    )?;
    println!("[output-latency-probe] started backend={backend:?} requested_block={block} rate={rate} seconds={seconds}");
    std::thread::sleep(Duration::from_secs(seconds));
    drop(stream);
    ENABLED.store(false, Relaxed);
    let count = COUNT.load(Relaxed);
    if count == 0 {
        return Err(format!("no valid output callbacks; invalid={}", INVALID.load(Relaxed)));
    }
    println!("[output-latency-probe] {}", serde_json::json!({
        "backend": format!("{backend:?}"), "requestedBlock": block, "sampleRate": rate,
        "seconds": seconds, "callbacks": count, "invalid": INVALID.load(Relaxed),
        "streamFault": fault.load(Relaxed),
        "callbackFramesMin": FRAMES_MIN.load(Relaxed), "callbackFramesMax": FRAMES_MAX.load(Relaxed),
        "callbackMsMin": FRAMES_MIN.load(Relaxed) as f64 / rate as f64 * 1000.0,
        "callbackMsMax": FRAMES_MAX.load(Relaxed) as f64 / rate as f64 * 1000.0,
        "reportedMsMin": NS_MIN.load(Relaxed) as f64 / 1e6,
        "reportedMsMax": NS_MAX.load(Relaxed) as f64 / 1e6,
        "reportedMsMean": NS_SUM.load(Relaxed) as f64 / count as f64 / 1e6,
        "productionMedianMs": output_latency_ns.load(Relaxed) as f64 / 1e6,
    }));
    if fault.load(Relaxed) || INVALID.load(Relaxed) != 0 {
        return Err("invalid timestamps or a stream fault occurred".into());
    }
    let published = output_latency_ns.load(Relaxed);
    if published == 0 || published < NS_MIN.load(Relaxed) || published > NS_MAX.load(Relaxed) {
        return Err("production median did not publish a valid observed device latency".into());
    }
    Ok(())
}
