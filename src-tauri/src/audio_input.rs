//! P11.0 — native audio INPUT capture (cpal, WASAPI-shared).
//!
//! This is the Rust-internal capture layer for the P11 audio-input path: a hardware guitar/line
//! signal is captured here, downmixed to mono f32, and pushed into the host side of an `rtrb` ring
//! (`Producer<f32>`). The RT producer loop (`host::clap`) drains that ring per block into
//! the hosted CLAP/VST3 plugin's input bus. **No PCM crosses the capability boundary** — the wet
//! (processed) signal returns to Web Audio only via the existing P9 hop-1 AudioNode path.
//!
//! cpal is ALREADY a dependency (`Cargo.toml`, `default-features = false`, realtime OFF — MMCSS
//! promotion is done manually in `host::clap`), but this is its FIRST actual use in the codebase: P9's
//! WASAPI device-period query went raw-`windows`-COM, not cpal. So there is no in-repo cpal pattern
//! to mirror — the API below was confirmed against cpal's `UPGRADING.md` via Context7.
#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, TryLockError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Sample, SampleFormat, StreamConfig};
use rtrb::Producer;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

/// Wakes the plugin's RT producer when a capture callback has pushed frames, so the producer runs on
/// the capture device's clock (`host::transport::Hop1Pipe::pace_on_input`). An auto-reset event:
/// `signal` is one non-blocking kernel call, safe on the driver's callback thread. A failed create
/// leaves it invalid, and the producer keeps its own timer.
pub struct InputWake(HANDLE);

// SAFETY: an event handle may be signalled and waited on from any thread; `Drop` closes it once.
unsafe impl Send for InputWake {}
unsafe impl Sync for InputWake {}

impl InputWake {
    pub fn new() -> Self {
        // SAFETY: plain auto-reset, initially unsignalled, unnamed event.
        Self(unsafe { CreateEventW(None, false, false, None) }.unwrap_or_default())
    }

    /// The event to wait on; `None` if it could not be created.
    pub fn handle(&self) -> Option<HANDLE> {
        (!self.0.is_invalid()).then_some(self.0)
    }

    fn signal(&self) {
        if let Some(h) = self.handle() {
            // SAFETY: `h` is our live event.
            let _ = unsafe { SetEvent(h) };
        }
    }
}

impl Drop for InputWake {
    fn drop(&mut self) {
        if let Some(h) = self.handle() {
            // SAFETY: created by `new`, closed once here.
            let _ = unsafe { CloseHandle(h) };
        }
    }
}

/// Selection for one concrete capture stream. ASIO retains this alongside the stream across disarm.
pub struct InputChannelControl {
    channels: usize,
    selected: AtomicU32,
}

impl InputChannelControl {
    fn resolve(channels: usize, requested: Option<u32>) -> Result<u32, String> {
        match requested {
            Some(channel) if (channel as usize) < channels => Ok(channel),
            Some(channel) => Err(format!("input channel {} is unavailable; this device has {channels} channels", channel as u64 + 1)),
            None => Ok(if channels >= 2 { 1 } else { 0 }),
        }
    }

    fn new(channels: usize, requested: Option<u32>) -> Result<Self, String> {
        Ok(Self { channels, selected: AtomicU32::new(Self::resolve(channels, requested)?) })
    }

    /// Owner-thread rearm. Wait for an in-flight old-channel callback before changing the pick.
    /// The caller publishes input_gen afterward, so no old-channel writer can follow that flush.
    pub fn select(&self, requested: Option<u32>, producer: &Mutex<Producer<f32>>) -> Result<(), String> {
        let selected = Self::resolve(self.channels, requested)?;
        let _guard = producer.lock().map_err(|_| "capture producer lock poisoned".to_string())?;
        self.selected.store(selected, Relaxed);
        Ok(())
    }
}

/// The real device callback's conversion and ring write, also exercised with multichannel fixtures.
fn capture_channel<T: Sample>(
    data: &[T], channels: usize, selection: &InputChannelControl,
    producer: &Mutex<Producer<f32>>, overruns: &AtomicU64, wake: Option<&InputWake>,
) where f32: cpal::FromSample<T> {
    match producer.try_lock() {
        Ok(mut producer) => {
            // Read INSIDE the lock: an old callback cannot retain its pick across an owner rearm.
            let channel = selection.selected.load(Relaxed) as usize;
            let mut dropped = 0u64;
            for frame in data.chunks_exact(channels) {
                if producer.push(f32::from_sample(frame[channel])).is_err() {
                    dropped += 1;
                }
            }
            if dropped > 0 {
                overruns.fetch_add(dropped, Relaxed);
            }
            if let Some(w) = wake {
                w.signal();
            }
        }
        Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => {
            overruns.fetch_add((data.len() / channels) as u64, Relaxed);
        }
    }
}

#[cfg(test)]
mod channel_tests {
    use super::*;
    use std::sync::atomic::Ordering::{Acquire, Release};
    use std::time::Duration;

    #[test]
    fn retained_control_selects_exact_stereo_pcm_and_rejects_missing_channels() {
        let (producer, mut consumer) = rtrb::RingBuffer::new(16);
        let producer = Mutex::new(producer);
        let overruns = AtomicU64::new(0);
        let channel = InputChannelControl::new(2, None).unwrap();
        let stereo = [0.75f32, -0.25, 0.5, -0.125];
        capture_channel(&stereo, 2, &channel, &producer, &overruns, None);
        assert_eq!([consumer.pop().unwrap(), consumer.pop().unwrap()], [-0.25, -0.125]);
        channel.select(Some(0), &producer).unwrap();
        capture_channel(&stereo, 2, &channel, &producer, &overruns, None);
        assert_eq!([consumer.pop().unwrap(), consumer.pop().unwrap()], [0.75, 0.5]);

        // ASIO commonly supplies I32. Exercise the same generic conversion invoked by that callback.
        let stereo_i32 = [1_073_741_824i32, -536_870_912, -1_073_741_824, 536_870_912];
        channel.select(None, &producer).unwrap();
        capture_channel(&stereo_i32, 2, &channel, &producer, &overruns, None);
        assert_eq!([consumer.pop().unwrap(), consumer.pop().unwrap()], [-0.25, 0.25]);
        assert!(channel.select(Some(2), &producer).is_err());
        capture_channel(&stereo, 2, &channel, &producer, &overruns, None);
        assert_eq!([consumer.pop().unwrap(), consumer.pop().unwrap()], [-0.25, -0.125]);
        assert!(consumer.pop().is_err());
        assert_eq!(overruns.load(Relaxed), 0);
        assert_eq!(InputChannelControl::new(1, None).unwrap().selected.load(Relaxed), 0);
        assert!(InputChannelControl::new(1, Some(1)).is_err());
    }

    #[test]
    fn rearm_waits_for_old_channel_writer_before_publishing_the_input_generation() {
        let (producer, mut consumer) = rtrb::RingBuffer::new(16);
        let producer = Arc::new(Mutex::new(producer));
        let channel = Arc::new(InputChannelControl::new(2, Some(0)).unwrap());
        let generation = Arc::new(AtomicU32::new(0));
        // Hold the exact mutex used by capture_channel to represent an old callback in flight.
        let mut old_writer = producer.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_producer = producer.clone();
        let worker_channel = channel.clone();
        let worker_generation = generation.clone();
        let owner = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            worker_channel.select(Some(1), &worker_producer).unwrap();
            // NativeIo publishes input_gen only after select returns.
            worker_generation.store(1, Release);
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(20)).is_err());
        assert_eq!(channel.selected.load(Relaxed), 0);
        assert_eq!(generation.load(Acquire), 0);
        old_writer.push(0.75).unwrap();
        drop(old_writer);
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        owner.join().unwrap();
        assert_eq!(generation.load(Acquire), 1);
        assert_eq!(consumer.pop().unwrap(), 0.75); // RT discards the old generation's queued PCM.
        let overruns = AtomicU64::new(0);
        capture_channel(&[0.75f32, -0.5], 2, &channel, &producer, &overruns, None);
        assert_eq!(consumer.pop().unwrap(), -0.5);
        assert!(consumer.pop().is_err());
    }

    #[test]
    fn contended_capture_lock_counts_every_dropped_frame() {
        let (producer, mut consumer) = rtrb::RingBuffer::new(16);
        let producer = Arc::new(Mutex::new(producer));
        let channel = InputChannelControl::new(2, Some(0)).unwrap();
        let overruns = AtomicU64::new(0);
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker_producer = producer.clone();
        let holder = std::thread::spawn(move || {
            let _guard = worker_producer.lock().unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });

        locked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        capture_channel(&[0.75f32, -0.5, 0.25, -0.125], 2, &channel, &producer, &overruns, None);
        assert_eq!(overruns.load(Relaxed), 2);
        assert!(consumer.pop().is_err());
        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }
}

/// One enumerated capture device. `id` is the stable cpal `DeviceId` string (round-trips through
/// `host.device_by_id` for arm-by-id); `name` is the human-readable description. Maps 1:1 to the
/// `AudioInputDevice` boundary struct the `plugin_list_input_devices` command returns to JS.
pub struct InputDeviceInfo {
    pub id: String,
    pub name: String,
    pub channels: u32,
}

/// Enumerate WASAPI-shared input devices. Opens NO stream (like the existing WASAPI period query),
/// so it is safe to call directly off the command thread — no owner-thread hop needed.
pub fn list_input_devices() -> Result<Vec<InputDeviceInfo>, String> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|e| format!("cpal input_devices: {e}"))?;
    let mut out = Vec::new();
    for dev in devices {
        // name() is deprecated in 0.18 → description() (Display) for the human name, id() for a
        // stable handle. Both are fallible; degrade gracefully rather than dropping the device.
        let name = dev
            .description()
            .map(|d| d.to_string())
            .unwrap_or_else(|_| "Unknown input".to_string());
        let id = dev
            .id()
            .map(|i| i.to_string())
            .unwrap_or_else(|_| name.clone());
        let channels = dev
            .default_input_config()
            .map(|c| c.channels() as u32)
            .unwrap_or(0);
        out.push(InputDeviceInfo { id, name, channels });
    }
    Ok(out)
}

/// Pick the WASAPI capture device (None = default). The ASIO low-latency tier does NOT go through here —
/// it uses the startup-cached duplex device (`audio_output::asio_cache()`) shared with the monitor, so
/// capture + playback ride ONE full-duplex driver (one clock); the single ASIO driver can't be
/// re-resolved once a stream holds it.
fn pick_input_device(device_id: Option<&str>) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    match device_id {
        Some(id) => {
            let parsed = id.parse().map_err(|_| format!("bad cpal device id: {id}"))?;
            host.device_by_id(&parsed)
                .ok_or_else(|| format!("cpal device_by_id({id}): not found"))
        }
        None => host
            .default_input_device()
            .ok_or_else(|| "no default input device".to_string()),
    }
}

/// Open a WASAPI-shared capture stream on `device_id` (None = default input), isolating one input
/// `channel`. Every captured frame is reduced to that single mono channel as f32 and pushed into
/// `producer` (the host side of the P11 input ring). The `Stream` is `!Send` — the caller MUST build
/// + keep it on the owner thread; dropping it stops capture (RAII). Only one stream feeds `producer`
/// at a time (the owner disarms before re-arming), so the single-producer `rtrb` invariant holds
/// across re-arm.
///
/// `channel` is the UI channel selector (0-based; `Some(c)` = explicit pick, validated against the device's
/// channel count; `None` = auto). A multi-input interface like the Scarlett exposes ALL its inputs as
/// ONE interleaved cpal stream, so the channel pick MUST happen here — averaging to mono halves the
/// chosen signal (−6 dB) and mixes in the other (often empty/mic) inputs.
///
/// `producer` is `Arc<Mutex<_>>` only to hand the single SPSC `Producer` to successive cpal
/// callbacks across arm/disarm cycles. The owner thread never locks it during steady capture (only
/// the cpal callback does), so the `try_lock` below is effectively uncontended and never blocks the
/// audio thread; a contended/poisoned lock simply drops that callback's frames.
/// Returns the stream, native capture rate (`R_in`, in Hz), and retained channel control. The
/// RT producer needs `R_in` to build the input resampler (`R_in`→`D`) — see `host::transport`'s `InPipe`.
///
/// `overruns` counts capture frames dropped by a full ring or an unavailable producer lock
/// (owner-local; mirrored into `ProducerDiag::input_overruns` at each gate emit). It is the only
/// visibility into capture loss. A full ring means the RT consumer is behind, which never produces
/// a consumer-side starve.
///
/// `fault` is latched true by cpal's ERROR callback — which is TERMINAL by contract: the stream is
/// dead and never resumes (interface unplugged, ASIO driver reset). The owner loop polls it and tears
/// the corpse down; the recoverable ring-full case stays the `overruns` counter, never this flag.
pub fn open_input_stream(
    backend: crate::audio_output::AudioBackend,
    device_id: Option<&str>,
    channel: Option<u32>,
    producer: Arc<Mutex<Producer<f32>>>,
    overruns: Arc<AtomicU64>,
    fault: Arc<AtomicBool>,
    wake: Arc<InputWake>,
) -> Result<(cpal::Stream, u32, Arc<InputChannelControl>), String> {
    // ASIO low-latency tier: capture on the startup-cached duplex device shared with the monitor (the
    // single ASIO driver can't be re-resolved once a stream holds it). WASAPI: resolve fresh by id.
    if backend.is_asio() {
        #[cfg(feature = "asio")]
        {
            let c = crate::audio_output::asio_cache()
                .ok_or_else(|| "ASIO backend selected without a cached device".to_string())?;
            log::info!("[audio_input] ASIO capture on cached duplex device");
            // Retry once: the FIRST build after a prior plugin unload left the ASIO driver running with
            // `asio_streams` still populated → cpal's reuse path calls `driver.start()` on a running
            // driver → BadMode. The failed build rolls back `asio_streams.X = None`, so the retry takes
            // the prepare path (which resets the driver) and succeeds. (See audio_output mirror.)
            return match build_input_on(
                &c.device,
                c.in_cfg,
                c.in_fmt,
                channel,
                producer.clone(),
                overruns.clone(),
                fault.clone(),
                wake.clone(),
            ) {
                Ok(r) => Ok(r),
                Err(first) => {
                    log::warn!("[audio_input] ASIO input build failed ({first}); retrying once");
                    build_input_on(
                        &c.device, c.in_cfg, c.in_fmt, channel, producer, overruns, fault, wake,
                    )
                }
            };
        }
        #[cfg(not(feature = "asio"))]
        return Err("ASIO backend is unavailable in this build".to_string());
    }
    let device = pick_input_device(device_id)?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("cpal default_input_config: {e}"))?;
    let config = StreamConfig {
        channels: supported.channels(),
        sample_rate: supported.sample_rate(),
        buffer_size: BufferSize::Default,
    };
    build_input_on(
        &device,
        config,
        supported.sample_format(),
        channel,
        producer,
        overruns,
        fault,
        wake,
    )
}

/// Build + start a capture stream on `device` with `config`/`fmt`, isolating one input `channel`.
/// `channel`: `Some(c)` = explicit pick validated against the channel count; `None` = auto (input 2 / index 1 on a
/// ≥2-in device — where Hi-Z/instrument inputs usually sit — else the sole channel). Each frame is
/// reduced to that mono channel as f32 and pushed into `producer`. Returns (stream, R_in, channel control). Shared by
/// the WASAPI + ASIO paths; the `!Send` stream must stay on the caller's (owner) thread.
///
/// `fault` carries the terminal-error latch (see `open_input_stream`); `wake` is signalled after each
/// push, so the RT producer runs on this stream's clock.
fn build_input_on(
    device: &cpal::Device,
    config: StreamConfig,
    fmt: SampleFormat,
    channel: Option<u32>,
    producer: Arc<Mutex<Producer<f32>>>,
    overruns: Arc<AtomicU64>,
    fault: Arc<AtomicBool>,
    wake: Arc<InputWake>,
) -> Result<(cpal::Stream, u32, Arc<InputChannelControl>), String> {
    let in_ch = (config.channels as usize).max(1);
    let in_rate = config.sample_rate;

    let channel_control = Arc::new(InputChannelControl::new(in_ch, channel)?);

    // Downmix interleaved frames → mono and push each sample. Built per sample-format because the
    // device may deliver I16/U16/I32 rather than F32; `f32::from_sample` (FromSample) converts.
    macro_rules! build_stream {
        ($T:ty) => {{
            let prod = producer.clone();
            let channel_a = channel_control.clone();
            let over_a = overruns.clone();
            let fault_a = fault.clone();
            let wake_a = wake.clone();
            device.build_input_stream(
                config,
                move |data: &[$T], _: &cpal::InputCallbackInfo| {
                    capture_channel(data, in_ch, &channel_a, &prod, &over_a, Some(&wake_a));
                },
                // Terminal by cpal contract: this stream is finished (device removed, driver reset)
                // and its data callback stops firing — so latch the fault for the owner loop, which
                // drops the dead stream and tells JS. Log first, latch second: the flag is what the
                // owner acts on, so it must not be observable before the line that explains it.
                move |e: cpal::Error| {
                    log::warn!("[audio_input] cpal stream error: {e}");
                    fault_a.store(true, Relaxed);
                },
                None, // timeout: Option<Duration>
            )
        }};
    }

    let stream = match fmt {
        SampleFormat::F32 => build_stream!(f32),
        // I32 is the common ASIO sample format (Focusrite USB ASIO reports it); `f32::from_sample`
        // handles the int→float scaling. WASAPI-shared is usually F32, so this only fires under ASIO.
        SampleFormat::I32 => build_stream!(i32),
        SampleFormat::I16 => build_stream!(i16),
        SampleFormat::U16 => build_stream!(u16),
        other => return Err(format!("unsupported input sample format: {other:?}")),
    }
    .map_err(|e| format!("cpal build_input_stream: {e}"))?;

    // MANDATORY in 0.18 — streams return paused on every backend; without play() capture is silent.
    stream
        .play()
        .map_err(|e| format!("cpal stream.play: {e}"))?;
    Ok((stream, in_rate, channel_control))
}

/// P11.3 de-risk probe (DEV, `app.exe --probe-asio`). Enumerates the ASIO host's device list to prove
/// two things before any real ASIO work: (1) the cpal `asio` feature actually COMPILED (libclang/bindgen
/// + Steinberg SDK wired up), and (2) the machine's installed ASIO drivers (e.g. Focusrite/Scarlett)
/// are VISIBLE to cpal. NOTE: cpal's device enumeration DOES load and initialise each driver DLL
/// (`Devices::next` → `load_driver` → `CoCreateInstance` + `ASIOInit`), so this probe can hang or
/// crash on a broken driver exactly like the in-app probe; it is a DEV tool, not a harmless check.
#[cfg(feature = "asio")]
pub fn probe_asio() {
    println!("[asio-probe] available hosts: {:?}", cpal::available_hosts());
    let host = match cpal::host_from_id(cpal::HostId::Asio) {
        Ok(h) => h,
        Err(e) => {
            println!("[asio-probe] ASIO host UNAVAILABLE: {e}");
            return;
        }
    };
    println!("[asio-probe] ASIO host OK");
    // Does the host expose default in/out devices? (cpal ASIO sometimes lists only one direction.)
    println!(
        "[asio-probe] default_input_device: {}",
        host.default_input_device()
            .and_then(|d| d.description().ok().map(|x| x.to_string()))
            .unwrap_or_else(|| "None".to_string())
    );
    println!(
        "[asio-probe] default_output_device: {}",
        host.default_output_device()
            .and_then(|d| d.description().ok().map(|x| x.to_string()))
            .unwrap_or_else(|| "None".to_string())
    );
    // Per device: does it yield an INPUT config and/or an OUTPUT config? (Fresh — no stream open, so
    // this isolates "cpal can't enumerate ASIO output" from "the input stream holds the driver".)
    match host.devices() {
        Ok(devs) => {
            let mut n = 0;
            for d in devs {
                n += 1;
                let name = d
                    .description()
                    .map(|x| x.to_string())
                    .unwrap_or_else(|_| "?".to_string());
                let in_cfg = match d.default_input_config() {
                    Ok(c) => format!("{}ch {:?} {}Hz", c.channels(), c.sample_format(), c.sample_rate()),
                    Err(e) => format!("ERR({e})"),
                };
                let out_cfg = match d.default_output_config() {
                    Ok(c) => format!("{}ch {:?} {}Hz", c.channels(), c.sample_format(), c.sample_rate()),
                    Err(e) => format!("ERR({e})"),
                };
                println!("[asio-probe] device #{n}: \"{name}\" | in: {in_cfg} | out: {out_cfg}");
            }
            println!("[asio-probe] total ASIO devices: {n}");
        }
        Err(e) => println!("[asio-probe] host.devices() error: {e}"),
    }
}

/// Stub when built without the `asio` feature, so the `--probe-asio` dispatch always links.
#[cfg(not(feature = "asio"))]
pub fn probe_asio() {
    println!("[asio-probe] this build has no `asio` feature — rebuild with `--features asio`");
}

/// P11.3 ASIO de-risk #2 (`app.exe --probe-asio-duplex`). Builds an ASIO INPUT and an ASIO OUTPUT
/// stream on the same full-duplex device (Focusrite) BACK-TO-BACK and runs both ~3s, reporting whether
/// each built + how many callbacks fired. This isolates the core viability question for the ASIO tier:
/// BleepLoop arms capture + monitor as two INDEPENDENT cpal streams, but a single ASIO driver can't be
/// re-opened while one direction holds it (proven: `default_output_config` errs once the input stream
/// is live). If both streams build + tick here, sequential build works and the real path just needs to
/// avoid re-enumerating; if the 2nd build errs, ASIO needs a coupled single-duplex-stream redesign.
#[cfg(feature = "asio")]
pub fn probe_asio_duplex() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let host = match cpal::host_from_id(cpal::HostId::Asio) {
        Ok(h) => h,
        Err(e) => {
            println!("[asio-duplex] ASIO host UNAVAILABLE: {e}");
            return;
        }
    };
    let device = match host
        .default_output_device()
        .or_else(|| host.default_input_device())
    {
        Some(d) => d,
        None => {
            println!("[asio-duplex] no ASIO device");
            return;
        }
    };
    let in_cfg: StreamConfig = match device.default_input_config() {
        Ok(c) => c.config(),
        Err(e) => {
            println!("[asio-duplex] default_input_config err: {e}");
            return;
        }
    };
    let out_cfg: StreamConfig = match device.default_output_config() {
        Ok(c) => c.config(),
        Err(e) => {
            println!("[asio-duplex] default_output_config err: {e}");
            return;
        }
    };
    println!("[asio-duplex] in_cfg={in_cfg:?} out_cfg={out_cfg:?}");

    // ASIO sample format is I32 here (proven by --probe-asio).
    let in_cb = Arc::new(AtomicUsize::new(0));
    let out_cb = Arc::new(AtomicUsize::new(0));

    let ic = in_cb.clone();
    let in_stream = device.build_input_stream(
        in_cfg,
        move |_data: &[i32], _: &cpal::InputCallbackInfo| {
            ic.fetch_add(1, Ordering::Relaxed);
        },
        |e| println!("[asio-duplex] INPUT stream error: {e}"),
        None,
    );
    let in_stream = match in_stream {
        Ok(s) => {
            println!("[asio-duplex] INPUT stream built OK");
            s
        }
        Err(e) => {
            println!("[asio-duplex] INPUT build FAILED: {e}");
            return;
        }
    };
    // Mirror the REAL arm sequence: start the input stream FIRST (driver now live), THEN build the
    // output stream — using the config captured above while the driver was free (default_output_config
    // errs once the driver is held). This is the decisive test for a "cache the config" fix.
    if let Err(e) = in_stream.play() {
        println!("[asio-duplex] INPUT play err: {e}");
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    println!("[asio-duplex] input running; now building OUTPUT while input is LIVE…");

    let oc = out_cb.clone();
    let out_stream = device.build_output_stream(
        out_cfg,
        move |data: &mut [i32], _: &cpal::OutputCallbackInfo| {
            oc.fetch_add(1, Ordering::Relaxed);
            for s in data.iter_mut() {
                *s = 0; // silence
            }
        },
        |e| println!("[asio-duplex] OUTPUT stream error: {e}"),
        None,
    );
    let out_stream = match out_stream {
        Ok(s) => {
            println!("[asio-duplex] OUTPUT stream built OK (sequential build works!)");
            s
        }
        Err(e) => {
            println!("[asio-duplex] OUTPUT build FAILED: {e} — ASIO needs a coupled duplex redesign");
            return;
        }
    };

    if let Err(e) = out_stream.play() {
        println!("[asio-duplex] OUTPUT play err: {e}");
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    println!(
        "[asio-duplex] after 2s of duplex: input callbacks={}, output callbacks={}",
        in_cb.load(Ordering::Relaxed),
        out_cb.load(Ordering::Relaxed)
    );
    drop(in_stream);
    drop(out_stream);
    println!("[asio-duplex] done");
}

/// Stub when built without the `asio` feature, so the `--probe-asio-duplex` dispatch always links.
#[cfg(not(feature = "asio"))]
pub fn probe_asio_duplex() {
    println!("[asio-duplex] this build has no `asio` feature — rebuild with `--features asio`");
}
