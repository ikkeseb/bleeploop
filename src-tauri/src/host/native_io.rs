//! Format-agnostic owner-thread state for native capture and wet monitoring.
//!
//! CLAP and VST3 have different plugin lifecycles, but their cpal/ASIO arm, disarm, fault and
//! diagnostic paths are identical. Keeping those paths here means backend ownership has one source
//! of truth and fixes cannot drift between the two formats.

use super::transport::ProducerDiag;
use crate::audio_output::{self, AudioBackend};

use std::sync::atomic::{
    AtomicBool, AtomicU32, AtomicU64,
    Ordering::{Relaxed, Release},
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer};
use tauri::Emitter;

/// Long enough for one large WASAPI period plus the ~10 ms declick envelope, but bounded so a dead
/// callback cannot hang the owner thread.
const MONITOR_FADE_WAIT_MS: u64 = 60;
/// Bound the fresh-output wait for an initial valid three-report device-latency window.
const MONITOR_OUT_BLOCK_WAIT_MS: u64 = 60;

struct HeldStream {
    backend: AudioBackend,
    _stream: cpal::Stream,
    rate: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransitionAction {
    None,
    DropRetained,
    Blocked,
}

fn transition_action(
    input: Option<AudioBackend>,
    monitor: Option<AudioBackend>,
    requested: AudioBackend,
    input_armed: bool,
    monitor_armed: bool,
) -> TransitionAction {
    let changes_backend =
        input.is_some_and(|b| b != requested) || monitor.is_some_and(|b| b != requested);
    if !changes_backend {
        TransitionAction::None
    } else if input_armed || monitor_armed {
        TransitionAction::Blocked
    } else {
        TransitionAction::DropRetained
    }
}

/// The concrete native-I/O owner shared by CLAP and VST3. It lives only on the plugin owner thread;
/// the `cpal::Stream`s are deliberately never made Send or hidden behind a plugin-backend trait.
pub(super) struct NativeIo {
    slot: u8,
    _format: &'static str,
    /// Tells the frontend a stream died (`plugin:stream-fault`, kind "input" | "output").
    fault_sink: Box<dyn Fn(&'static str)>,
    diag: Arc<ProducerDiag>,
    has_input: bool,
    input_producer: Arc<Mutex<Producer<f32>>>,
    monitor_consumer: Arc<Mutex<Consumer<f32>>>,
    monitor_gain: Arc<AtomicU32>,
    input_overruns: Arc<AtomicU64>,
    monitor_starves: Arc<AtomicU64>,
    monitor_out_block: Arc<AtomicU32>,
    monitor_output_latency_ns: Arc<AtomicU64>,
    monitor_fade: Arc<AtomicU32>,
    monitor_faded: Arc<AtomicBool>,
    input_fault: Arc<AtomicBool>,
    monitor_fault: Arc<AtomicBool>,
    input_stream: Option<HeldStream>,
    input_channel: Option<Arc<crate::audio_input::InputChannelControl>>,
    monitor_stream: Option<HeldStream>,
    input_armed: bool,
    monitor_armed: bool,
}

impl NativeIo {
    pub(super) fn new(
        slot: u8,
        format: &'static str,
        window: tauri::WebviewWindow,
        diag: Arc<ProducerDiag>,
        has_input: bool,
        input_producer: Arc<Mutex<Producer<f32>>>,
        monitor_consumer: Consumer<f32>,
        monitor_gain: Arc<AtomicU32>,
    ) -> Self {
        let fault_sink = Box::new(move |kind: &'static str| {
            let _ = window.emit(
                "plugin:stream-fault",
                serde_json::json!({ "slot": slot, "kind": kind }),
            );
        });
        Self::with_fault_sink(
            slot,
            format,
            fault_sink,
            diag,
            has_input,
            input_producer,
            monitor_consumer,
            monitor_gain,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_fault_sink(
        slot: u8,
        format: &'static str,
        fault_sink: Box<dyn Fn(&'static str)>,
        diag: Arc<ProducerDiag>,
        has_input: bool,
        input_producer: Arc<Mutex<Producer<f32>>>,
        monitor_consumer: Consumer<f32>,
        monitor_gain: Arc<AtomicU32>,
    ) -> Self {
        Self {
            slot,
            _format: format,
            fault_sink,
            diag,
            has_input,
            input_producer,
            monitor_consumer: Arc::new(Mutex::new(monitor_consumer)),
            monitor_gain,
            input_overruns: Arc::new(AtomicU64::new(0)),
            monitor_starves: Arc::new(AtomicU64::new(0)),
            monitor_out_block: Arc::new(AtomicU32::new(0)),
            monitor_output_latency_ns: Arc::new(AtomicU64::new(0)),
            monitor_fade: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            monitor_faded: Arc::new(AtomicBool::new(false)),
            input_fault: Arc::new(AtomicBool::new(false)),
            monitor_fault: Arc::new(AtomicBool::new(false)),
            input_stream: None,
            input_channel: None,
            monitor_stream: None,
            input_armed: false,
            monitor_armed: false,
        }
    }

    fn input_backend(&self) -> Option<AudioBackend> {
        self.input_stream.as_ref().map(|s| s.backend)
    }

    fn monitor_backend(&self) -> Option<AudioBackend> {
        self.monitor_stream.as_ref().map(|s| s.backend)
    }

    fn prepare_backend(&mut self, requested: AudioBackend) -> Result<(), String> {
        let input_backend = self.input_backend();
        let monitor_backend = self.monitor_backend();
        match transition_action(
            input_backend,
            monitor_backend,
            requested,
            self.input_armed,
            self.monitor_armed,
        ) {
            TransitionAction::None => Ok(()),
            TransitionAction::Blocked => Err(
                "audio backend changed — disarm both plugin input and monitor before re-arming"
                    .to_string(),
            ),
            TransitionAction::DropRetained => {
                log::info!(
                    "[plugin_host] slot {} native I/O backend transition {:?}/{:?} → {:?}",
                    self.slot,
                    input_backend,
                    monitor_backend,
                    requested
                );
                // ASIO input and output are one coordinated driver lifetime. Drop BOTH retained
                // directions before releasing the holder; dropping only the direction being rebuilt
                // leaves a live ASIO stream behind while another slot is allowed to claim the driver.
                self.input_stream = None;
                self.input_channel = None;
                self.monitor_stream = None;
                self.input_fault.store(false, Relaxed);
                self.monitor_fault.store(false, Relaxed);
                self.monitor_out_block.store(0, Relaxed);
                self.monitor_output_latency_ns.store(0, Relaxed);
                self.release_asio_if_unused();
                Ok(())
            }
        }
    }

    fn has_asio_stream(&self) -> bool {
        self.input_backend().is_some_and(AudioBackend::is_asio)
            || self.monitor_backend().is_some_and(AudioBackend::is_asio)
    }

    fn release_asio_if_unused(&self) {
        if !self.has_asio_stream() {
            audio_output::release_asio_holder(self.slot);
        }
    }

    fn publish_input(&self, rate: u32, backend: AudioBackend) {
        self.diag.input_is_asio.store(backend.is_asio(), Relaxed);
        self.diag.input_rate.store(rate, Relaxed);
        self.diag.input_gen.fetch_add(1, Release);
    }

    fn publish_monitor(&self, rate: u32, backend: AudioBackend) {
        self.diag.monitor_is_asio.store(backend.is_asio(), Relaxed);
        self.diag.monitor_rate.store(rate, Relaxed);
        self.diag.monitor_gen.fetch_add(1, Release);
    }

    fn drain_monitor(&self) {
        if let Ok(mut consumer) = self.monitor_consumer.lock() {
            while consumer.pop().is_ok() {}
        }
    }

    pub(super) fn arm_input(
        &mut self,
        device_id: Option<&str>,
        channel: Option<u32>,
        cancelled: &AtomicBool,
    ) -> Result<(), String> {
        if !self.has_input {
            return Err("plugin has no audio input bus (a synth cannot be armed)".to_string());
        }

        let backend = AudioBackend::selected();
        self.prepare_backend(backend)?;

        let result = if backend.is_asio() && self.input_backend() == Some(backend) {
            // ASIO keep-alive re-arm: the callback kept filling the ring while the RT input pipe was
            // absent. Republish its captured rate; the generation bump makes RT flush and rebuild.
            // Channel choice lives in the callback, so changing it needs no ASIO stream rebuild.
            // select waits for any old-channel writer before the generation bump flushes its PCM.
            self.input_channel.as_ref().expect("retained input has channel control")
                .select(channel, &self.input_producer)?;
            let rate = self.input_stream.as_ref().expect("backend checked").rate;
            self.publish_input(rate, backend);
            self.input_armed = true;
            Ok(())
        } else if backend.is_asio() && !audio_output::try_acquire_asio_holder(self.slot) {
            Err("ASIO is in use by the other slot — unload its plugin to free it".to_string())
        } else {
            self.input_stream = None;
            self.input_channel = None;
            self.input_fault.store(false, Relaxed);
            match crate::audio_input::open_input_stream(
                backend,
                device_id,
                channel,
                self.input_producer.clone(),
                self.input_overruns.clone(),
                self.input_fault.clone(),
            ) {
                Ok((stream, rate, channel_control)) => {
                    self.input_channel = Some(channel_control);
                    self.input_stream = Some(HeldStream {
                        backend,
                        _stream: stream,
                        rate,
                    });
                    self.publish_input(rate, backend);
                    self.input_armed = true;
                    Ok(())
                }
                Err(error) => {
                    // The old stream is already gone: publish "no stream" so RT and the readers
                    // stop using its rate (audit B7).
                    self.publish_input(0, backend);
                    self.input_armed = false;
                    self.release_asio_if_unused();
                    Err(error)
                }
            }
        };

        if result.is_ok() && cancelled.load(Relaxed) {
            log::warn!("[plugin_host] slot {} armInput: owner-request cancelled mid-call (caller timed out) — rolling back, disarming the capture stream", self.slot);
            self.disarm_input_inner();
        }
        result
    }

    fn disarm_input_inner(&mut self) {
        self.publish_input(0, self.input_backend().unwrap_or(AudioBackend::Wasapi));
        self.input_armed = false;
        if self.input_backend() != Some(AudioBackend::Asio) {
            self.input_stream = None;
            self.input_channel = None;
        }
        self.release_asio_if_unused();
    }

    pub(super) fn disarm_input(&mut self) -> Result<(), String> {
        self.disarm_input_inner();
        Ok(())
    }

    pub(super) fn arm_monitor(
        &mut self,
        device_id: Option<&str>,
        cancelled: &AtomicBool,
    ) -> Result<(), String> {
        let backend = AudioBackend::selected();
        self.prepare_backend(backend)?;

        let result = if backend.is_asio() && self.monitor_backend() == Some(backend) {
            self.drain_monitor();
            self.monitor_fade.store(1.0f32.to_bits(), Relaxed);
            self.monitor_faded.store(false, Relaxed);
            let rate = self.monitor_stream.as_ref().expect("backend checked").rate;
            self.publish_monitor(rate, backend);
            self.monitor_armed = true;
            Ok(())
        } else if backend.is_asio() && !audio_output::try_acquire_asio_holder(self.slot) {
            Err("ASIO is in use by the other slot — unload its plugin to free it".to_string())
        } else {
            self.monitor_stream = None;
            self.monitor_fault.store(false, Relaxed);
            self.monitor_out_block.store(0, Relaxed);
            self.monitor_output_latency_ns.store(0, Relaxed);
            self.drain_monitor();
            self.monitor_fade.store(1.0f32.to_bits(), Relaxed);
            self.monitor_faded.store(false, Relaxed);
            match audio_output::open_output_stream(
                backend,
                device_id,
                self.monitor_consumer.clone(),
                self.monitor_gain.clone(),
                self.monitor_starves.clone(),
                self.monitor_fade.clone(),
                self.monitor_faded.clone(),
                self.monitor_out_block.clone(),
                self.monitor_output_latency_ns.clone(),
                self.monitor_fault.clone(),
            ) {
                Ok((stream, rate)) => {
                    self.monitor_stream = Some(HeldStream {
                        backend,
                        _stream: stream,
                        rate,
                    });
                    self.publish_monitor(rate, backend);
                    self.monitor_armed = true;
                    Ok(())
                }
                Err(error) => {
                    // The old stream is already gone: publish "no stream" so RT and the latency
                    // readout stop using its rate (audit B7).
                    self.publish_monitor(0, backend);
                    self.monitor_armed = false;
                    self.release_asio_if_unused();
                    Err(error)
                }
            }
        };

        let rolled_back = result.is_ok() && cancelled.load(Relaxed);
        if rolled_back {
            log::warn!("[plugin_host] slot {} armMonitor: owner-request cancelled mid-call (caller timed out) — rolling back, disarming the native monitor", self.slot);
            self.disarm_monitor_inner(false);
        } else if result.is_ok() {
            let deadline = Instant::now() + Duration::from_millis(MONITOR_OUT_BLOCK_WAIT_MS);
            while (self.monitor_out_block.load(Relaxed) == 0 || self.monitor_output_latency_ns.load(Relaxed) == 0)
                && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            // A slow device open can consume most of the caller's timeout. The timestamp wait
            // must not create a new window where cancellation leaves a native monitor armed.
            if cancelled.load(Relaxed) {
                self.disarm_monitor_inner(false);
            }
            self.diag
                .monitor_out_block
                .store(self.monitor_out_block.load(Relaxed), Relaxed);
            self.diag.monitor_output_latency_ns.store(self.monitor_output_latency_ns.load(Relaxed), Relaxed);
        }
        result
    }

    fn disarm_monitor_inner(&mut self, _warn_on_timeout: bool) {
        if self.monitor_stream.is_some() {
            self.monitor_faded.store(false, Relaxed);
            self.monitor_fade.store(0.0f32.to_bits(), Relaxed);
            let deadline = Instant::now() + Duration::from_millis(MONITOR_FADE_WAIT_MS);
            while !self.monitor_faded.load(Relaxed) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            #[cfg(debug_assertions)]
            if _warn_on_timeout && !self.monitor_faded.load(Relaxed) {
                log::warn!("[plugin_host] {} monitor declick timed out ({}ms) — hard-stop fallback (possible click)", self._format, MONITOR_FADE_WAIT_MS);
            }
        }

        self.publish_monitor(0, self.monitor_backend().unwrap_or(AudioBackend::Wasapi));
        self.monitor_armed = false;
        self.drain_monitor();
        if self.monitor_backend() != Some(AudioBackend::Asio) {
            self.monitor_stream = None;
        }
        self.release_asio_if_unused();
    }

    pub(super) fn disarm_monitor(&mut self) -> Result<(), String> {
        self.disarm_monitor_inner(true);
        Ok(())
    }

    /// Consume terminal cpal latches. The dead handle is always dropped, including ASIO, and the
    /// frontend is told after native state already reflects the loss.
    pub(super) fn poll_faults(&mut self) {
        if self.input_fault.swap(false, Relaxed) && self.input_stream.is_some() {
            log::error!("[plugin_host] slot {} native capture stream FAULTED (device lost) — dropping it; the slot is no longer live", self.slot);
            self.publish_input(0, self.input_backend().unwrap_or(AudioBackend::Wasapi));
            self.input_armed = false;
            self.input_stream = None;
            self.input_channel = None;
            self.release_asio_if_unused();
            (self.fault_sink)("input");
        }

        if self.monitor_fault.swap(false, Relaxed) && self.monitor_stream.is_some() {
            log::error!("[plugin_host] slot {} native monitor stream FAULTED (device lost) — dropping it; JS falls back to the web monitor path", self.slot);
            self.publish_monitor(0, self.monitor_backend().unwrap_or(AudioBackend::Wasapi));
            self.monitor_armed = false;
            self.drain_monitor();
            self.monitor_stream = None;
            self.release_asio_if_unused();
            (self.fault_sink)("output");
        }
    }

    pub(super) fn mirror_diag(&self) {
        self.diag
            .monitor_starves
            .store(self.monitor_starves.load(Relaxed), Relaxed);
        self.diag
            .monitor_out_block
            .store(self.monitor_out_block.load(Relaxed), Relaxed);
        self.diag.monitor_output_latency_ns.store(self.monitor_output_latency_ns.load(Relaxed), Relaxed);
        self.diag
            .input_overruns
            .store(self.input_overruns.load(Relaxed), Relaxed);
    }
}

impl Drop for NativeIo {
    fn drop(&mut self) {
        // Drop both ASIO directions before releasing the single duplex holder. Field metadata, not
        // the mutable global preference, decides whether a driver-backed stream still exists.
        self.input_stream = None;
        self.input_channel = None;
        self.monitor_stream = None;
        self.release_asio_if_unused();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_io(diag: &Arc<ProducerDiag>) -> NativeIo {
        let (in_tx, _in_rx) = rtrb::RingBuffer::<f32>::new(16);
        let (_mon_tx, mon_rx) = rtrb::RingBuffer::<f32>::new(16);
        NativeIo::with_fault_sink(
            0,
            "test",
            Box::new(|_| {}),
            diag.clone(),
            true,
            Arc::new(Mutex::new(in_tx)),
            mon_rx,
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
        )
    }

    /// A re-arm drops the old stream before opening the new one; when that open fails (here: a
    /// device id cpal cannot parse, so no hardware is needed) the published rates must say "no
    /// stream", not the dropped stream's rate.
    #[test]
    fn a_failed_rearm_publishes_rate_zero() {
        if AudioBackend::selected().is_asio() {
            return; // the WASAPI open path is the one under test
        }
        let diag = Arc::new(ProducerDiag::new());
        let mut io = test_io(&diag);
        let not_cancelled = AtomicBool::new(false);

        // As if a 48 kHz stream had been armed on both directions.
        io.publish_input(48_000, AudioBackend::Wasapi);
        io.publish_monitor(48_000, AudioBackend::Wasapi);
        let input_gen = diag.input_gen.load(Relaxed);
        let monitor_gen = diag.monitor_gen.load(Relaxed);

        assert!(io
            .arm_input(Some("no-such-device"), None, &not_cancelled)
            .is_err());
        assert_eq!(diag.input_rate.load(Relaxed), 0);
        assert!(diag.input_gen.load(Relaxed) > input_gen, "RT must see the change");

        assert!(io.arm_monitor(Some("no-such-device"), &not_cancelled).is_err());
        assert_eq!(diag.monitor_rate.load(Relaxed), 0);
        assert!(diag.monitor_gen.load(Relaxed) > monitor_gen, "RT must see the change");
    }

    #[test]
    fn backend_transition_drops_both_retained_streams_only_while_idle() {
        assert_eq!(
            transition_action(
                None,
                Some(AudioBackend::Asio),
                AudioBackend::Wasapi,
                false,
                false,
            ),
            TransitionAction::DropRetained
        );
        assert_eq!(
            transition_action(
                Some(AudioBackend::Asio),
                Some(AudioBackend::Asio),
                AudioBackend::Wasapi,
                true,
                false,
            ),
            TransitionAction::Blocked
        );
        assert_eq!(
            transition_action(
                Some(AudioBackend::Asio),
                Some(AudioBackend::Asio),
                AudioBackend::Asio,
                false,
                false,
            ),
            TransitionAction::None
        );
    }
}
