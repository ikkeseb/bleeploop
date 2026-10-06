//! OWNS: Share output (STATUS E2 "user-picked endpoint"): while
//! ASIO plays (its output bypasses the Windows audio engine, so no app capture can hear it), the
//! engine's post-limiter stereo master is mirrored to a WASAPI render endpoint the user picked, for
//! OBS, browsers and voice chat. On WASAPI no mirror opens: app capture takes the main output.
//!
//! The engine callback pushes each block through [`ShareTap`] into a [`pipes`] ring (drop-on-full);
//! the mirror's own cpal stream pulls it through the pipe's resampler and drift controller at the
//! endpoint's rate. The mirror's callback touches only the pipe and the counters, never the engine
//! lock, so a stalled or dead mirror costs the engine nothing but overrun counts.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use lf_engine::grid::Frame;

use super::pipes::{self, PipeConfig, PullPipe, PushEnd};
use super::IoCounters;

/// The mirror's fill setpoint (40 ms), raised to `PipeConfig::setpoint`'s floor when the engine block
/// is large: one engine block, the largest pull, 3 ms. The largest pull is taken as two ~10 ms WASAPI
/// shared periods asked for at once (what an endpoint's callbacks do in a burst was not traced). At
/// 20 ms (one period) the mirror into the rig's Scarlett, its own Windows endpoint while ASIO 64
/// played, ran short in bursts about every 10.5 minutes on an idle machine (37 starves and 2 trims in
/// a 25-minute soak, `--probe-engine … --share`), each a gap in what a stream or a call hears; at
/// 40 ms the same soak counted nothing in 26 minutes. Nobody monitors through the mirror, so its
/// delay costs nothing.
const SETPOINT: f64 = 0.040;
const WASAPI_PERIOD: f64 = 0.010;
/// Headroom above the setpoint before the tap drops (a mirror that stalls or starts late).
const HEADROOM: f64 = 0.100;
/// Frames the mirror pulls per piece; a larger device callback (WASAPI's first one can be its whole
/// buffer) is served in pieces.
const MAX_PULL: usize = 1024;

/// The callback's end: owned inside the engine lock (`super::Rt`), fed every block after the limiter.
/// Drop-on-full, never blocks; a full ring counts `IoCounters::share_overruns`.
pub(crate) struct ShareTap {
    push: PushEnd,
    /// One interleaved piece of a block.
    scratch: Vec<f32>,
}

impl ShareTap {
    /// Push one block of the stereo master (post-limiter, what the main output plays).
    pub(crate) fn push(&mut self, left: &[f32], right: &[f32], counters: &super::IoCounters) {
        let n = left.len().min(right.len());
        let piece = self.scratch.len() / 2;
        let mut dropped = 0;
        for start in (0..n).step_by(piece) {
            let end = (start + piece).min(n);
            for (frame, (&l, &r)) in self.scratch.chunks_exact_mut(2).zip(left[start..end].iter().zip(&right[start..end])) {
                frame[0] = l;
                frame[1] = r;
            }
            dropped += self.push.push(&self.scratch[..2 * (end - start)]);
        }
        if dropped > 0 {
            counters.share_overruns.fetch_add(1, Relaxed);
        }
    }
}

/// The device owner's end: the mirror's WASAPI stream. Dropping it stops only the mirror.
pub(crate) struct ShareOutput {
    _stream: cpal::Stream,
    fault: Arc<AtomicBool>,
}

impl ShareOutput {
    /// Open the mirror on `endpoint` (a WASAPI render device id) for an engine at `engine_rate` Hz,
    /// rendering `block`-frame blocks. The tap goes into the callback's `Rt`.
    pub(crate) fn open(endpoint: &str, engine_rate: u32, block: Frame, core: std::sync::Arc<super::Core>) -> Result<(ShareOutput, ShareTap), String> {
        let device = crate::audio_output::pick_output_device(Some(endpoint))?;
        let (config, format) = crate::audio_output::output_config(&device)?;
        let (tap, pipe) = share_pipe(engine_rate, config.sample_rate, block)?;
        let mirror = Mirror::new(pipe, config.channels as usize);
        let fault = Arc::new(AtomicBool::new(false));
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, mirror, core, fault.clone()),
            SampleFormat::I32 => build::<i32>(&device, config, mirror, core, fault.clone()),
            SampleFormat::I16 => build::<i16>(&device, config, mirror, core, fault.clone()),
            other => Err(format!("share output: unsupported sample format {other:?}")),
        }?;
        // cpal 0.18 streams start paused.
        stream.play().map_err(|e| format!("share output: play: {e}"))?;
        Ok((ShareOutput { _stream: stream, fault }, tap))
    }

    /// The stream died (its error callback latched): the owner drops the mirror and reports it.
    pub(crate) fn faulted(&self) -> bool {
        self.fault.load(Relaxed)
    }
}

/// The tap and the mirror's pipe for an engine at `engine_rate` with `block`-frame blocks, mirrored at
/// `out_rate` (allocates: off the audio thread).
fn share_pipe(engine_rate: u32, out_rate: u32, block: Frame) -> Result<(ShareTap, PullPipe), String> {
    let block = usize::try_from(block).ok().filter(|&b| b > 0).ok_or_else(|| format!("share output: bad block {block}"))?;
    let setpoint = SETPOINT.max(block as f64 / engine_rate as f64 + 2.0 * WASAPI_PERIOD + 0.003);
    let capacity = ((setpoint + HEADROOM) * engine_rate as f64).ceil() as usize + block;
    let (push, pipe) = pipes::pipe(PipeConfig { in_rate: engine_rate, out_rate, channels: 2, capacity, setpoint, max_pull: MAX_PULL })?;
    Ok((ShareTap { push, scratch: vec![0.0; 2 * block.min(MAX_PULL)] }, pipe))
}

/// The mirror callback's state: the pipe, and a stereo piece to convert into the device's layout.
struct Mirror {
    pipe: PullPipe,
    stereo: Vec<f32>,
    channels: usize,
    /// The largest callback the setpoint is sized for: two WASAPI periods, in device frames.
    largest_pull: usize,
    /// No callback has run yet.
    first: bool,
}

impl Mirror {
    fn new(pipe: PullPipe, channels: usize) -> Mirror {
        let largest_pull = (2.0 * WASAPI_PERIOD * pipe.out_rate()).round() as usize;
        Mirror { pipe, stereo: vec![0.0; 2 * MAX_PULL], channels: channels.max(1), largest_pull, first: true }
    }

    /// Fill one device callback: stereo on channels 0 and 1 (a mono endpoint gets (L + R) / 2), silence
    /// on the rest; a callback the ring could not cover counts one `share_starves`.
    fn render<T: SizedSample + FromSample<f32>>(&mut self, data: &mut [T], counters: &IoCounters) {
        let ch = self.channels;
        // cpal starts the endpoint empty, so its first callback asks for the whole buffer, more than the
        // setpoint holds: served, it would run the ring short at once and starve again while it
        // primes. That one callback is the endpoint's prefill: silence. Only the first, so an endpoint
        // whose every callback is larger still plays (and counts its starves).
        if std::mem::take(&mut self.first) && data.len() / ch > self.largest_pull {
            data.fill(T::EQUILIBRIUM);
            return;
        }
        let mut short = 0;
        for piece in data.chunks_mut(MAX_PULL * ch) {
            let frames = piece.len() / ch;
            let stereo = &mut self.stereo[..2 * frames];
            short += self.pipe.pull(stereo);
            for (out, lr) in piece.chunks_exact_mut(ch).zip(stereo.chunks_exact(2)) {
                if ch == 1 {
                    out[0] = T::from_sample(0.5 * (lr[0] + lr[1]));
                } else {
                    out[0] = T::from_sample(lr[0]);
                    out[1] = T::from_sample(lr[1]);
                    for s in &mut out[2..] {
                        *s = T::from_sample(0.0f32);
                    }
                }
            }
        }
        if short > 0 {
            counters.share_starves.fetch_add(1, Relaxed);
        }
        let trims = self.pipe.take_trims();
        if trims > 0 {
            counters.share_trims.fetch_add(trims, Relaxed);
        }
    }
}

fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: StreamConfig,
    mut mirror: Mirror,
    core: Arc<super::Core>,
    fault: Arc<AtomicBool>,
) -> Result<cpal::Stream, String> {
    let mut promoted = false;
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                if !promoted {
                    promoted = true;
                    let _ = super::promote_pro_audio();
                }
                let _ftz = super::fpu::DenormalsOff::new();
                mirror.render(data, &core.counters);
            },
            // Terminal by cpal's contract (the endpoint went away): latch it for the owner, who drops
            // the mirror and reports it. Nothing logs here: this runs on the stream's thread.
            move |_| fault.store(true, Relaxed),
            None,
        )
        .map_err(|e| format!("share output: build_output_stream: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_io::pipes::tests::wandering;
    use cpal::Sample;

    fn counts(c: &IoCounters) -> (u64, u64, u64) {
        (c.share_starves.load(Relaxed), c.share_overruns.load(Relaxed), c.share_trims.load(Relaxed))
    }

    /// The engine at 48 kHz in 256-frame ASIO blocks, the mirror at 44.1 kHz in wandering WASAPI-sized
    /// callbacks (436–456 frames), its clock 400 ppm off the engine's, 10 simulated minutes; and the other
    /// way round (an engine at a picked 44.1 kHz, the mirror's endpoint at 48 kHz).
    #[test]
    fn the_mirror_follows_the_engine_through_a_skewed_clock_without_starving() {
        for (engine_rate, mirror_rate, skew) in [(48_000, 44_100, 400.0), (48_000, 44_100, -400.0), (44_100, 48_000, 400.0), (44_100, 48_000, -400.0)] {
            let counters = IoCounters::default();
            let (mut tap, pipe) = share_pipe(engine_rate, mirror_rate, 256).unwrap();
            let mut mirror = Mirror::new(pipe, 2);
            let (left, right) = (vec![0.5f32; 256], vec![-0.25f32; 256]);
            let mut data = vec![0i16; 2 * 1024];
            let mut size = wandering();
            let engine_period = 256.0 / (engine_rate as f64 * (1.0 + skew * 1e-6));
            let (mut t_engine, mut t_mirror, mut i) = (0.0f64, 0.0f64, 0u64);
            while t_mirror < 600.0 {
                if t_engine <= t_mirror {
                    tap.push(&left, &right, &counters);
                    t_engine += engine_period;
                    continue;
                }
                // WASAPI-sized at the mirror's rate (the rig's 436–456 frames at 44.1 kHz).
                let n = (size(i) - 34) * mirror_rate as usize / 44_100;
                mirror.render(&mut data[..2 * n], &counters);
                if t_mirror >= 60.0 {
                    // Left on channel 0, right on 1, as i16 (the cubic may round a constant by an LSB).
                    let (l, r) = (i16::from_sample(0.5f32), i16::from_sample(-0.25f32));
                    assert!((data[0] - l).abs() <= 1 && (data[1] - r).abs() <= 1, "{:?}", &data[..2]);
                }
                t_mirror += n as f64 / mirror_rate as f64;
                i += 1;
            }
            // Not one starve or overrun, the startup included: the drift is learned inside the margin.
            assert_eq!(counts(&counters), (0, 0, 0), "{skew:+} ppm: starves/overruns/trims");
            let drift = mirror.pipe.drift_ppm();
            assert!((drift - skew).abs() < 0.1 * skew.abs(), "{skew:+} ppm: learned {drift}");
            // What the ring holds after a pull: under the setpoint plus one engine block.
            let fill_ms = mirror.pipe.fill() as f64 / (engine_rate as f64 / 1000.0);
            assert!(fill_ms < SETPOINT * 1000.0 + 5.4, "{skew:+} ppm: fill {fill_ms} ms");
        }
    }

    /// A render callback that asks for its next period at once (then none for a period), and one that
    /// comes a period late and asks for two: the endpoint's callbacks around a period slip. Neither runs
    /// the mirror short or over, at any engine block.
    #[test]
    fn a_period_pulled_early_or_late_is_covered() {
        for block in [64usize, 128, 256] {
            let counters = IoCounters::default();
            let (mut tap, pipe) = share_pipe(44_100, 44_100, block as Frame).unwrap();
            let mut mirror = Mirror::new(pipe, 2);
            let (left, right) = (vec![0.5f32; block], vec![-0.25f32; block]);
            let mut data = vec![0.0f32; 2 * 441];
            let engine_period = block as f64 / 44_100.0;
            let (mut t_engine, mut t_mirror, mut pulls, mut owed) = (0.0f64, 0.0f64, 0u64, false);
            while t_mirror < 120.0 {
                if t_engine <= t_mirror {
                    tap.push(&left, &right, &counters);
                    t_engine += engine_period;
                    continue;
                }
                mirror.render(&mut data, &counters);
                if std::mem::take(&mut owed) {
                    mirror.render(&mut data, &counters);
                }
                pulls += 1;
                // Once settled, every 3.01 s, by turns: the next period asked for at once and then no
                // callback for a period, or no callback for a period and then two periods at once.
                if t_mirror >= 30.0 && pulls % 301 == 0 {
                    if (pulls / 301) % 2 == 0 {
                        mirror.render(&mut data, &counters);
                    } else {
                        owed = true;
                    }
                    t_mirror += 0.010;
                }
                t_mirror += 0.010;
            }
            assert_eq!(counts(&counters), (0, 0, 0), "block {block}: starves/overruns/trims");
        }
    }

    /// The mirror opens while the engine already plays: the ring is full when the endpoint's first
    /// callback asks for its whole buffer (cpal starts WASAPI empty), then a period every 10 ms. The
    /// first callback plays silence and the mirror starts on the next one, at any engine block, with
    /// no starve; one just over two periods is startup silence as well.
    #[test]
    fn a_whole_buffer_first_pull_does_not_start_short() {
        for block in [64usize, 128, 256] {
            for first in [2048usize, 883] {
                let counters = IoCounters::default();
                let (mut tap, pipe) = share_pipe(44_100, 44_100, block as Frame).unwrap();
                let mut mirror = Mirror::new(pipe, 2);
                let (left, right) = (vec![0.5f32; block], vec![-0.25f32; block]);
                let engine_period = block as f64 / 44_100.0;
                // ~300 ms of engine blocks before the mirror's first callback: the ring runs full.
                let mut t_engine = 0.0f64;
                while t_engine < 0.300 {
                    tap.push(&left, &right, &counters);
                    t_engine += engine_period;
                }
                let mut data = vec![0.0f32; 2 * first];
                mirror.render(&mut data, &counters);
                let mut data = vec![0.0f32; 2 * 441];
                let mut t_mirror = t_engine + 0.010;
                let end = t_mirror + 10.0;
                while t_mirror < end {
                    if t_engine <= t_mirror {
                        tap.push(&left, &right, &counters);
                        t_engine += engine_period;
                        continue;
                    }
                    mirror.render(&mut data, &counters);
                    t_mirror += 0.010;
                }
                let (starves, _, trims) = counts(&counters);
                assert_eq!((starves, trims), (0, 0), "block {block}, first pull {first}: starves/trims");
                assert!((data[0] - 0.5).abs() < 1e-3 && (data[1] + 0.25).abs() < 1e-3, "block {block}, first pull {first}: {:?}", &data[..2]);
            }
        }
    }

    /// Only the first callback is the prefill: an endpoint whose every callback is over two periods
    /// still plays from its second.
    #[test]
    fn only_the_first_large_pull_is_silence() {
        let counters = IoCounters::default();
        let (mut tap, pipe) = share_pipe(44_100, 44_100, 256).unwrap();
        let mut mirror = Mirror::new(pipe, 2);
        for _ in 0..40 {
            tap.push(&[0.5; 256], &[-0.25; 256], &counters);
        }
        let mut data = vec![1.0f32; 2 * 1024];
        mirror.render(&mut data, &counters);
        assert!(data.iter().all(|&x| x == 0.0), "the first: silence");
        mirror.render(&mut data, &counters);
        let last = &data[data.len() - 2..];
        assert!((last[0] - 0.5).abs() < 1e-3 && (last[1] + 0.25).abs() < 1e-3, "the second plays: {last:?}");
    }

    #[test]
    fn a_full_ring_counts_one_overrun_per_block_and_never_blocks() {
        let counters = IoCounters::default();
        let (mut tap, pipe) = share_pipe(48_000, 48_000, 256).unwrap();
        let block = vec![0.1f32; 256];
        // Nobody pulls: the ring fills (~140 ms + a block), then every block drops and counts once.
        for _ in 0..1000 {
            tap.push(&block, &block, &counters);
        }
        let capacity = pipe.fill();
        assert!((6720..6720 + 2 * 256).contains(&capacity), "capacity {capacity}");
        let full_blocks = 1000 - capacity / 256;
        assert!(counts(&counters).1 >= full_blocks as u64 - 1 && counts(&counters).1 <= full_blocks as u64);
    }

    #[test]
    fn a_mono_endpoint_gets_the_mid_and_extra_channels_stay_silent() {
        let counters = IoCounters::default();
        for (channels, expect) in [(1usize, vec![0.3f32]), (4, vec![0.5, 0.1, 0.0, 0.0])] {
            let (mut tap, pipe) = share_pipe(48_000, 48_000, 256).unwrap();
            let mut mirror = Mirror::new(pipe, channels);
            let mut data = vec![9.0f32; channels * 480];
            for _ in 0..8 {
                // Two blocks per callback: more than the mirror takes, so it never runs short here.
                tap.push(&[0.5; 256], &[0.1; 256], &counters);
                tap.push(&[0.5; 256], &[0.1; 256], &counters);
                mirror.render(&mut data, &counters);
            }
            let last = &data[channels * 479..];
            for (got, want) in last.iter().zip(&expect) {
                assert!((got - want).abs() < 1e-5, "{channels} channels: {last:?}");
            }
        }
    }

    #[test]
    fn a_short_callback_counts_one_starve_however_many_pieces_ran_short() {
        let counters = IoCounters::default();
        let (mut tap, pipe) = share_pipe(48_000, 48_000, 256).unwrap();
        let mut mirror = Mirror::new(pipe, 2);
        let mut data = vec![0.0f32; 2 * 2048];
        for _ in 0..8 {
            tap.push(&[0.5; 256], &[0.5; 256], &counters);
        }
        // Primes at the 40 ms setpoint (1920 frames) and plays two periods (960) of them: covered.
        mirror.render(&mut data[..2 * 960], &counters);
        assert_eq!(counts(&counters).0, 0);
        // 2048 frames, pulled in two pieces, both short: one starve.
        mirror.render(&mut data, &counters);
        assert_eq!(counts(&counters).0, 1);
        assert!(data[2 * 1024..].iter().all(|&s| s == 0.0));
    }
}
