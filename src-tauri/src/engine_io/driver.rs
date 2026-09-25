//! OWNS: the seam the device owner opens streams through ([`Driver`]) and what crosses it: a resolved
//! device ([`Spec`]), the callback bodies a pair of streams runs ([`Wiring`]), the running streams
//! ([`Streams`]) and Share output's mirror ([`Mirror`]). `cpal_driver` is the real driver; the tests'
//! `fake_driver` runs the same bodies from a thread, with no hardware.

use std::sync::Arc;

use super::callback::{Capture, DuplexInput, JoinInput, Render, Run, Side, Tap};
use super::pipes::{PullPipe, PushEnd};
use super::{Core, DeviceRequest};
use crate::audio_output::AudioBackend;

/// A device resolved for opening: what the owner builds the engine and the bodies for.
#[derive(Clone, Debug)]
pub(crate) struct Spec {
    pub(crate) backend: AudioBackend,
    /// The output's rate: the engine's.
    pub(crate) rate: u32,
    /// The input's rate (ASIO: one driver, the same rate; WASAPI: the join resamples it).
    pub(crate) in_rate: u32,
    pub(crate) in_channels: usize,
    pub(crate) out_channels: usize,
    /// Frames per callback asked of the driver (ASIO `DeviceRequest::buffer`), 0 = the driver's own.
    pub(crate) block: u32,
    pub(crate) input_name: String,
    pub(crate) output_name: String,
}

/// Running streams. Dropping them stops the device: the output first, then the input.
pub(crate) struct Streams {
    output: Option<Box<dyn Send>>,
    input: Option<Box<dyn Send>>,
}

impl Streams {
    pub(crate) fn new(input: Box<dyn Send>, output: Box<dyn Send>) -> Streams {
        Streams { output: Some(output), input: Some(input) }
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        drop(self.output.take());
        drop(self.input.take());
    }
}

/// A started device: its streams and the frames per output callback the driver settled on.
pub(crate) struct Started {
    pub(crate) streams: Streams,
    pub(crate) block: u32,
}

/// The callback bodies for one run. ASIO's are cheap and built fresh for each stream build (a failed
/// ASIO build is retried once); WASAPI's own the join pipe's two ends, so they are built once.
pub(crate) struct Wiring {
    pub(crate) core: Arc<Core>,
    pub(crate) run: Arc<Run>,
    push: Option<PushEnd>,
    pull: Option<PullPipe>,
}

impl Wiring {
    /// `join`: WASAPI's pipe (input → engine); `None` on ASIO (the same-cycle handoff).
    pub(crate) fn new(core: Arc<Core>, run: Arc<Run>, join: Option<(PushEnd, PullPipe)>) -> Wiring {
        let (push, pull) = match join {
            Some((push, pull)) => (Some(push), Some(pull)),
            None => (None, None),
        };
        Wiring { core, run, push, pull }
    }

    /// The input callback's body for `spec`'s input.
    pub(crate) fn capture(&mut self, spec: &Spec) -> Result<Capture, String> {
        let (core, run) = (self.core.clone(), self.run.clone());
        if spec.backend.is_asio() {
            return Ok(Capture::Duplex(DuplexInput::new(core, run, spec.in_channels, spec.rate)));
        }
        let push = self.push.take().ok_or("the join pipe's input end is already wired")?;
        Ok(Capture::Join(JoinInput::new(core, run, spec.in_channels, spec.rate, push)))
    }

    /// The output callback's body for `spec`'s output.
    pub(crate) fn render(&mut self, spec: &Spec) -> Result<Render, String> {
        let (core, run) = (self.core.clone(), self.run.clone());
        if spec.backend.is_asio() {
            return Ok(Render::duplex(core, run, spec.out_channels, spec.rate));
        }
        let pull = self.pull.take().ok_or("the join pipe's output end is already wired")?;
        Ok(Render::join(core, run, spec.out_channels, spec.rate, pull))
    }

    /// A stream's error callback.
    pub(crate) fn on_error(&self, side: Side) -> impl FnMut(cpal::Error) + Send + 'static {
        let (core, run) = (self.core.clone(), self.run.clone());
        move |error| run.stream_error(side, error, &core.counters)
    }
}

/// Share output's mirror stream, owned by the device owner (`share::ShareOutput`; a fake in tests).
pub(crate) trait Mirror {
    /// Its stream died: the owner drops the mirror and reports it.
    fn faulted(&self) -> bool;
}

/// An open Share output: the mirror the owner keeps and the tap the callback feeds.
pub(crate) type Share = (Box<dyn Mirror>, Box<dyn Tap>);

/// How the device owner reaches devices. Called on the owner thread only.
pub(crate) trait Driver: Send + 'static {
    /// What `resolve` found, handed back to `start`.
    type Device;

    /// Find the device `request` names and its stream configs; opens no stream, so a failure leaves
    /// the running device alone.
    fn resolve(&mut self, request: &DeviceRequest) -> Result<(Spec, Self::Device), String>;

    /// Build and play the streams, the input first, then the output (ASIO runs its registered
    /// callbacks in that order in one bufferSwitch), each running `wiring`'s bodies.
    fn start(&mut self, device: Self::Device, spec: &Spec, wiring: Wiring) -> Result<Started, String>;

    /// Open Share output's mirror on the WASAPI render `endpoint`, for an engine at `rate` rendering
    /// `block`-frame blocks: the mirror and the tap the callback feeds.
    fn open_share(&mut self, endpoint: &str, rate: u32, block: u32, core: &Arc<Core>) -> Result<Share, String>;
}
