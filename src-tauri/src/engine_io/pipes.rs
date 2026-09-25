//! OWNS: the pipe that joins two clocks: frames pushed on one (a capture callback, the engine) are
//! pulled on another (the engine's output callback, the Share mirror's), resampled, with a drift
//! controller trimming the ratio so the ring holds its setpoint. The WASAPI join (input → engine) and
//! Share output (engine → mirror endpoint) both run on it.
//!
//! Reshaped from `host/transport.rs`'s `InPipe`/`OutMonitorPipe`/`DriftController`, which stay where
//! they are for the live line until Stage 6 deletes them with the WebView bridge.
//!
//! Shape: the pusher writes interleaved f32 into an rtrb ring; each `pull` steps the drift controller
//! once on the ring's fill, then resamples exactly the frames asked for (rubato `Async` poly-cubic,
//! `FixedAsync::Output` with the chunk size set per pull), so a callback of any size up to `max_pull`
//! is served straight into the caller's buffer: no output FIFO, no latency beyond the setpoint and
//! the interpolator. Nothing here logs or allocates after [`pipe`].
//!
//! Startup, starves and trims: the pipe primes (plays silence, consumes nothing) until the ring reaches
//! the setpoint, then drops any backlog above it (the Stage 1 spike's WASAPI join carried a 298 ms
//! startup backlog it never drained: `docs/plans/native-engine.md` § Stage 1). A ring that runs short
//! after that serves what it holds, zero-fills the rest and primes again, so a pusher that stalls comes
//! back at the setpoint instead of limping along on an empty ring. A ring that runs over by more than
//! the setpoint again (the puller stalled while the pusher kept on: a WASAPI render glitch) is trimmed
//! back to it the same way, counted ([`PullPipe::take_trims`]): the ±1 % controller would take seconds
//! to drain it, and the pipe's delay would sit that far past [`PullPipe::delay_frames`] meanwhile.

use rtrb::{Consumer, Producer, RingBuffer};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, PolynomialDegree, Resampler};

/// How a pipe is built.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PipeConfig {
    /// The pushing side's rate and the pulling side's, Hz.
    pub(crate) in_rate: u32,
    pub(crate) out_rate: u32,
    /// Interleaved channels per frame (1 for the join, 2 for Share output).
    pub(crate) channels: usize,
    /// Ring capacity, in frames at `in_rate`.
    pub(crate) capacity: usize,
    /// The fill the drift controller holds, in seconds. At least the largest push plus the largest pull
    /// plus ~3 ms: a pull may land on either end of the push sawtooth, and while the two callbacks run
    /// nearly in step (a 10 ms WASAPI join) the controller cannot tell which end it sees; the ~3 ms is
    /// its startup transient while it learns the drift (the matrix test in this file).
    pub(crate) setpoint: f64,
    /// The largest `pull`, in frames at `out_rate`: the pipe's buffers are sized for it at build.
    pub(crate) max_pull: usize,
}

// PI gains: ωn = √KI = 0.1 rad/s, ζ = KP / (2ωn) = 0.7. Faster than `host/transport.rs`'s
// `DriftController` (ωn 0.04): a pipe that starts with its drift unknown must not dip into a starve
// while it learns it, and at 0.04 rad/s a 10 ms ↔ 10 ms WASAPI join at −400 ppm ran short in its first
// minute (the matrix test below).
const KP: f64 = 0.14; // ratio correction per second of fill error
const KI: f64 = 0.01; // ratio correction per second² of fill error
/// The fill a pull sees jumps by whole pushes (a sawtooth of ±½ push around the true level); the
/// controller reads it through this low-pass (seconds) so the jumps do not reach the ratio as jitter.
const FILL_SMOOTHING: f64 = 0.5;
/// ±1 % authority: covers any crystal drift, far inside the resampler's band below.
const MAX_REL_CORR: f64 = 0.01;
/// Input frames a frame spends inside the resampler after it leaves the ring: rubato's `FixedAsync::
/// Output` consumes up to its cubic's 4 points ahead (its `last_index` stays in (−5, −4]), so a pull
/// plays from 3–4 frames behind the ring's read head. Measured at 2.9–3.6 by
/// `delay_frames_is_the_measured_delay_from_push_to_play` (rubato's own `output_delay` says 2).
const RESAMPLER_LAG: f64 = 3.0;
/// The resampler's construction band. Strictly wider than the controller's authority, so a clamped
/// ratio never errors and the input a max-size pull needs stays inside rubato's own buffer.
const MAX_RATIO_RELATIVE: f64 = 1.02;

/// PI on the (smoothed) ring fill, with conditional integration and clamps. The plant is an
/// integrator (fill += pushed − consumed), so the closed loop is type 1: the integral settles on the
/// clock drift itself and the fill error on zero.
struct DriftController {
    /// The smoothed fill error, seconds.
    err: f64,
    integ: f64,
}

impl DriftController {
    /// `err` = fill − setpoint in seconds (+ = over-full), `dt` = the seconds this pull covers. Returns
    /// the relative ratio for rubato: over-full → below 1 → each output frame consumes more input.
    fn step(&mut self, err: f64, dt: f64) -> f64 {
        self.err += (err - self.err) * (dt / FILL_SMOOTHING).min(1.0);
        let p = KP * self.err;
        let next = self.integ + KI * self.err * dt;
        // Anti-windup: hold the integral while the output is saturated and integrating would push it
        // further in.
        let unsat = -(p + next);
        if unsat.abs() < MAX_REL_CORR || unsat.signum() != (-(p + self.integ)).signum() {
            self.integ = next.clamp(-MAX_REL_CORR, MAX_REL_CORR);
        }
        1.0 + (-(p + self.integ)).clamp(-MAX_REL_CORR, MAX_REL_CORR)
    }
}

/// The pushing side. Never blocks, never allocates.
pub(crate) struct PushEnd {
    ring: Producer<f32>,
    channels: usize,
}

impl PushEnd {
    /// Push interleaved frames; returns how many frames did not fit (dropped: the caller counts them).
    pub(crate) fn push(&mut self, frames: &[f32]) -> usize {
        let offered = frames.len() / self.channels;
        // Whole frames only, so the ring always holds a multiple of `channels` samples.
        let fit = (self.ring.slots() / self.channels).min(offered);
        let _ = self.ring.push_partial_slice(&frames[..fit * self.channels]);
        offered - fit
    }
}

/// The pulling side. Never blocks, never allocates after `pipe` built it.
pub(crate) struct PullPipe {
    ring: Consumer<f32>,
    rs: Async<f32>,
    /// One pull's input frames, interleaved: rubato wants them contiguous and the ring may wrap.
    scratch: Vec<f32>,
    channels: usize,
    in_rate: f64,
    out_rate: f64,
    setpoint: f64,
    /// The setpoint in frames at `in_rate`.
    target: usize,
    max_pull: usize,
    ctrl: DriftController,
    /// Consuming: the ring reached the setpoint and has not run short since.
    primed: bool,
    /// Primed once: silence before the first prime is startup, not a shortfall.
    started: bool,
    /// Over-full rings trimmed back to the setpoint since the last [`PullPipe::take_trims`].
    trims: u64,
}

/// Build a pipe (allocates: off the audio thread).
pub(crate) fn pipe(config: PipeConfig) -> Result<(PushEnd, PullPipe), String> {
    let PipeConfig { in_rate, out_rate, channels, capacity, setpoint, max_pull } = config;
    if in_rate == 0 || out_rate == 0 || channels == 0 || max_pull == 0 {
        return Err(format!("pipe: invalid config {config:?}"));
    }
    let target = (setpoint * in_rate as f64).round();
    if !setpoint.is_finite() || setpoint < 0.0 || target as usize > capacity {
        return Err(format!("pipe: the setpoint does not fit the ring ({config:?})"));
    }
    // Poly cubic, not sinc: the ratio moves every pull and sinc would recompute its anti-alias
    // filters on each change (transport.rs's reasoning for `Hop1Pipe`).
    let rs = Async::<f32>::new_poly(
        out_rate as f64 / in_rate as f64,
        MAX_RATIO_RELATIVE,
        PolynomialDegree::Cubic,
        max_pull,
        channels,
        FixedAsync::Output,
    )
    .map_err(|e| format!("pipe: rubato Async::new_poly: {e}"))?;
    // rubato's own buffer holds `input_frames_max` new frames; with the ratio clamped to ±1 % inside the
    // ±2 % band, the input a pull needs stays below it at any chunk size (the margin is 1 % of the
    // chunk plus the interpolator's slack). `pull_piece` still checks: a miss is silence, not a panic.
    let scratch = vec![0.0f32; rs.input_frames_max() * channels];
    let (producer, consumer) = RingBuffer::new(capacity * channels);
    Ok((
        PushEnd { ring: producer, channels },
        PullPipe {
            ring: consumer,
            rs,
            scratch,
            channels,
            in_rate: in_rate as f64,
            out_rate: out_rate as f64,
            setpoint,
            target: target as usize,
            max_pull,
            ctrl: DriftController { err: 0.0, integ: 0.0 },
            primed: false,
            started: false,
            trims: 0,
        },
    ))
}

impl PullPipe {
    /// Fill `out` (interleaved, `out.len() / channels` frames at `out_rate`, at most `max_pull`):
    /// resampled from the ring, the ratio trimmed toward the setpoint. A short ring zero-fills the rest
    /// (silence, never stale); returns how many frames were zero-filled (0 = none). The first pull that
    /// finds the ring at or above the setpoint drops any startup backlog down to it (a capture that ran
    /// before the output opened must not ride along as latency). Until then the pull plays silence and
    /// consumes nothing; that startup silence returns 0. After it, a ring that ran short primes again
    /// (silence, counted, until it is back at the setpoint), and a ring found above twice the setpoint
    /// drops back to it (counted in [`PullPipe::take_trims`]). A larger `out` is served in `max_pull`
    /// pieces.
    pub(crate) fn pull(&mut self, out: &mut [f32]) -> usize {
        let ch = self.channels;
        let whole = out.len() / ch * ch;
        out[whole..].fill(0.0);
        let mut short = 0;
        for piece in out[..whole].chunks_mut(self.max_pull * ch) {
            short += self.pull_piece(piece);
        }
        short
    }

    fn pull_piece(&mut self, out: &mut [f32]) -> usize {
        let ch = self.channels;
        let n = out.len() / ch;
        if !self.primed {
            let fill = self.fill();
            if fill < self.target {
                out.fill(0.0);
                return if self.started { n } else { 0 };
            }
            self.drop_to_target(fill);
            (self.primed, self.started) = (true, true);
        } else {
            // The fill a pull sees settles within about one push of the setpoint (a pull lands anywhere
            // on the push sawtooth, plus the controller's ~3 ms while it learns), and the setpoint is at
            // least the largest push plus the largest pull plus 3 ms (`PipeConfig::setpoint`): twice
            // the setpoint is never reached in steady state, only after the puller lost time. The drift
            // learned so far stays; the smoothed error restarts at the setpoint the ring now holds.
            let fill = self.fill();
            if fill > 2 * self.target {
                self.drop_to_target(fill);
                self.ctrl.err = 0.0;
                self.trims += 1;
            }
        }
        let fill = self.fill();
        let err = (fill as f64 - self.target as f64) / self.in_rate;
        let rel = self.ctrl.step(err, n as f64 / self.out_rate);
        // Both are in range by construction (n ≤ max_pull; rel within ±1 % of a ±2 % band).
        let _ = self.rs.set_resample_ratio_relative(rel, true);
        let _ = self.rs.set_chunk_size(n);
        let mut k = n;
        if self.rs.input_frames_next() > fill {
            // Short: produce the most frames the ring covers, silence the rest, prime again.
            self.primed = false;
            let deficit = (self.rs.input_frames_next() - fill) as f64;
            k = n.saturating_sub((deficit * self.rs.resample_ratio()).ceil() as usize);
            while k > 0 {
                let _ = self.rs.set_chunk_size(k);
                if self.rs.input_frames_next() <= fill {
                    break;
                }
                k -= 1;
            }
        }
        let need = self.rs.input_frames_next();
        if k > 0 && need * ch <= self.scratch.len() {
            let input = &mut self.scratch[..need * ch];
            let produced = match self.ring.pop_entire_slice(input) {
                Ok(()) => match (
                    InterleavedSlice::new(&input[..], ch, need),
                    InterleavedSlice::new_mut(&mut out[..k * ch], ch, k),
                ) {
                    (Ok(i), Ok(mut o)) => self.rs.process_into_buffer(&i, &mut o, None).map_or(0, |(_, p)| p),
                    _ => 0,
                },
                Err(_) => 0,
            };
            k = produced.min(k);
        } else {
            k = 0;
        }
        out[k * ch..].fill(0.0);
        n - k
    }

    /// Drop the oldest frames of a ring holding `fill` down to the setpoint.
    fn drop_to_target(&mut self, fill: usize) {
        if let Ok(backlog) = self.ring.read_chunk((fill - self.target) * self.channels) {
            backlog.commit_all();
        }
    }

    /// Over-full rings trimmed back to the setpoint since the last call (the caller counts them, as it
    /// counts the zero-filled pulls `pull` returns); each one skipped the excess, a jump in what plays.
    pub(crate) fn take_trims(&mut self) -> u64 {
        std::mem::take(&mut self.trims)
    }

    /// What the pipe delays a frame by once settled, in frames at `out_rate`: the setpoint plus the
    /// resampler's own delay. The join's share of `ProcessContext::input_frames`. Measured from the push
    /// call to the frame's place in a pull, averaged over a push's frames; while push and pull run in
    /// step the true figure sits anywhere within ± half a push of it (the matrix test's note).
    pub(crate) fn delay_frames(&self) -> f64 {
        (self.setpoint * self.in_rate + RESAMPLER_LAG) * self.out_rate / self.in_rate
    }

    /// The clock drift the controller has learned, in ppm (diagnostics): + = the pusher's clock runs
    /// fast against the puller's.
    pub(crate) fn drift_ppm(&self) -> f64 {
        self.ctrl.integ * 1.0e6
    }

    /// The ring's fill now, in frames at `in_rate`.
    pub(crate) fn fill(&self) -> usize {
        self.ring.slots() / self.channels
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A pusher and a puller on two simulated clocks, in accelerated time (no device, no sleeping):
    /// the pusher delivers `push` frames every `push / (in_rate · (1 + skew))` seconds, the puller asks
    /// for `pulls(i)` frames every `pulls(i) / out_rate` seconds.
    pub(crate) struct Clocks {
        pub(crate) config: PipeConfig,
        pub(crate) skew_ppm: f64,
        pub(crate) push: usize,
        /// Seconds the pusher runs before the first pull.
        pub(crate) head_start: f64,
        /// (at, seconds): the first pull due at or after `at` comes that much late, the pusher running
        /// on meanwhile (a render callback the device skipped).
        pub(crate) stall: Option<(f64, f64)>,
    }

    pub(crate) struct Pulled<'a> {
        pub(crate) t: f64,
        pub(crate) out: &'a [f32],
        /// The fill the pull found (before it consumed), in frames at `in_rate`.
        pub(crate) fill_before: usize,
        pub(crate) short: usize,
        /// Frames the pushes since the previous pull dropped.
        pub(crate) dropped: usize,
        pub(crate) pipe: &'a PullPipe,
    }

    impl Clocks {
        /// `source(frame, out)` writes one pushed frame (all channels); `each` sees every pull.
        pub(crate) fn run(
            &self,
            seconds: f64,
            mut pulls: impl FnMut(u64) -> usize,
            mut source: impl FnMut(u64, &mut [f32]),
            mut each: impl FnMut(&Pulled),
        ) {
            let c = self.config;
            let (mut tx, mut rx) = pipe(c).unwrap();
            let mut block = vec![0.0f32; self.push * c.channels];
            let mut out = vec![0.0f32; c.max_pull * c.channels];
            let push_period = self.push as f64 / (c.in_rate as f64 * (1.0 + self.skew_ppm * 1e-6));
            let (mut t_push, mut t_pull) = (0.0f64, self.head_start);
            let (mut pushed, mut pull_i, mut dropped) = (0u64, 0u64, 0usize);
            let mut n = pulls(0);
            let mut stall = self.stall;
            while t_pull < seconds {
                if let Some((_, late)) = stall.filter(|&(at, _)| t_pull >= at) {
                    t_pull += late;
                    stall = None;
                }
                if t_push <= t_pull {
                    for (k, frame) in block.chunks_mut(c.channels).enumerate() {
                        source(pushed + k as u64, frame);
                    }
                    pushed += self.push as u64;
                    dropped += tx.push(&block);
                    t_push += push_period;
                } else {
                    let fill_before = rx.fill();
                    let short = rx.pull(&mut out[..n * c.channels]);
                    each(&Pulled { t: t_pull, out: &out[..n * c.channels], fill_before, short, dropped, pipe: &rx });
                    dropped = 0;
                    t_pull += n as f64 / c.out_rate as f64;
                    pull_i += 1;
                    n = pulls(pull_i);
                }
            }
        }
    }

    /// Pull sizes 470–490, the way a WASAPI shared callback wanders (a fixed LCG: reproducible).
    pub(crate) fn wandering() -> impl FnMut(u64) -> usize {
        let mut x: u32 = 0x2545_f491;
        move |_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            470 + (x >> 16) as usize % 21
        }
    }

    fn config(in_rate: u32, out_rate: u32) -> PipeConfig {
        PipeConfig { in_rate, out_rate, channels: 1, capacity: in_rate as usize, setpoint: SETPOINT, max_pull: 1024 }
    }

    /// 10 ms pushes, pulls up to 11.1 ms, +3 ms (`PipeConfig::setpoint`'s rule), rounded up.
    const SETPOINT: f64 = 0.025;

    /// The bound on settling, in simulated seconds from the first push, with no drift learned yet: from
    /// then on no pull runs short, no push overflows, and every fill a pull finds sits within one push
    /// plus 3 ms of the setpoint (a pull lands anywhere on the push sawtooth).
    const SETTLED_BY: f64 = 60.0;

    /// The learned drift is scored as its mean from `SETTLED_BY` to the end: where push and pull run
    /// nearly in step (10 ms ↔ 10 ms) the fill a pull sees only moves when a push crosses it, once per
    /// push / skew = 25 s at 400 ppm, so the instantaneous estimate wanders around the skew by up to
    /// ~±230 ppm over that beat (the report's "instant worst"); elsewhere it holds within ~15 ppm.
    #[test]
    fn the_fill_settles_on_the_setpoint_and_learns_the_skew_at_400_ppm_either_way() {
        let (mut report, mut failed) = (String::new(), 0);
        for &(in_rate, out_rate) in &[(48_000u32, 48_000u32), (44_100, 48_000), (48_000, 44_100)] {
            for skew in [400.0, -400.0] {
                for pulls in ["256", "480", "470-490"] {
                    let push = in_rate as usize / 100;
                    let clocks = Clocks { config: config(in_rate, out_rate), skew_ppm: skew, push, head_start: 0.0, stall: None };
                    let band_ms = push as f64 / in_rate as f64 * 1e3 + 3.0;
                    let (mut settled_at, mut shorts, mut drops, mut trims, mut lo_ms, mut hi_ms) = (0.0f64, 0, 0, 0, 0.0f64, 0.0f64);
                    let (mut drift_sum, mut drift_n, mut drift_worst, mut wobble) = (0.0, 0u32, 0.0f64, 0.0f64);
                    let mut size: Box<dyn FnMut(u64) -> usize> = match pulls {
                        "256" => Box::new(|_| 256),
                        "480" => Box::new(|_| 480),
                        _ => Box::new(wandering()),
                    };
                    clocks.run(600.0, &mut size, |_, f| f.fill(0.0), |p| {
                        let e_ms = (p.fill_before as f64 / in_rate as f64 - SETPOINT) * 1e3;
                        shorts += (p.short > 0) as usize;
                        drops += p.dropped;
                        trims = p.pipe.trims;
                        // Startup priming (the ring filling to the setpoint) is not a settling miss.
                        if p.pipe.started && (p.short > 0 || p.dropped > 0 || e_ms.abs() > band_ms) {
                            settled_at = p.t;
                        }
                        if p.t < SETTLED_BY {
                            return;
                        }
                        (lo_ms, hi_ms) = (lo_ms.min(e_ms), hi_ms.max(e_ms));
                        let drift = p.pipe.drift_ppm();
                        (drift_sum, drift_n) = (drift_sum + drift, drift_n + 1);
                        drift_worst = drift_worst.max((drift - skew).abs());
                        // The ratio applied against the one that exactly cancels the skew, ppm.
                        let applied = p.pipe.rs.resample_ratio() * in_rate as f64 / out_rate as f64;
                        wobble = wobble.max(((1.0 / applied - 1.0) * 1e6 - skew).abs());
                    });
                    let drift = drift_sum / drift_n as f64;
                    report += &format!(
                        "{in_rate}->{out_rate} {skew:+} ppm, pulls {pulls}: settled at {settled_at:.1} s, shorts {shorts}, overruns {drops}, trims {trims}; \
                         after {SETTLED_BY} s: fill {lo_ms:+.1}..{hi_ms:+.1} ms, mean drift {drift:+.1} ppm (instant worst {drift_worst:.0} off), \
                         ratio wobble {wobble:.0} ppm\n"
                    );
                    // No starve, overrun or trim at all, the startup included: the setpoint's margin covers
                    // the controller while it learns the drift.
                    if shorts + drops > 0 || trims > 0 || settled_at > SETTLED_BY || (drift - skew).abs() > 0.1 * skew.abs() {
                        failed += 1;
                        report += "  ^ FAILED\n";
                    }
                }
            }
        }
        println!("{report}");
        assert_eq!(failed, 0, "cases failed:\n{report}");
    }

    #[test]
    fn a_sine_keeps_its_frequency_and_has_no_discontinuity_after_settling() {
        for &(in_rate, out_rate) in &[(44_100u32, 48_000u32), (48_000, 44_100), (48_000, 48_000)] {
            let (skew, freq) = (400.0, 440.0);
            let clocks = Clocks { config: config(in_rate, out_rate), skew_ppm: skew, push: in_rate as usize / 100, head_start: 0.0, stall: None };
            // Counted on the puller's frames: the pusher's clock runs `skew` fast, so its 440 Hz plays at
            // 440·(1 + skew) there once the pipe tracks it (untracked, it would read 400 ppm low).
            let expected = freq * (1.0 + skew * 1e-6);
            let w = 2.0 * std::f64::consts::PI * expected / out_rate as f64;
            let (mut frame, mut prev, mut first_up, mut last_up, mut ups) = (0u64, [0.0f64; 2], None, 0.0, 0u64);
            let mut worst: f64 = 0.0;
            clocks.run(
                SETTLED_BY + 120.0,
                wandering(),
                |k, f| f[0] = (0.5 * (2.0 * std::f64::consts::PI * freq * k as f64 / in_rate as f64).sin()) as f32,
                |p| {
                    assert_eq!(p.pipe.trims, 0, "{in_rate}->{out_rate}: trimmed at {:.1} s", p.t);
                    for &y in p.out {
                        let y = y as f64;
                        if p.t >= SETTLED_BY {
                            // A pure sine satisfies y[n+1] = 2cos(w)·y[n] − y[n−1]: a dropped, repeated
                            // or stale frame shows up in the residual.
                            worst = worst.max((y - 2.0 * w.cos() * prev[1] + prev[0]).abs());
                            if prev[1] < 0.0 && y >= 0.0 {
                                let at = frame as f64 - 1.0 + prev[1] / (prev[1] - y);
                                first_up.get_or_insert(at);
                                last_up = at;
                                ups += 1;
                            }
                        }
                        prev = [prev[1], y];
                        frame += 1;
                    }
                },
            );
            let measured = (ups - 1) as f64 / ((last_up - first_up.unwrap()) / out_rate as f64);
            let off_ppm = (measured / expected - 1.0) * 1e6;
            println!("{in_rate}->{out_rate}: {measured:.4} Hz ({off_ppm:+.1} ppm off), worst residual {worst:.2e}");
            assert!(off_ppm.abs() < 40.0, "{in_rate}->{out_rate}: {measured} Hz, expected {expected}");
            assert!(worst < 1e-3, "{in_rate}->{out_rate}: a discontinuity of {worst}");
        }
    }

    #[test]
    fn the_first_pull_drops_a_startup_backlog_to_the_setpoint() {
        // ~300 ms pushed before the output opens (the Stage 1 spike's 298 ms join ring). Equal rates and
        // the frame index as the signal, so the output names the frame it plays; the pull lands
        // mid-period so push/pull order never hangs on a float tie.
        let config = PipeConfig { setpoint: 0.020, ..config(48_000, 48_000) };
        let clocks = Clocks { config, skew_ppm: 0.0, push: 480, head_start: 0.305, stall: None };
        let mut pulls = Vec::new();
        clocks.run(0.6, |_| 480, |k, f| f[0] = k as f32, |p| pulls.push((p.fill_before, p.short, p.out[100], p.pipe.fill())));
        let (fill_before, short, played, fill_after) = pulls[0];
        assert!(fill_before >= 14_400, "the backlog was there: {fill_before}");
        assert_eq!(short, 0);
        // The drop leaves the read head the setpoint (960 frames) behind the ring's end; output frame j
        // plays input j − 1 from there (the cubic's 2-frame history).
        let head = (fill_before - 960) as f32;
        assert!((played - (head + 99.0)).abs() <= 1.0, "played frame {played}, head {head}");
        assert!(fill_after <= 960 - 470, "the ring kept the backlog: {fill_after}");
        let worst = pulls[1..].iter().map(|p| p.0).max().unwrap();
        assert!(worst <= 960 + 480, "the fill after the drop: {worst}");
        assert!(pulls.iter().all(|p| p.1 == 0));
    }

    /// A render callback that comes 60–150 ms late (a WASAPI glitch) while the pusher runs on: the next
    /// pull trims the excess once, the learned drift survives it, and the fill is back in the settled
    /// band from the pull after, with no starve in the minute that follows. Left to the ±1 % controller,
    /// the excess would drain over seconds and its recovery undershoot into a starve.
    #[test]
    fn a_late_puller_trims_the_excess_once_and_keeps_its_drift() {
        let (mut report, mut failed) = (String::new(), 0);
        for &(in_rate, out_rate) in &[(48_000u32, 48_000u32), (44_100, 48_000), (48_000, 44_100)] {
            for skew in [400.0, -400.0] {
                for late in [0.060, 0.100, 0.150] {
                    let push = in_rate as usize / 100;
                    let at = SETTLED_BY + 30.0;
                    let clocks =
                        Clocks { config: config(in_rate, out_rate), skew_ppm: skew, push, head_start: 0.0, stall: Some((at, late)) };
                    let band_ms = push as f64 / in_rate as f64 * 1e3 + 3.0;
                    let (mut trims, mut shorts, mut drops, mut worst_ms, mut stalled_fill_ms) = (0, 0, 0, 0.0f64, 0.0);
                    let (mut drift_before, mut drift_after, mut resumed) = (0.0, 0.0, 0u32);
                    clocks.run(at + late + 60.0, wandering(), |_, f| f.fill(0.0), |p| {
                        if p.t < at {
                            drift_before = p.pipe.drift_ppm();
                            return;
                        }
                        let e_ms = (p.fill_before as f64 / in_rate as f64 - SETPOINT) * 1e3;
                        trims = p.pipe.trims;
                        shorts += (p.short > 0) as usize;
                        drops += p.dropped;
                        resumed += 1;
                        if resumed == 1 {
                            // The pull that comes late finds the excess and trims it.
                            stalled_fill_ms = e_ms;
                            drift_after = p.pipe.drift_ppm();
                        } else {
                            worst_ms = worst_ms.max(e_ms.abs());
                        }
                    });
                    report += &format!(
                        "{in_rate}->{out_rate} {skew:+} ppm, {:.0} ms late: found {stalled_fill_ms:+.1} ms over, trims {trims}, \
                         then fill within {worst_ms:.1} ms (band {band_ms:.1}), shorts {shorts}, overruns {drops}, \
                         drift {drift_before:+.0} -> {drift_after:+.0} ppm\n",
                        late * 1e3
                    );
                    if trims != 1 || shorts + drops > 0 || worst_ms > band_ms || (drift_after - drift_before).abs() > 50.0 {
                        failed += 1;
                        report += "  ^ FAILED\n";
                    }
                }
            }
        }
        println!("{report}");
        assert_eq!(failed, 0, "cases failed:\n{report}");
    }

    /// Share output's shape (`share.rs`): 256-frame engine pushes at 48 kHz, 436–456-frame pulls at
    /// 44.1 kHz, the 20 ms setpoint, stereo. Its sawtooth never reads as a trim.
    #[test]
    fn share_shaped_pushes_never_trim() {
        for skew in [400.0, -400.0] {
            let config = PipeConfig { channels: 2, setpoint: 0.020, ..config(48_000, 44_100) };
            let clocks = Clocks { config, skew_ppm: skew, push: 256, head_start: 0.0, stall: None };
            let mut size = wandering();
            let (mut trims, mut shorts) = (0, 0);
            clocks.run(300.0, |i| size(i) - 34, |_, f| f.fill(0.0), |p| {
                trims = p.pipe.trims;
                shorts += (p.pipe.started && p.short > 0) as usize;
            });
            assert_eq!((trims, shorts), (0, 0), "{skew:+} ppm: trims, shorts");
        }
    }

    #[test]
    fn a_short_ring_zero_fills_counts_and_comes_back_at_the_setpoint() {
        let (mut tx, mut rx) = pipe(PipeConfig { setpoint: 0.020, ..config(48_000, 48_000) }).unwrap();
        let mut out = vec![1.0f32; 480];
        // Before the first prime: silence, not a shortfall.
        tx.push(&[0.5; 480]);
        assert_eq!(rx.pull(&mut out), 0);
        assert!(out.iter().all(|&s| s == 0.0));
        tx.push(&[0.5; 960]);
        assert_eq!(rx.pull(&mut out), 0);
        // The pusher stops. Full pulls until the ring runs short; that pull plays what the ring held
        // and zero-fills the rest.
        let short = loop {
            match rx.pull(&mut out) {
                0 => continue,
                short => break short,
            }
        };
        assert!(short < 480, "the ring held something: {short}");
        assert!(out[480 - short..].iter().all(|&s| s == 0.0));
        assert!(out[..480 - short].iter().all(|&s| (s - 0.5).abs() < 1e-6));
        // Then silence, counted, until pushes bring the ring back to the setpoint.
        assert_eq!(rx.pull(&mut out), 480);
        assert!(out.iter().all(|&s| s == 0.0));
        let mut resumed_at = 0;
        for _ in 0..10 {
            tx.push(&[0.5; 480]);
            let fill = rx.fill();
            if rx.pull(&mut out) == 0 {
                resumed_at = fill;
                break;
            }
        }
        assert!(resumed_at >= 960, "resumed below the setpoint: {resumed_at}");
        assert!(out[4..].iter().all(|&s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn a_full_ring_drops_what_does_not_fit_and_reports_it() {
        let (mut tx, rx) = pipe(PipeConfig { capacity: 1000, setpoint: 0.020, ..config(48_000, 48_000) }).unwrap();
        assert_eq!(tx.push(&[0.0; 960]), 0);
        assert_eq!(tx.push(&[0.0; 480]), 440);
        assert_eq!(rx.fill(), 1000);
        let (mut tx, rx) = pipe(PipeConfig { channels: 2, capacity: 100, setpoint: 0.001, ..config(48_000, 48_000) }).unwrap();
        assert_eq!(tx.push(&[0.0; 2 * 150]), 50);
        assert_eq!(rx.fill(), 100);
    }

    #[test]
    fn delay_frames_is_the_measured_delay_from_push_to_play() {
        // The frame index as the signal (the cubic plays a ramp exactly), so each output frame names
        // the input frame it plays, and that frame's push time is known. Scored once settled (from
        // 120 s), over pulls (256) that sample every phase of the pushes.
        for &(in_rate, out_rate) in &[(48_000u32, 48_000u32), (44_100, 48_000), (48_000, 44_100)] {
            let push = in_rate as usize / 100;
            let clocks = Clocks { config: config(in_rate, out_rate), skew_ppm: 400.0, push, head_start: 0.0, stall: None };
            let push_period = push as f64 / (in_rate as f64 * (1.0 + 400e-6));
            let (mut sum, mut count, mut delay_frames) = (0.0, 0u64, 0.0);
            clocks.run(180.0, |_| 256, |k, f| f[0] = (k % (1 << 20)) as f32, |p| {
                assert_eq!(p.pipe.trims, 0, "{in_rate}->{out_rate}: trimmed at {:.1} s", p.t);
                if p.t < 120.0 {
                    return;
                }
                delay_frames = p.pipe.delay_frames();
                for (j, &y) in p.out.iter().enumerate() {
                    // Frames count modulo 2^20 to keep f32 exact enough; unwrap against the pull's time.
                    let wraps = ((p.t * in_rate as f64 - y as f64) / (1 << 20) as f64).round();
                    let frame = y as f64 + wraps * (1 << 20) as f64;
                    let pushed_at = (frame / push as f64).floor() * push_period;
                    sum += p.t + j as f64 / out_rate as f64 - pushed_at;
                    count += 1;
                }
            });
            let measured_ms = sum / count as f64 * 1e3;
            let reported_ms = delay_frames / out_rate as f64 * 1e3;
            println!("{in_rate}->{out_rate}: measured {measured_ms:.3} ms, delay_frames {reported_ms:.3} ms");
            assert!((measured_ms - reported_ms).abs() < 0.05, "{in_rate}->{out_rate}: measured {measured_ms} ms, reported {reported_ms} ms");
        }
    }

    /// `host::rt_alloc`'s shim is this crate's global allocator in debug builds (the test build): its
    /// counter moves for any allocation on a thread that holds the guard.
    #[cfg(debug_assertions)]
    #[test]
    fn push_and_pull_never_allocate() {
        use crate::host::rt_alloc;
        use std::sync::atomic::Ordering::Relaxed;
        let counted = || rt_alloc::RT_ALLOCS.load(Relaxed);
        // The probe sees an allocation (else a zero below proves nothing).
        let before = counted();
        {
            let _g = rt_alloc::guard();
            std::hint::black_box(vec![0u8; 16]);
        }
        assert!(counted() > before, "the allocation counter is not live in this build");

        let (mut tx, mut rx) = pipe(PipeConfig { channels: 2, ..config(44_100, 48_000) }).unwrap();
        let block = vec![0.25f32; 2 * 441];
        let mut out = vec![0.0f32; 2 * 3000];
        let mut size = wandering();
        let before = counted();
        {
            let _g = rt_alloc::guard();
            // Startup backlog, priming, varying sizes, an oversized pull (served in pieces), a starve.
            for _ in 0..30 {
                tx.push(&block);
            }
            for i in 0..2000u64 {
                tx.push(&block);
                let n = if i == 500 { 3000 } else { size(i) };
                rx.pull(&mut out[..2 * n]);
                std::hint::black_box(rx.drift_ppm() + rx.delay_frames() + rx.fill() as f64);
            }
            for _ in 0..10 {
                rx.pull(&mut out[..2 * 480]);
            }
        }
        assert_eq!(counted(), before, "push/pull allocated");
    }
}
