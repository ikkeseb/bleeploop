//! One-shot DEV software-path measurement. No absolute ASIO epoch is used. Callback entry is
//! timestamped on our monotonic clock; its unknown delay from ASIO buffer-switch time remains an
//! explicit limitation. PCM never leaves Rust: the command returns correlated marker positions.
#![cfg(all(windows, debug_assertions))]

use std::sync::{
    atomic::{
        AtomicU32, AtomicU64, AtomicUsize,
        Ordering::{Acquire, Relaxed, Release},
    },
    OnceLock,
};
use std::time::Instant;

const CHIPS: &[u8] = b"111111000001000011000101001111010001110010010110111011001101010";
// Avoid locking every marker to the same phase of a 10 ms browser output callback.
const START_MS: [u64; 8] = [413, 937, 1463, 2019, 2597, 3197, 3823, 4471];
const CAPACITY: usize = 96_000 * 6;
static EPOCH: OnceLock<Instant> = OnceLock::new();
static RECORDING: OnceLock<Recording> = OnceLock::new();

#[derive(Default)]
struct CallbackStamp {
    first_sample: AtomicUsize,
    entry_ns: AtomicU64,
    driver_delay_ns: AtomicU64,
}

pub(crate) struct Recording {
    target: usize,
    slot: u8,
    loss_before: [u64; 2],
    end_ns: AtomicU64,
    samples: Box<[AtomicU32]>,
    times: Box<[AtomicU64]>,
    write: AtomicUsize,
    source_frames: AtomicU64,
    injection_times: [AtomicU64; 8],
    injection_offsets: [AtomicU32; 8],
    injection_rates: [AtomicU64; 8],
    callbacks: Box<[CallbackStamp]>,
    callback_count: AtomicUsize,
    rate: AtomicU32,
    invalid: AtomicU64,
}

fn now_ns() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

#[tauri::command]
pub(crate) fn marker_probe_clock() -> f64 {
    now_ns() as f64 / 1e6
}

#[tauri::command]
pub(crate) fn marker_probe_begin(
    state: tauri::State<'_, crate::host::PluginHostState>,
    slot: u8,
) -> Result<(), String> {
    let (target, loss_before) = crate::host::marker_probe_target(&state, slot)?;
    if RECORDING.get().is_some() {
        return Err("Marker probe is one-shot; relaunch for another configuration".into());
    }
    let recording = Recording {
        target,
        slot,
        loss_before,
        end_ns: AtomicU64::new(0),
        samples: (0..CAPACITY).map(|_| AtomicU32::new(0)).collect(),
        times: (0..CAPACITY).map(|_| AtomicU64::new(0)).collect(),
        write: AtomicUsize::new(0),
        source_frames: AtomicU64::new(0),
        injection_times: std::array::from_fn(|_| AtomicU64::new(0)),
        injection_offsets: std::array::from_fn(|_| AtomicU32::new(0)),
        injection_rates: std::array::from_fn(|_| AtomicU64::new(0)),
        callbacks: (0..16_384).map(|_| CallbackStamp::default()).collect(),
        callback_count: AtomicUsize::new(0),
        rate: AtomicU32::new(0),
        invalid: AtomicU64::new(0),
    };
    recording.end_ns.store(now_ns() + 5_000_000_000, Relaxed);
    RECORDING
        .set(recording)
        .map_err(|_| "Marker probe already started".into())
}

/// Cancellation stops new markers. Keep injecting silence and suppressing native output for two
/// seconds to drain queued PCM. This deadline also runs without frontend cooperation.
#[tauri::command]
pub(crate) fn marker_probe_cancel() {
    if let Some(r) = RECORDING.get() {
        r.end_ns.fetch_min(now_ns(), Relaxed);
    }
}

pub(crate) fn inject(target: usize, mono: &mut [f32], rate: f64) {
    let Some(r) = RECORDING.get() else {
        return;
    };
    if target != r.target {
        return;
    }
    let now = now_ns();
    let end = r.end_ns.load(Relaxed);
    if now >= end + 2_000_000_000 {
        return;
    }
    mono.fill(0.0);
    if now >= end {
        return;
    }
    let first = r.source_frames.fetch_add(mono.len() as u64, Relaxed);
    let rate_frames = rate as u64;
    let starts = START_MS.map(|ms| (ms * rate_frames + 500) / 1000);
    let marker_frames = (CHIPS.len() as u64 * rate_frames).div_ceil(6000);
    for (burst, start) in starts.into_iter().enumerate() {
        let from = first.max(start);
        let to = (first + mono.len() as u64).min(start + marker_frames);
        if from >= to {
            continue;
        }
        if from == start {
            // All samples in this block are published together. Keep the marker's offset
            // separate from the observed block-entry instant; offset/rate is NOT an observed
            // physical or wall-clock injection delay.
            r.injection_offsets[burst].store((start - first) as u32, Relaxed);
            r.injection_rates[burst].store(rate.to_bits(), Relaxed);
            r.injection_times[burst].store(now, Release);
        }
        for frame in from..to {
            let chip = ((frame - start) * 6000 / rate_frames) as usize;
            mono[(frame - first) as usize] = if CHIPS[chip] == b'1' { 0.001 } else { -0.001 };
        }
    }
}

/// Called once at callback entry. The first tuple field is the monotonic callback-entry instant
/// plus SAME-callback reported presentation delay, never cpal's broken absolute ASIO epoch.
pub(crate) fn output_begin(info: &cpal::OutputCallbackInfo, rate: u32) -> Option<(u64, bool)> {
    let r = RECORDING.get()?;
    let now = now_ns();
    let end = r.end_ns.load(Relaxed);
    if now >= end + 2_000_000_000 {
        return None;
    }
    if let Err(previous) = r.rate.compare_exchange(0, rate, Relaxed, Relaxed) {
        if previous != rate {
            r.invalid.fetch_add(1, Relaxed);
        }
    }
    let ts = info.timestamp();
    let delay = ts.playback.checked_duration_since(ts.callback);
    let ns = match delay {
        Some(d) if !d.is_zero() && d.as_secs_f64() < 1.0 => d.as_nanos() as u64,
        _ => {
            r.invalid.fetch_add(1, Relaxed);
            0
        }
    };
    if now < end {
        let index = r.callback_count.load(Relaxed);
        if let Some(stamp) = r.callbacks.get(index) {
            stamp.first_sample.store(r.write.load(Relaxed), Relaxed);
            stamp.entry_ns.store(now, Relaxed);
            stamp.driver_delay_ns.store(ns, Relaxed);
            r.callback_count.store(index + 1, Release);
        } else {
            r.invalid.fetch_add(1, Relaxed);
        }
    }
    Some((now + ns, now < end))
}

pub(crate) fn output_sample(
    context: Option<(u64, bool)>,
    sample: Option<f32>,
    offset: usize,
    rate: u32,
) {
    let Some((time, true)) = context else {
        return;
    };
    let r = RECORDING.get().unwrap();
    let sample = match sample {
        Some(v) => v,
        None => {
            r.invalid.fetch_add(1, Relaxed);
            0.0
        }
    };
    let i = r.write.load(Relaxed);
    if i >= CAPACITY || !(44100..=96000).contains(&rate) {
        r.invalid.fetch_add(1, Relaxed);
        return;
    }
    r.samples[i].store(sample.to_bits(), Relaxed);
    r.times[i].store(time + offset as u64 * 1_000_000_000 / rate as u64, Relaxed);
    r.write.store(i + 1, Release);
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Marker {
    frame: usize,
    score: f64,
    presentation_ms: f64,
    injection_time_ms: f64,
    injection_offset_frames: u32,
    injection_sample_rate: f64,
    callback_entry_time_ms: f64,
    reported_driver_delay_ms: f64,
    callback_offset_frames: usize,
    native_sample_rate: u32,
}

fn find_markers(samples: &[f32], rate: u32) -> Vec<(usize, f64)> {
    let reference: Vec<f64> = (0..(CHIPS.len() as f64 * rate as f64 / 6000.0).ceil() as usize)
        .map(|i| {
            if CHIPS[(i as f64 * 6000.0 / rate as f64).floor() as usize] == b'1' {
                1.0
            } else {
                -1.0
            }
        })
        .collect();
    let score = |start: usize| {
        let mut dot = 0.0;
        let mut energy = 0.0;
        for (k, reference) in reference.iter().enumerate() {
            let v = samples[start + k] as f64;
            dot += v * reference;
            energy += v * v;
        }
        if energy > 1e-12 {
            dot / (energy * reference.len() as f64).sqrt()
        } else {
            0.0
        }
    };
    let mut result = Vec::new();
    let mut i = 0;
    while i + reference.len() < samples.len() {
        if score(i) >= 0.72 {
            let mut best = (i, -1.0);
            for j in i.saturating_sub(4)..=(i + 12).min(samples.len() - reference.len() - 1) {
                let value = score(j);
                if value > best.1 {
                    best = (j, value);
                }
            }
            if best.1 >= 0.85 {
                result.push(best);
                i = best.0 + rate as usize / 4;
            }
        }
        i += 4;
    }
    result
}

#[tauri::command]
pub(crate) async fn marker_probe_result(
    state: tauri::State<'_, crate::host::PluginHostState>,
) -> Result<Vec<Marker>, String> {
    let r = RECORDING.get().ok_or("Probe not started")?;
    if now_ns() < r.end_ns.load(Relaxed) + 2_000_000_000 {
        return Err("Probe is still draining its silent tail".into());
    }
    let (target, loss) = crate::host::marker_probe_target(&state, r.slot)?;
    if target != r.target || loss != r.loss_before {
        return Err("Native plugin changed or transport dropped samples".into());
    }
    if r.invalid.load(Relaxed) != 0 {
        return Err("Native underrun, invalid timestamps/rate or capture overflow".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let len = r.write.load(Acquire);
        let samples: Vec<f32> = r.samples[..len]
            .iter()
            .map(|v| f32::from_bits(v.load(Relaxed)))
            .collect();
        let markers = find_markers(&samples, r.rate.load(Relaxed));
        if markers.len() != 8 {
            return Err(format!(
                "Expected 8 native markers, detected {}",
                markers.len()
            ));
        }
        if r.injection_times.iter().any(|time| time.load(Acquire) == 0) {
            return Err("Missing native injection timestamp".into());
        }
        let callbacks = &r.callbacks[..r.callback_count.load(Acquire)];
        if callbacks.is_empty() || callbacks[0].first_sample.load(Relaxed) != 0 {
            return Err("Missing native callback timestamp".into());
        }
        Ok(markers
            .into_iter()
            .enumerate()
            .map(|(i, (frame, score))| {
                // One timestamp per callback. Correlation locates the marker's exact sample in
                // that callback; the offset is reported separately from its observed entry time.
                let stamp = callbacks
                    .iter()
                    .rev()
                    .find(|stamp| stamp.first_sample.load(Relaxed) <= frame)
                    .unwrap();
                Marker {
                    frame,
                    score,
                    presentation_ms: r.times[frame].load(Relaxed) as f64 / 1e6,
                    injection_time_ms: r.injection_times[i].load(Relaxed) as f64 / 1e6,
                    injection_offset_frames: r.injection_offsets[i].load(Relaxed),
                    injection_sample_rate: f64::from_bits(r.injection_rates[i].load(Relaxed)),
                    callback_entry_time_ms: stamp.entry_ns.load(Relaxed) as f64 / 1e6,
                    reported_driver_delay_ms: stamp.driver_delay_ns.load(Relaxed) as f64 / 1e6,
                    callback_offset_frames: frame - stamp.first_sample.load(Relaxed),
                    native_sample_rate: r.rate.load(Relaxed),
                }
            })
            .collect())
    })
    .await
    .map_err(|e| format!("Marker analysis failed: {e}"))?
}

#[cfg(test)]
mod tests {
    #[test]
    fn marker_detector_actual_pattern_survives_fractional_interpolation() {
        for rate in [44100u32, 48000, 96000] {
            let mut samples = vec![0.0f32; rate as usize];
            for i in 0..(super::CHIPS.len() as f64 * rate as f64 / 6000.0).ceil() as usize {
                samples[137 + i] =
                    if super::CHIPS[(i as f64 * 6000.0 / rate as f64).floor() as usize] == b'1' {
                        0.037
                    } else {
                        -0.037
                    };
            }
            for i in (1..samples.len()).rev() {
                samples[i] = samples[i] * 0.7 + samples[i - 1] * 0.3;
            }
            let markers = super::find_markers(&samples, rate);
            assert_eq!(markers.len(), 1);
            assert_eq!(markers[0].0, 137);
        }
    }
}
