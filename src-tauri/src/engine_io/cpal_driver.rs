//! OWNS: the real [`Driver`]: cpal streams on ASIO (the startup-cached duplex driver,
//! `audio_output::asio_cache`) and WASAPI (devices by id, `audio_output`/`audio_input`'s picks), running
//! the bodies of `callback.rs`; and Share output's mirror (`share.rs`).
//!
//! An ASIO run builds on a cpal device of its own, found again by the cached driver's name (the cached
//! one would hand a new run the last run's stream state), and first opens the driver at another block
//! size and destroys it (`preopen`): opened again at the size it last ran, the rig's driver delivers
//! about two periods later than it reports (`docs/plans/native-engine.md` § Stage 1, "Cause and fix").
//! A cpal ASIO driver lives as long as a stream holds it: dropping a run's streams stops it, disposes
//! its buffers and exits it (asio-sys's `Driver` drop).
//!
//! Every latency handed to a body is a delta within ONE stream's timestamps (input: callback − capture;
//! output: playback − callback): cpal ASIO instants are never compared across streams, each stream
//! having its own time base (`docs/plans/native-engine.md` § Stage 1).

use std::sync::Arc;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

use super::callback::{Side, Tap};
use super::driver::{Driver, Mirror, Share, Spec, Started, Streams, Wiring};
use super::share::{ShareOutput, ShareTap};
use super::{Core, DeviceRequest, IoCounters};
use crate::audio_output::AudioBackend;

pub(crate) struct CpalDriver {
    /// ASIO: open the driver at another block size before each run (`preopen`). Off only for the
    /// engine probe's before-and-after comparison (`--no-preopen`).
    pub(crate) preopen: bool,
}

/// How long the preopen runs the driver at the other block size (the Stage 1 fix measured with this).
const PREOPEN_RUN: Duration = Duration::from_millis(300);

/// A resolved pair: the input and output devices with their stream configs. No input: WASAPI output
/// only (no capture endpoint), the engine's input silent.
pub(crate) struct CpalDevice {
    input: Option<cpal::Device>,
    in_config: StreamConfig,
    in_format: SampleFormat,
    output: cpal::Device,
    out_config: StreamConfig,
    out_format: SampleFormat,
}

impl Driver for CpalDriver {
    type Device = CpalDevice;

    fn resolve(&mut self, request: &DeviceRequest) -> Result<(Spec, CpalDevice), String> {
        match request.backend {
            AudioBackend::Asio => resolve_asio(request),
            AudioBackend::Wasapi => resolve_wasapi(request),
        }
    }

    /// Both builds before either plays. A cpal ASIO build starts the driver and a playing stream's
    /// callback takes cpal's `asio_streams` mutex; the output build holds that mutex while it re-creates
    /// the buffers (`ASIOStop` first), so a playing input could block a bufferSwitch the stop waits on.
    /// A paused stream's callback returns before the mutex.
    ///
    /// WASAPI plays without its input when the capture stream does not build or play (a microphone
    /// Windows' privacy settings block): the engine's input is silence and the status says so. ASIO's
    /// duplex needs both.
    fn start(&mut self, device: CpalDevice, spec: &Spec, mut wiring: Wiring) -> Result<Started, String> {
        let asio = spec.backend.is_asio();
        let device = if asio { asio_run(device, spec, self.preopen)? } else { device };
        let input = match device.input.as_ref() {
            Some(input) => match retry_on_asio(asio, "input", || input_stream(input, &device, spec, &mut wiring)) {
                Ok(stream) => Some(stream),
                Err(error) if !asio => {
                    log::warn!("[engine_io] the WASAPI input did not open ({error}): output only");
                    None
                }
                Err(error) => return Err(error),
            },
            None => None,
        };
        let output = retry_on_asio(asio, "output", || output_stream(&device, spec, &mut wiring))?;
        let input = match input.map(|stream| stream.play().map(|()| stream)) {
            Some(Err(error)) if !asio => {
                log::warn!("[engine_io] the WASAPI input did not play ({error}): output only");
                None
            }
            Some(played) => Some(played.map_err(|e| format!("cpal input play: {e}"))?),
            None => None,
        };
        output.play().map_err(|e| format!("cpal output play: {e}"))?;
        let block = output.buffer_size().unwrap_or(spec.block);
        let input_open = input.is_some();
        let input = input.map(|stream| Box::new(stream) as Box<dyn Send>);
        Ok(Started { streams: Streams::new(input, Box::new(output)), block, input_open })
    }

    fn open_share(&mut self, endpoint: &str, rate: u32, block: u32, core: &Arc<Core>) -> Result<Share, String> {
        let (mirror, tap) = ShareOutput::open(endpoint, rate, block as lf_engine::grid::Frame, core.clone())?;
        Ok((Box::new(mirror), Box::new(tap)))
    }
}

impl Tap for ShareTap {
    fn push(&mut self, left: &[f32], right: &[f32], counters: &IoCounters) {
        ShareTap::push(self, left, right, counters);
    }
}

impl Mirror for ShareOutput {
    fn faulted(&self) -> bool {
        ShareOutput::faulted(self)
    }
}

/// Build once more after a failed ASIO build: a driver left running by the previous run's streams
/// (dropping an ASIO stream only removes its callback) makes cpal's reuse path fail with BadMode, and the
/// failed build resets it, so the second build takes the prepare path (as `audio_output` does).
fn retry_on_asio(asio: bool, what: &str, mut build: impl FnMut() -> Result<cpal::Stream, String>) -> Result<cpal::Stream, String> {
    match build() {
        Err(first) if asio => {
            log::warn!("[engine_io] ASIO {what} build failed ({first}); retrying once");
            build()
        }
        result => result,
    }
}

fn input_stream(input: &cpal::Device, device: &CpalDevice, spec: &Spec, wiring: &mut Wiring) -> Result<cpal::Stream, String> {
    let capture = wiring.capture(spec)?;
    let on_error = wiring.on_error(Side::Input);
    macro_rules! build {
        ($T:ty) => {{
            let mut capture = capture;
            input.build_input_stream::<$T, _, _>(
                device.in_config,
                move |data: &[$T], info: &cpal::InputCallbackInfo| {
                    let t = info.timestamp();
                    capture.capture(data, t.callback.checked_duration_since(t.capture));
                },
                on_error,
                None,
            )
        }};
    }
    match device.in_format {
        SampleFormat::F32 => build!(f32),
        // ASIO drivers commonly deliver I32 (Focusrite's does); WASAPI shared mode is usually F32.
        SampleFormat::I32 => build!(i32),
        SampleFormat::I16 => build!(i16),
        SampleFormat::U16 => build!(u16),
        other => return Err(format!("unsupported input sample format: {other:?}")),
    }
    .map_err(|e| format!("cpal build_input_stream: {e}"))
}

fn output_stream(device: &CpalDevice, spec: &Spec, wiring: &mut Wiring) -> Result<cpal::Stream, String> {
    let render = wiring.render(spec)?;
    let on_error = wiring.on_error(Side::Output);
    macro_rules! build {
        ($T:ty) => {{
            let mut render = render;
            device.output.build_output_stream::<$T, _, _>(
                device.out_config,
                move |data: &mut [$T], info: &cpal::OutputCallbackInfo| {
                    let entry = Instant::now();
                    let t = info.timestamp();
                    render.render(data, entry, t.playback.checked_duration_since(t.callback));
                },
                on_error,
                None,
            )
        }};
    }
    match device.out_format {
        SampleFormat::F32 => build!(f32),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I16 => build!(i16),
        SampleFormat::U16 => build!(u16),
        other => return Err(format!("unsupported output sample format: {other:?}")),
    }
    .map_err(|e| format!("cpal build_output_stream: {e}"))
}

/// ASIO: the cached duplex driver, both directions on it; `DeviceRequest::buffer` becomes a fixed
/// buffer size.
#[cfg(feature = "asio")]
fn resolve_asio(request: &DeviceRequest) -> Result<(Spec, CpalDevice), String> {
    let cache = crate::audio_output::asio_cache().ok_or("no ASIO driver is cached (the startup probe has not found one)")?;
    let (mut in_config, mut out_config) = (cache.in_cfg, cache.out_cfg);
    if let Some(frames) = request.buffer {
        in_config.buffer_size = cpal::BufferSize::Fixed(frames);
        out_config.buffer_size = cpal::BufferSize::Fixed(frames);
    }
    if in_config.sample_rate != out_config.sample_rate {
        return Err(format!("the ASIO driver reports input {} Hz and output {} Hz", in_config.sample_rate, out_config.sample_rate));
    }
    let spec = Spec {
        backend: AudioBackend::Asio,
        rate: out_config.sample_rate,
        in_rate: in_config.sample_rate,
        in_channels: in_config.channels as usize,
        out_channels: out_config.channels as usize,
        block: request.buffer.unwrap_or(0),
        input_name: cache.name.clone(),
        output_name: cache.name.clone(),
    };
    let device = CpalDevice {
        input: Some(cache.device.clone()),
        in_config,
        in_format: cache.in_fmt,
        output: cache.device.clone(),
        out_config,
        out_format: cache.out_fmt,
    };
    Ok((spec, device))
}

#[cfg(not(feature = "asio"))]
fn resolve_asio(_: &DeviceRequest) -> Result<(Spec, CpalDevice), String> {
    Err("this build has no ASIO support".to_string())
}

/// ASIO: the device a run builds on, its driver opened at another block size and destroyed first when
/// the run asks for a size (`preopen`; a preopen that fails is logged and the run goes on).
#[cfg(feature = "asio")]
fn asio_run(device: CpalDevice, spec: &Spec, preopen: bool) -> Result<CpalDevice, String> {
    if preopen && spec.block > 0 {
        if let Err(error) = preopen_asio(&device, spec) {
            log::warn!("[engine_io] the ASIO preopen failed ({error}); opening at {} frames anyway", spec.block);
        }
    }
    let fresh = find_asio(&spec.output_name)?;
    Ok(CpalDevice { input: Some(fresh.clone()), output: fresh, ..device })
}

#[cfg(not(feature = "asio"))]
fn asio_run(_: CpalDevice, _: &Spec, _: bool) -> Result<CpalDevice, String> {
    Err("this build has no ASIO support".to_string())
}

/// The cached ASIO driver as a new cpal device, with no stream state: found by name, which loads each
/// driver listed before it once (as the startup probe did) and this one, then exits it again.
#[cfg(feature = "asio")]
fn find_asio(name: &str) -> Result<cpal::Device, String> {
    use cpal::traits::HostTrait;
    let host = cpal::host_from_id(cpal::HostId::Asio).map_err(|e| format!("ASIO host unavailable ({e})"))?;
    let mut devices = host.devices().map_err(|e| format!("ASIO devices: {e}"))?;
    devices
        .find(|d| d.description().is_ok_and(|x| x.to_string() == name))
        .ok_or_else(|| format!("the ASIO driver \"{name}\" did not load"))
}

/// Open the driver at a block size other than the run's, play it silent for `PREOPEN_RUN` and drop it
/// (both streams, then the device): the driver is stopped, its buffers disposed and it exits, so the
/// run's open is its first at the run's size.
#[cfg(feature = "asio")]
fn preopen_asio(device: &CpalDevice, spec: &Spec) -> Result<(), String> {
    let began = Instant::now();
    let other = other_block(device, spec.block);
    let fresh = find_asio(&spec.output_name)?;
    let (mut in_config, mut out_config) = (device.in_config, device.out_config);
    in_config.buffer_size = cpal::BufferSize::Fixed(other);
    out_config.buffer_size = cpal::BufferSize::Fixed(other);
    // Both built before either plays, as a run is (`start`); an ASIO sample format's silence is zero bytes.
    let input = fresh
        .build_input_stream_raw(in_config, device.in_format, |_: &cpal::Data, _: &cpal::InputCallbackInfo| {}, |_| {}, None)
        .map_err(|e| format!("preopen input: {e}"))?;
    let output = fresh
        .build_output_stream_raw(out_config, device.out_format, |data: &mut cpal::Data, _: &cpal::OutputCallbackInfo| data.bytes_mut().fill(0), |_| {}, None)
        .map_err(|e| format!("preopen output: {e}"))?;
    input.play().map_err(|e| format!("preopen input play: {e}"))?;
    output.play().map_err(|e| format!("preopen output play: {e}"))?;
    std::thread::sleep(PREOPEN_RUN);
    drop(output);
    drop(input);
    drop(fresh);
    log::info!("[engine_io] ASIO preopen at {other} frames before the {}-frame open: {} ms", spec.block, began.elapsed().as_millis());
    Ok(())
}

/// A block size the driver takes that is not `block`.
#[cfg(feature = "asio")]
fn other_block(device: &CpalDevice, block: u32) -> u32 {
    let (min, max) = match device.output.default_output_config().map(|c| *c.buffer_size()) {
        Ok(cpal::SupportedBufferSize::Range { min, max }) => (min, max),
        _ => (0, u32::MAX),
    };
    [256, 128, 512, 64, 1024].into_iter().find(|&b| b != block && (min..=max).contains(&b)).unwrap_or(if block == 256 { 128 } else { 256 })
}

/// WASAPI shared mode: the picked (or default) endpoints at their mix formats; the period is the
/// audio engine's, so `DeviceRequest::buffer` does not apply. With no default capture endpoint (or one
/// that reports no format) it plays output only; a picked input that is gone is an error.
fn resolve_wasapi(request: &DeviceRequest) -> Result<(Spec, CpalDevice), String> {
    let output = crate::audio_output::pick_output_device(request.output.as_deref())?;
    let (out_config, out_format) = crate::audio_output::output_config(&output)?;
    let input = crate::audio_input::pick_input_device(request.input.as_deref()).and_then(|input| {
        let supported = input.default_input_config().map_err(|e| format!("cpal default_input_config: {e}"))?;
        Ok((input, supported))
    });
    let input = match input {
        Ok(found) => Some(found),
        Err(error) if request.input.is_none() => {
            log::warn!("[engine_io] no WASAPI capture ({error}): output only");
            None
        }
        Err(error) => return Err(error),
    };
    let in_config = match &input {
        Some((_, supported)) => StreamConfig { channels: supported.channels(), sample_rate: supported.sample_rate(), buffer_size: cpal::BufferSize::Default },
        None => out_config,
    };
    if in_config.sample_rate == 0 || out_config.sample_rate == 0 {
        return Err("a WASAPI endpoint reports a zero sample rate".to_string());
    }
    let name = |d: &cpal::Device, fallback: &str| d.description().map(|x| x.to_string()).unwrap_or_else(|_| fallback.to_string());
    let spec = Spec {
        backend: AudioBackend::Wasapi,
        rate: out_config.sample_rate,
        in_rate: in_config.sample_rate,
        in_channels: if input.is_some() { in_config.channels as usize } else { 0 },
        out_channels: out_config.channels as usize,
        block: 0,
        input_name: input.as_ref().map_or_else(String::new, |(d, _)| name(d, "Unknown input")),
        output_name: name(&output, "Unknown output"),
    };
    let in_format = input.as_ref().map_or(out_format, |(_, supported)| supported.sample_format());
    Ok((spec, CpalDevice { input: input.map(|(d, _)| d), in_config, in_format, output, out_config, out_format }))
}
