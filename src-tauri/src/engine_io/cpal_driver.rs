//! OWNS: the real [`Driver`]: cpal streams on ASIO (the startup-cached duplex driver,
//! `audio_output::asio_cache`) and WASAPI (devices by id, `audio_output`/`audio_input`'s picks), running
//! the bodies of `callback.rs`; and Share output's mirror (`share.rs`).
//!
//! Every latency handed to a body is a delta within ONE stream's timestamps (input: callback − capture;
//! output: playback − callback): cpal ASIO instants are never compared across streams, each stream
//! having its own time base (`docs/plans/native-engine.md` § Stage 1).

use std::sync::Arc;
use std::time::Instant;

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

use super::callback::{Side, Tap};
use super::driver::{Driver, Mirror, Share, Spec, Started, Streams, Wiring};
use super::share::{ShareOutput, ShareTap};
use super::{Core, DeviceRequest, IoCounters};
use crate::audio_output::AudioBackend;

pub(crate) struct CpalDriver;

/// A resolved pair: the input and output devices with their stream configs.
pub(crate) struct CpalDevice {
    input: cpal::Device,
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
    fn start(&mut self, device: CpalDevice, spec: &Spec, mut wiring: Wiring) -> Result<Started, String> {
        let asio = spec.backend.is_asio();
        let input = retry_on_asio(asio, "input", || input_stream(&device, spec, &mut wiring))?;
        let output = retry_on_asio(asio, "output", || output_stream(&device, spec, &mut wiring))?;
        input.play().map_err(|e| format!("cpal input play: {e}"))?;
        output.play().map_err(|e| format!("cpal output play: {e}"))?;
        let block = output.buffer_size().unwrap_or(spec.block);
        Ok(Started { streams: Streams::new(Box::new(input), Box::new(output)), block })
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

fn input_stream(device: &CpalDevice, spec: &Spec, wiring: &mut Wiring) -> Result<cpal::Stream, String> {
    let capture = wiring.capture(spec)?;
    let on_error = wiring.on_error(Side::Input);
    macro_rules! build {
        ($T:ty) => {{
            let mut capture = capture;
            device.input.build_input_stream::<$T, _, _>(
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
        input: cache.device.clone(),
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

/// WASAPI shared mode: the picked (or default) endpoints at their mix formats; the period is the
/// audio engine's, so `DeviceRequest::buffer` does not apply.
fn resolve_wasapi(request: &DeviceRequest) -> Result<(Spec, CpalDevice), String> {
    let output = crate::audio_output::pick_output_device(request.output.as_deref())?;
    let (out_config, out_format) = crate::audio_output::output_config(&output)?;
    let input = crate::audio_input::pick_input_device(request.input.as_deref())?;
    let supported = input.default_input_config().map_err(|e| format!("cpal default_input_config: {e}"))?;
    let in_config = StreamConfig { channels: supported.channels(), sample_rate: supported.sample_rate(), buffer_size: cpal::BufferSize::Default };
    if in_config.sample_rate == 0 || out_config.sample_rate == 0 {
        return Err("a WASAPI endpoint reports a zero sample rate".to_string());
    }
    let name = |d: &cpal::Device, fallback: &str| d.description().map(|x| x.to_string()).unwrap_or_else(|_| fallback.to_string());
    let spec = Spec {
        backend: AudioBackend::Wasapi,
        rate: out_config.sample_rate,
        in_rate: in_config.sample_rate,
        in_channels: in_config.channels as usize,
        out_channels: out_config.channels as usize,
        block: 0,
        input_name: name(&input, "Unknown input"),
        output_name: name(&output, "Unknown output"),
    };
    Ok((spec, CpalDevice { input, in_config, in_format: supported.sample_format(), output, out_config, out_format }))
}
