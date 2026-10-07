//! DEV: the probe's loopback tone (`--probe-engine … --tone`, `probe.rs`'s header). A steady sine leaves
//! on one output side from a hook in the output callback ([`ToneRig`], `Rt::tone`), comes back through a
//! cable on slot 1's input, and [`Detector`] watches it for discontinuities, frame by frame, in that same
//! hook. Beside it the output callback notes every callback that ran long or entered late ([`SlowLog`]),
//! so the report ties an event to a callback.
//!
//! The RT side ([`ToneRig::block`], [`Detector::push`], [`SlowLog::record`]) never allocates, logs, locks
//! or waits: every table is filled when the rig is built, and the probe talks to it through the atomics
//! of [`Shared`]. The report ([`ToneRig::report`]) runs on the probe's thread once the device has closed.
//!
//! What it cannot see: a slip of a whole multiple of [`PERIOD`] frames leaves a perfect sine (true of any
//! single tone and its period), a phase slip while the level is out of its band is hidden until the level
//! is back, and the cable returns through the same driver's input, so an event may be the input's.

use std::f64::consts::{PI, TAU};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};
use std::sync::Arc;
use std::time::Duration;

use lf_engine::grid::Frame;

/// The tone: [`CYCLES`] whole cycles in `PERIOD` frames at any rate, 1078.7 Hz at 48 kHz and 991.0 Hz at
/// 44.1 kHz. Why this one:
/// - a frame's phase is `CYCLES · frame mod PERIOD`, a table index: continuous across callbacks by
///   construction, and the detector regenerates the reference of any frame exactly;
/// - a fit window of `PERIOD` frames holds whole cycles, so DC, the tone's image at twice its frequency
///   and its harmonics cancel exactly: a clean tone fits to one amplitude and phase wherever a window starts;
/// - `PERIOD` is prime, so a slip of d frames moves the phase by 2d/89 of a turn: nothing only for a
///   multiple of 89 frames (no multiple of a 64, 128 or 256-frame buffer under 5696 frames), 0.141 rad
///   for one frame, and never less than 1/89 of a turn (0.071 rad, 2.3 times [`PHASE_MIN`]) otherwise.
pub(crate) const PERIOD: usize = 89;
const CYCLES: usize = 2;
/// The tone's amplitude on the output side (-12 dBFS).
pub(crate) const LEVEL: f32 = 0.25;
/// The fit window, in frames: [`PERIOD`] (whole cycles, above). 1.9 ms at 48 kHz: short enough to place
/// an event within a period of a 128-frame buffer, long enough that -60 dBFS of noise moves a window's
/// phase by under 0.001 rad.
pub(crate) const WINDOW: usize = PERIOD;
/// The tone's own period in frames (a phase step in frames is taken modulo this).
const CYCLE_FRAMES: f64 = PERIOD as f64 / CYCLES as f64;

/// From the tone's full level to the calibration, the calibration's span, and the span events merge
/// within (grouping only: the coincidence window is [`COINCIDENCE_PERIODS`]), in seconds.
const SETTLE_SECONDS: f64 = 0.5;
const CALIBRATE_SECONDS: f64 = 1.0;
const MERGE_SECONDS: f64 = 0.1;

/// The frozen thresholds: each the larger of a multiple of the noise the calibration learned and a floor.
/// `RES_*`: the recurrence residual, floor in parts of the amplitude (a one-frame slip peaks at 0.14, at
/// its weakest phase 0.02: that one is the phase cue's). `PHASE_*`: a window's phase against the
/// reference, floor in radians (0.21 of a frame; a one-frame slip is 0.141). `AMP_*`: a window's
/// amplitude against the reference, floor in parts of it (a hole of 9 frames or more dips a window by it).
const RES_SIGMAS: f64 = 10.0;
const RES_MIN: f64 = 0.02;
const PHASE_SIGMAS: f64 = 10.0;
const PHASE_MIN: f64 = 0.03;
const AMP_SIGMAS: f64 = 10.0;
const AMP_MIN: f64 = 0.05;
/// A tone too unsteady to watch: a phase threshold past half a one-frame slip, or an amplitude threshold
/// that the control's hole (a third of a window at its weakest split) would not clear with margin.
const PHASE_MAX: f64 = 0.5 * TAU * CYCLES as f64 / PERIOD as f64;
const AMP_MAX: f64 = 0.15;
/// A window's phase is judged only at this share of the amplitude or more (below it the angle is noise).
const PHASE_AMP_FLOOR: f64 = 0.5;
/// Below this amplitude at the input there is no tone (-46 dBFS: a missing cable, the wrong channel).
const MIN_LEVEL: f64 = 0.005;
/// An input sample at or past this clipped (`callback.rs`'s meter rule).
const CLIP: f32 = 0.999;

/// Table sizes: events, coverage gaps, waveform snippets (the first events'), and a snippet's frames
/// before its event was seen and after.
const EVENT_CAP: usize = 256;
const GAP_CAP: usize = 64;
const SNIPPETS: usize = 8;
const SNIP_HALF: usize = 256;

/// The rig: the tone's ramp in and out, from the calibration to the first control, from the second
/// control to the ramp out (seconds), and a control's hole (frames of silence in the output tone).
const RAMP_SECONDS: f64 = 0.02;
const CONTROL_AFTER_SECONDS: f64 = 0.25;
const TAIL_SECONDS: f64 = 0.5;
const HOLE: Frame = 64;

/// [`SlowLog`]: its rows, the rows only a long callback may take, and how many periods behind the best
/// phase an entry is recorded as late. `callback::PhaseSlips` reports a wake more than one period behind;
/// the rig's USB driver does that ~180 times a second at 64 frames with nothing wrong (92 to 109 frames
/// behind), so one period would flood the table. Two periods is past that jitter.
const SLOW_CAP: usize = 16_384;
const LONG_RESERVE: usize = 1024;
const LATE_PERIODS: u64 = 2;

/// The report. An event coincides with a callback within this many periods plus one fit window, either
/// with the callback's frame as it is (the input's side) or shifted by the round trip (the output's).
const COINCIDENCE_PERIODS: Frame = 4;
/// A control is looked for this long after it left, and found within this of the alignment plus a window.
const CONTROL_SEARCH_SECONDS: f64 = 0.25;
const CONTROL_TOLERANCE_MS: f64 = 1.0;
/// A control came back as its hole alone when its residual cues span [`HOLE`] frames, at most
/// `CONTROL_SPAN_UNDER` short or `CONTROL_SPAN_OVER` long: the residual names the hole's first frame or
/// the one after, and the first frame back or the one after (63 to 65 frames), and the converters ring
/// past the residual threshold once the tone is back (on the rig's interface at 44.1 kHz the two
/// controls spanned 90 and 110 frames; the control's line prints the span). A dropout that lengthens the
/// hole by `CONTROL_SPAN_OVER` or less is hidden in it.
const CONTROL_SPAN_UNDER: Frame = 2;
const CONTROL_SPAN_OVER: Frame = 64;
/// The callbacks' log saw the run when it counted at least this share of the callbacks the watched span
/// holds at the status' block size: the callbacks at the span's edges fall outside it, and a log that
/// was on for half the span or less is no record of it.
const LOG_SHARE: f64 = 0.5;
/// Late entries printed one by one (every long callback is).
const LATE_LINES: usize = 64;
/// A snippet's printed frames: before the event's onset, and in all.
const SNIP_PRINT_BEFORE: Frame = 24;
const SNIP_PRINT: usize = 128;

pub(crate) const CUE_RESIDUAL: u8 = 1;
pub(crate) const CUE_PHASE: u8 = 2;
pub(crate) const CUE_AMPLITUDE: u8 = 4;

/// An angle in (-π, π].
fn wrap(x: f64) -> f64 {
    let t = x.rem_euclid(TAU);
    if t > PI { t - TAU } else { t }
}

/// One period of the tone: each table index's (sin, cos).
#[derive(Clone)]
struct Wave([(f64, f64); PERIOD]);

impl Wave {
    fn new() -> Wave {
        Wave(std::array::from_fn(|i| (TAU * i as f64 / PERIOD as f64).sin_cos()))
    }

    /// The reference at device frame `frame`: (sin, cos) of its phase.
    fn at(&self, frame: Frame) -> (f64, f64) {
        self.0[(CYCLES as Frame * frame).rem_euclid(PERIOD as Frame) as usize]
    }
}

/// What the calibration found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cal {
    Pending = 0,
    Ok = 1,
    NoTone = 2,
    Clipped = 3,
    Unsteady = 4,
}

impl Cal {
    fn of(code: u8) -> Cal {
        [Cal::Pending, Cal::Ok, Cal::NoTone, Cal::Clipped, Cal::Unsteady].into_iter().find(|c| *c as u8 == code).unwrap_or(Cal::Pending)
    }

    /// Why the tone cannot be watched, or `None` for a calibrated one.
    pub(crate) fn fault(self) -> Option<&'static str> {
        match self {
            Cal::Ok => None,
            Cal::Pending => Some("the tone never calibrated"),
            Cal::NoTone => Some("no tone on the input (check the cable, --tone and --out)"),
            Cal::Clipped => Some("the input clipped"),
            Cal::Unsteady => Some("the input is no steady tone (its scatter puts a threshold past a one-frame slip or the control's hole)"),
        }
    }
}

/// The calibration's result: the tone at the input, its noise, and the thresholds frozen from them.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Learned {
    /// The reference: amplitude, and phase against the generated tone (the round trip's).
    pub(crate) amp: f64,
    pub(crate) phase: f64,
    /// The residual's RMS in parts of the amplitude, and the windows' scatter: phase in radians,
    /// amplitude in parts of it.
    pub(crate) residual_rms: f64,
    pub(crate) phase_sd: f64,
    pub(crate) amp_sd: f64,
    pub(crate) residual_bar: f64,
    pub(crate) phase_bar: f64,
    pub(crate) amp_bar: f64,
}

/// One loopback discontinuity: every cue that fired within [`MERGE_SECONDS`] of the one before.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ToneEvent {
    /// The first and last device frame a cue fired at: the residual's frame, or a window's first and last.
    pub(crate) onset: Frame,
    pub(crate) end: Frame,
    /// `CUE_*` bits.
    pub(crate) cues: u8,
    /// The largest residual, in parts of the amplitude.
    pub(crate) peak_residual: f64,
    /// The lasting phase step, in radians: positive when the input ran ahead (frames lost on the way),
    /// negative when it fell behind (frames repeated). 0 when the phase came back.
    pub(crate) phase_step: f64,
    /// The smallest window amplitude, in parts of the reference.
    pub(crate) min_amp: f64,
    /// The first and last frame the residual cue fired at (with `CUE_RESIDUAL`).
    pub(crate) residual_from: Frame,
    pub(crate) residual_to: Frame,
    /// The reference phase when the event began.
    from_phase: f64,
}

impl ToneEvent {
    /// The phase step as a slip in frames, modulo the tone's period of 44.5: in (-22.25, 22.25].
    pub(crate) fn slip_frames(&self) -> f64 {
        self.phase_step / TAU * CYCLE_FRAMES
    }

    fn cues_text(&self) -> String {
        let names = [(CUE_RESIDUAL, "residual"), (CUE_PHASE, "phase"), (CUE_AMPLITUDE, "amplitude")];
        names.iter().filter(|(bit, _)| self.cues & bit != 0).map(|(_, name)| *name).collect::<Vec<_>>().join("+")
    }
}

/// Frames the detector did not watch: a slice whose input never reached the engine (`jump` false), or
/// frames no hook ran for (a callback that missed the engine lock or faulted: the frame counter jumped).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Gap {
    pub(crate) frame: Frame,
    pub(crate) frames: Frame,
    pub(crate) jump: bool,
}

/// The raw input around an event: `data[..len]` from device frame `first`.
#[derive(Clone)]
struct Snippet {
    event: usize,
    first: Frame,
    len: usize,
    data: [f32; 2 * SNIP_HALF],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Off,
    Settle,
    Calibrate,
    Watch,
    Done,
}

/// The tone's watcher. Two cues, either of which opens an event:
/// - the recurrence residual `x[n] − 2cos(w)·x[n−1] + x[n−2]`, zero for a clean sine, a spike at a splice
///   (it names the frame, but is blind inside a zeroed span and weak for a slip near a whole period);
/// - a quadrature fit of each [`WINDOW`] against the generated tone's phase: input and output share the
///   device's clock, so amplitude and phase are constants once settled. A frame lost or repeated anywhere
///   on the round trip moves the phase for good; a hole or a dip lowers the amplitude.
///
/// It settles, learns the tone and its noise over the calibration, and freezes the thresholds. After a
/// lasting phase step it records the step in the event and takes the new phase as its reference, so one
/// slip is one event and the next is still seen; the amplitude reference never moves. Everything is
/// preallocated: [`Detector::push`] runs in the output callback.
#[derive(Clone)]
pub(crate) struct Detector {
    wave: Wave,
    /// 2cos(w), the recurrence's coefficient.
    c2: f64,
    mode: Mode,
    cal: Cal,
    settle_until: Frame,
    cal_until: Frame,
    merge: Frame,
    min_windows: u32,
    /// The frame the next slice starts at, when the callbacks run on.
    next: Option<Frame>,
    /// The two samples before this one, and how many of them follow on without a gap.
    x1: f64,
    x2: f64,
    history: u8,
    /// The window in progress: its first frame, frames, correlation sums and largest residual.
    win_start: Frame,
    win_n: usize,
    acc_sin: f64,
    acc_cos: f64,
    win_peak: f64,
    /// The calibration's sums: windows, the first one's phase, amplitude and phase offset (and squares),
    /// residual squares and their count.
    cal_n: u32,
    phase0: f64,
    sum_amp: f64,
    sum_amp2: f64,
    sum_d: f64,
    sum_d2: f64,
    sum_r2: f64,
    r_n: u64,
    clipped: u64,
    learned: Learned,
    /// The phase reference (the learned one until a step moves it), the window before (its phase, and
    /// whether its amplitude was in band), and whether a flagged window still waits for a steady phase.
    ref_phase: f64,
    prev: Option<(f64, bool)>,
    disturbed: bool,
    events: Vec<ToneEvent>,
    n_events: usize,
    events_over: u64,
    /// The event in progress (`None` past the table's end) while `open`, and the last frame a cue fired at.
    cur: Option<usize>,
    open: bool,
    last_cue: Frame,
    gaps: Vec<Gap>,
    n_gaps: usize,
    gaps_over: u64,
    /// The last [`SNIP_HALF`] input samples, and the snippet still taking its second half.
    ring: [f32; SNIP_HALF],
    ring_n: usize,
    snippets: Vec<Snippet>,
    n_snippets: usize,
    filling: bool,
    /// The watched span, the frames the hook ran for in it and those that carried valid input.
    watch_from: Frame,
    watch_to: Frame,
    pushed: u64,
    observed: u64,
    /// (frame, `pushed`, `observed`) at the start of the window in progress, and at the start of the last
    /// whole one: where the watched span ends. A slip shows in the phase of the window after it, so only
    /// the frames before the last whole window were judged.
    mark: (Frame, u64, u64),
    claim: (Frame, u64, u64),
}

impl Detector {
    /// Allocates: build it off the audio thread.
    pub(crate) fn new() -> Detector {
        Detector {
            wave: Wave::new(),
            c2: 2.0 * (TAU * CYCLES as f64 / PERIOD as f64).cos(),
            mode: Mode::Off,
            cal: Cal::Pending,
            settle_until: 0,
            cal_until: 0,
            merge: 0,
            min_windows: 0,
            next: None,
            x1: 0.0,
            x2: 0.0,
            history: 0,
            win_start: 0,
            win_n: 0,
            acc_sin: 0.0,
            acc_cos: 0.0,
            win_peak: 0.0,
            cal_n: 0,
            phase0: 0.0,
            sum_amp: 0.0,
            sum_amp2: 0.0,
            sum_d: 0.0,
            sum_d2: 0.0,
            sum_r2: 0.0,
            r_n: 0,
            clipped: 0,
            learned: Learned::default(),
            ref_phase: 0.0,
            prev: None,
            disturbed: false,
            events: vec![ToneEvent::default(); EVENT_CAP],
            n_events: 0,
            events_over: 0,
            cur: None,
            open: false,
            last_cue: 0,
            gaps: vec![Gap::default(); GAP_CAP],
            n_gaps: 0,
            gaps_over: 0,
            ring: [0.0; SNIP_HALF],
            ring_n: 0,
            snippets: vec![Snippet { event: 0, first: 0, len: 0, data: [0.0; 2 * SNIP_HALF] }; SNIPPETS],
            n_snippets: 0,
            filling: false,
            watch_from: 0,
            watch_to: 0,
            pushed: 0,
            observed: 0,
            mark: (0, 0, 0),
            claim: (0, 0, 0),
        }
    }

    /// The tone plays at full level from device frame `full_from` at `rate`: settle, calibrate, watch.
    pub(crate) fn start(&mut self, rate: u32, full_from: Frame) {
        let frames = |seconds: f64| (seconds * rate as f64) as Frame;
        self.mode = Mode::Settle;
        self.settle_until = full_from + frames(SETTLE_SECONDS);
        self.cal_until = self.settle_until + frames(CALIBRATE_SECONDS);
        self.merge = frames(MERGE_SECONDS);
        // Half the windows the calibration's span holds: fewer, and gaps ate it.
        self.min_windows = (frames(CALIBRATE_SECONDS) / WINDOW as Frame / 2) as u32;
    }

    /// Stop watching (the tone is about to fade). The watched span ends where the last whole window
    /// began: the frames after it were never judged, and are not claimed.
    pub(crate) fn finish(&mut self) {
        if self.mode == Mode::Watch {
            (self.watch_to, self.pushed, self.observed) = self.claim;
        }
        if self.mode != Mode::Off {
            self.mode = Mode::Done;
        }
    }

    pub(crate) fn watching(&self) -> bool {
        self.mode == Mode::Watch
    }

    pub(crate) fn cal(&self) -> Cal {
        self.cal
    }

    pub(crate) fn learned(&self) -> &Learned {
        &self.learned
    }

    pub(crate) fn events(&self) -> &[ToneEvent] {
        &self.events[..self.n_events]
    }

    pub(crate) fn gaps(&self) -> &[Gap] {
        &self.gaps[..self.n_gaps]
    }

    /// Events and gaps seen so far, those past the tables included.
    fn counts(&self) -> (u64, u64) {
        (self.n_events as u64 + self.events_over, self.n_gaps as u64 + self.gaps_over)
    }

    /// One slice of the output callback: the cable's input from device frame `frame`. `valid` false: the
    /// slice's input never reached the callback (its samples are a zero fill), a coverage gap and no
    /// event. A `frame` that does not follow the slice before is a gap too: no hook ran in between.
    pub(crate) fn push(&mut self, frame: Frame, input: &[f32], valid: bool) {
        if matches!(self.mode, Mode::Off | Mode::Done) {
            return;
        }
        let len = input.len() as Frame;
        let counted = matches!(self.mode, Mode::Calibrate | Mode::Watch);
        if let Some(next) = self.next.replace(frame + len) {
            if frame != next {
                if counted {
                    self.gap(next, frame - next, true);
                }
                self.restart();
                // The snippets' frames run on from their first: none holds samples from both sides.
                self.ring_n = 0;
                self.filling = false;
            }
        }
        if !valid {
            if counted {
                self.gap(frame, len, false);
            }
            if self.mode == Mode::Watch {
                self.pushed += len as u64;
            }
            self.restart();
            for &x in input {
                self.keep(x);
            }
            return;
        }
        for (k, &x) in input.iter().enumerate() {
            self.sample(frame + k as Frame, x);
        }
    }

    /// Forget the run of samples in progress: the residual's history and the window.
    fn restart(&mut self) {
        self.history = 0;
        self.win_n = 0;
        self.acc_sin = 0.0;
        self.acc_cos = 0.0;
        self.win_peak = 0.0;
        self.prev = None;
    }

    fn gap(&mut self, frame: Frame, frames: Frame, jump: bool) {
        // Slice after slice without input is one gap.
        if let Some(last) = self.n_gaps.checked_sub(1).map(|i| &mut self.gaps[i]) {
            if last.jump == jump && last.frame + last.frames == frame && frames > 0 {
                last.frames += frames;
                return;
            }
        }
        if self.n_gaps < GAP_CAP {
            self.gaps[self.n_gaps] = Gap { frame, frames, jump };
            self.n_gaps += 1;
        } else {
            self.gaps_over += 1;
        }
    }

    /// Keep a sample for the snippets.
    fn keep(&mut self, x: f32) {
        self.ring[self.ring_n % SNIP_HALF] = x;
        self.ring_n += 1;
        if self.filling {
            let s = &mut self.snippets[self.n_snippets - 1];
            s.data[s.len] = x;
            s.len += 1;
            self.filling = s.len < s.data.len();
        }
    }

    fn sample(&mut self, frame: Frame, input: f32) {
        self.keep(input);
        if self.mode == Mode::Settle {
            if frame < self.settle_until {
                return;
            }
            self.mode = Mode::Calibrate;
            self.restart();
        }
        if input.abs() >= CLIP {
            self.clipped += 1;
        }
        let x = input as f64;
        let residual = if self.history >= 2 { (x - self.c2 * self.x1 + self.x2).abs() } else { 0.0 };
        (self.x2, self.x1) = (self.x1, x);
        if self.mode == Mode::Watch {
            self.pushed += 1;
            self.observed += 1;
            if residual > self.learned.residual_bar * self.learned.amp {
                self.cue(frame, frame, CUE_RESIDUAL);
                self.worst(residual, 1.0);
            }
        } else if self.history >= 2 {
            self.sum_r2 += residual * residual;
            self.r_n += 1;
        }
        self.history = (self.history + 1).min(2);
        self.win_peak = self.win_peak.max(residual);

        if self.win_n == 0 {
            self.win_start = frame;
            if self.mode == Mode::Watch {
                self.mark = (frame, self.pushed - 1, self.observed - 1);
            }
        }
        let (sin, cos) = self.wave.at(frame);
        self.acc_sin += x * sin;
        self.acc_cos += x * cos;
        self.win_n += 1;
        if self.win_n == WINDOW {
            self.window(frame);
            self.win_n = 0;
            self.acc_sin = 0.0;
            self.acc_cos = 0.0;
            self.win_peak = 0.0;
        }
    }

    /// A whole window ending at `end`: for x = a·sin(θ + φ) against the reference's θ, the sums are
    /// (a·W/2)·cos φ and (a·W/2)·sin φ.
    fn window(&mut self, end: Frame) {
        let amp = 2.0 / WINDOW as f64 * self.acc_sin.hypot(self.acc_cos);
        let phase = self.acc_cos.atan2(self.acc_sin);
        match self.mode {
            Mode::Calibrate => {
                if self.cal_n == 0 {
                    self.phase0 = phase;
                }
                let d = wrap(phase - self.phase0);
                self.cal_n += 1;
                self.sum_amp += amp;
                self.sum_amp2 += amp * amp;
                self.sum_d += d;
                self.sum_d2 += d * d;
                if end + 1 >= self.cal_until {
                    self.freeze(end + 1, phase);
                }
            }
            Mode::Watch => {
                self.claim = self.mark;
                let l = self.learned;
                let ratio = amp / l.amp;
                let level_ok = (ratio - 1.0).abs() <= l.amp_bar;
                let judged = ratio >= PHASE_AMP_FLOOR;
                let stepped = judged && wrap(phase - self.ref_phase).abs() > l.phase_bar;
                if !level_ok || stepped {
                    let cues = u8::from(!level_ok) * CUE_AMPLITUDE | u8::from(stepped) * CUE_PHASE;
                    self.cue(self.win_start, end, cues);
                    self.worst(self.win_peak, ratio);
                    self.disturbed = true;
                }
                if self.disturbed {
                    // Steady again: the level in band and the phase where the window before had it.
                    let steady = level_ok && judged && self.prev.is_some_and(|(p, ok)| ok && wrap(phase - p).abs() <= l.phase_bar);
                    if steady {
                        if stepped {
                            // A lasting step: recorded, then the new phase is the reference.
                            self.ref_phase = phase;
                            if let Some(e) = self.cur.map(|i| &mut self.events[i]) {
                                e.phase_step = wrap(phase - e.from_phase);
                            }
                        }
                        self.disturbed = false;
                    }
                }
                self.prev = Some((phase, level_ok && judged));
            }
            _ => {}
        }
    }

    /// The calibration's end at device frame `at`: the tone, its noise, the thresholds, and whether it
    /// can be watched.
    fn freeze(&mut self, at: Frame, last_phase: f64) {
        let n = self.cal_n.max(1) as f64;
        let amp = self.sum_amp / n;
        let mean_d = self.sum_d / n;
        let per_amp = |x: f64| if amp > 0.0 { x / amp } else { 0.0 };
        let mut l = Learned {
            amp,
            phase: wrap(self.phase0 + mean_d),
            residual_rms: per_amp((self.sum_r2 / self.r_n.max(1) as f64).sqrt()),
            phase_sd: (self.sum_d2 / n - mean_d * mean_d).max(0.0).sqrt(),
            amp_sd: per_amp((self.sum_amp2 / n - amp * amp).max(0.0).sqrt()),
            ..Learned::default()
        };
        l.residual_bar = (RES_SIGMAS * l.residual_rms).max(RES_MIN);
        l.phase_bar = (PHASE_SIGMAS * l.phase_sd).max(PHASE_MIN);
        l.amp_bar = (AMP_SIGMAS * l.amp_sd).max(AMP_MIN);
        self.learned = l;
        self.cal = if self.clipped > 0 {
            Cal::Clipped
        } else if amp < MIN_LEVEL {
            Cal::NoTone
        } else if self.cal_n < self.min_windows || l.phase_bar > PHASE_MAX || l.amp_bar > AMP_MAX {
            Cal::Unsteady
        } else {
            Cal::Ok
        };
        if self.cal == Cal::Ok {
            self.mode = Mode::Watch;
            self.watch_from = at;
            self.watch_to = at;
            self.claim = (at, 0, 0);
            self.ref_phase = l.phase;
            self.prev = Some((last_phase, true));
        } else {
            self.mode = Mode::Done;
        }
    }

    /// A cue fired over frames `from..=to` (the sample in hand is `to`): it joins the event in progress
    /// when it comes within the merge span of that one's last cue, else it opens one.
    fn cue(&mut self, from: Frame, to: Frame, cues: u8) {
        if !(self.open && from - self.last_cue <= self.merge) {
            self.open = true;
            self.last_cue = to;
            if self.n_events < EVENT_CAP {
                self.events[self.n_events] = ToneEvent { onset: from, end: to, min_amp: 1.0, from_phase: self.ref_phase, ..ToneEvent::default() };
                self.cur = Some(self.n_events);
                self.n_events += 1;
                self.snip(to);
            } else {
                self.events_over += 1;
                self.cur = None;
            }
        }
        self.last_cue = self.last_cue.max(to);
        if let Some(e) = self.cur.map(|i| &mut self.events[i]) {
            e.end = e.end.max(to);
            if cues & CUE_RESIDUAL != 0 {
                if e.cues & CUE_RESIDUAL == 0 {
                    e.residual_from = to;
                }
                e.residual_to = to;
            }
            e.cues |= cues;
        }
    }

    /// Fold a residual and a window's amplitude ratio into the event in progress.
    fn worst(&mut self, residual: f64, ratio: f64) {
        let amp = self.learned.amp;
        if let Some(e) = self.cur.map(|i| &mut self.events[i]) {
            e.peak_residual = e.peak_residual.max(residual / amp);
            e.min_amp = e.min_amp.min(ratio);
        }
    }

    /// Start the new event's snippet from the ring: the samples up to `now`, the next ones to follow.
    fn snip(&mut self, now: Frame) {
        if self.n_snippets == SNIPPETS {
            return;
        }
        let k = self.ring_n.min(SNIP_HALF);
        let s = &mut self.snippets[self.n_snippets];
        for (i, y) in s.data[..k].iter_mut().enumerate() {
            *y = self.ring[(self.ring_n - k + i) % SNIP_HALF];
        }
        (s.event, s.first, s.len) = (self.n_events - 1, now + 1 - k as Frame, k);
        self.n_snippets += 1;
        self.filling = true;
    }
}

/// One output callback that ran long or entered late.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Slow {
    /// Its first device frame and its frames.
    pub(crate) frame: Frame,
    pub(crate) frames: u32,
    pub(crate) nanos: u64,
    /// Frames behind the wakes' best phase at entry, when more than a period (`callback::PhaseSlips`).
    pub(crate) late: u32,
    /// It took its whole period or more (`BlockLoad::over_budget`'s rule).
    pub(crate) long: bool,
}

struct SlowRow {
    frame: AtomicI64,
    frames: AtomicU32,
    nanos: AtomicU64,
    late: AtomicU32,
    long: AtomicBool,
}

/// The output callbacks that ran long (always) or entered [`LATE_PERIODS`] periods late or more, while
/// the tone is watched. `Render::render` writes it outside the engine lock, so it is atomics: one writer
/// (the output callback), read by the probe once the device has closed. Late entries leave the last
/// [`LONG_RESERVE`] rows to the long callbacks.
pub(crate) struct SlowLog {
    on: AtomicBool,
    /// Every callback it saw while on: the proof that it recorded.
    seen: AtomicU64,
    len: AtomicUsize,
    /// Every long callback and late entry while on, those past the table included, and those past it.
    long: AtomicU64,
    late: AtomicU64,
    long_over: AtomicU64,
    late_over: AtomicU64,
    rows: Box<[SlowRow]>,
}

impl SlowLog {
    fn new() -> SlowLog {
        let row = |_| SlowRow { frame: AtomicI64::new(0), frames: AtomicU32::new(0), nanos: AtomicU64::new(0), late: AtomicU32::new(0), long: AtomicBool::new(false) };
        SlowLog {
            on: AtomicBool::new(false),
            seen: AtomicU64::new(0),
            len: AtomicUsize::new(0),
            long: AtomicU64::new(0),
            late: AtomicU64::new(0),
            long_over: AtomicU64::new(0),
            late_over: AtomicU64::new(0),
            rows: (0..SLOW_CAP).map(row).collect(),
        }
    }

    /// One output callback: `frames` from device frame `frame`, `elapsed` long, entered `late` frames
    /// behind the best phase (0 within a period). Never allocates or waits.
    pub(crate) fn record(&self, frame: Frame, frames: usize, elapsed: Duration, late: u64, rate: u32) {
        if frames == 0 || !self.on.load(Relaxed) {
            return;
        }
        self.seen.fetch_add(1, Relaxed);
        let long = elapsed.as_secs_f64() * rate as f64 >= frames as f64;
        let late_entry = late >= LATE_PERIODS * frames as u64;
        if !long && !late_entry {
            return;
        }
        self.long.fetch_add(u64::from(long), Relaxed);
        self.late.fetch_add(u64::from(late_entry), Relaxed);
        let len = self.len.load(Relaxed);
        if len >= if long { SLOW_CAP } else { SLOW_CAP - LONG_RESERVE } {
            let over = if long { &self.long_over } else { &self.late_over };
            over.fetch_add(1, Relaxed);
            return;
        }
        let row = &self.rows[len];
        row.frame.store(frame, Relaxed);
        row.frames.store(frames as u32, Relaxed);
        row.nanos.store(elapsed.as_nanos() as u64, Relaxed);
        row.late.store(late.min(u32::MAX as u64) as u32, Relaxed);
        row.long.store(long, Relaxed);
        self.len.store(len + 1, Relaxed);
    }

    fn rows(&self) -> Vec<Slow> {
        self.rows[..self.len.load(Relaxed)]
            .iter()
            .map(|r| Slow { frame: r.frame.load(Relaxed), frames: r.frames.load(Relaxed), nanos: r.nanos.load(Relaxed), late: r.late.load(Relaxed), long: r.long.load(Relaxed) })
            .collect()
    }
}

/// What the probe's thread and the callbacks share for a tone run (`Core::tone`): the probe's orders,
/// the rig's progress, and the callbacks' [`SlowLog`].
pub(crate) struct Shared {
    rate: AtomicU32,
    start: AtomicBool,
    stop: AtomicBool,
    state: AtomicU8,
    cal: AtomicU8,
    events: AtomicU64,
    gaps: AtomicU64,
    pub(crate) slow: SlowLog,
}

const IDLE: u8 = 0;
const RUNNING: u8 = 1;
const WATCHING: u8 = 2;
const DONE: u8 = 3;

impl Shared {
    /// Allocates the callbacks' table.
    pub(crate) fn new() -> Arc<Shared> {
        Arc::new(Shared {
            rate: AtomicU32::new(0),
            start: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            state: AtomicU8::new(IDLE),
            cal: AtomicU8::new(Cal::Pending as u8),
            events: AtomicU64::new(0),
            gaps: AtomicU64::new(0),
            slow: SlowLog::new(),
        })
    }

    /// Start the tone at the next output callback, on a device that runs at `rate`.
    pub(crate) fn begin(&self, rate: u32) {
        self.rate.store(rate, Relaxed);
        // Published after the rate: the hook's `Acquire` reads the rate stored before it.
        self.start.store(true, Release);
    }

    /// Plant the second control, then fade the tone out.
    pub(crate) fn end(&self) {
        self.stop.store(true, Relaxed);
    }

    pub(crate) fn done(&self) -> bool {
        self.state.load(Relaxed) == DONE
    }

    pub(crate) fn cal(&self) -> Cal {
        Cal::of(self.cal.load(Relaxed))
    }

    /// Events (the controls among them) and coverage gaps so far.
    pub(crate) fn counts(&self) -> (u64, u64) {
        (self.events.load(Relaxed), self.gaps.load(Relaxed))
    }
}

/// The tone's hook in the output callback (`Rt::tone`), set while no device runs. From the probe's
/// `begin` it adds the sine to one output side, after the engine's render and the master fade, ramps it
/// in, feeds the detector the cable's input, plants a control once the detector watches (a [`HOLE`] in the
/// output tone) and another on the probe's `end`, then ramps out. Its gain and phase are functions of the
/// device frame, so the cable never carries a step but the controls'.
pub(crate) struct ToneRig {
    shared: Arc<Shared>,
    right: bool,
    wave: Wave,
    det: Detector,
    state: u8,
    rate: u32,
    /// The ramp in's first frame, a ramp's frames, and the ramp out's first frame once `end` was seen.
    from: Frame,
    ramp: Frame,
    out_from: Option<Frame>,
    /// Each control's first frame, once planted.
    holes: [Option<Frame>; 2],
}

impl ToneRig {
    /// Allocates: build it off the audio thread. `right`: the output side the cable leaves from.
    pub(crate) fn new(shared: Arc<Shared>, right: bool) -> ToneRig {
        ToneRig { shared, right, wave: Wave::new(), det: Detector::new(), state: IDLE, rate: 0, from: 0, ramp: 1, out_from: None, holes: [None; 2] }
    }

    fn frames(&self, seconds: f64) -> Frame {
        (seconds * self.rate as f64) as Frame
    }

    /// The tone's gain at `frame`: the ramps, and nothing inside a control's hole.
    fn gain(&self, frame: Frame) -> f32 {
        if self.holes.iter().flatten().any(|&h| (h..h + HOLE).contains(&frame)) {
            return 0.0;
        }
        let up = (frame - self.from) as f32 / self.ramp as f32;
        let down = self.out_from.map_or(1.0, |out| 1.0 - (frame - out) as f32 / self.ramp as f32);
        up.min(down).clamp(0.0, 1.0)
    }

    /// One slice of the output callback: `input` the cable's input from device frame `frame` (`valid`
    /// false: a zero fill, the slice's input never arrived), `left` and `right` what the engine rendered.
    pub(crate) fn block(&mut self, frame: Frame, input: &[f32], valid: bool, left: &mut [f32], right: &mut [f32]) {
        match self.state {
            IDLE => {
                if !self.shared.start.swap(false, Acquire) {
                    return;
                }
                self.rate = self.shared.rate.load(Relaxed).max(1);
                self.from = frame;
                self.ramp = self.frames(RAMP_SECONDS).max(1);
                self.det.start(self.rate, frame + self.ramp);
                self.state = RUNNING;
            }
            DONE => return,
            _ => {}
        }
        let end = frame + input.len() as Frame;
        if self.out_from.is_none() && self.shared.stop.load(Relaxed) {
            if self.det.watching() {
                self.holes[1] = Some(frame);
            }
            self.out_from = Some(frame + self.frames(TAIL_SECONDS));
        }
        if self.out_from.is_some_and(|out| end > out) {
            self.det.finish();
            self.shared.slow.on.store(false, Relaxed);
        }
        let out = if self.right { right } else { left };
        for (k, y) in out.iter_mut().enumerate() {
            let f = frame + k as Frame;
            *y += LEVEL * self.gain(f) * self.wave.at(f).0 as f32;
        }
        let watched = self.det.watching();
        self.det.push(frame, input, valid);
        if !watched && self.det.watching() {
            self.holes[0] = Some(end + self.frames(CONTROL_AFTER_SECONDS));
            self.shared.slow.on.store(true, Relaxed);
        }
        self.state = match self.out_from {
            Some(out) if end >= out + self.ramp => DONE,
            _ if self.det.watching() => WATCHING,
            _ => RUNNING,
        };
        let (events, gaps) = self.det.counts();
        self.shared.events.store(events, Relaxed);
        self.shared.gaps.store(gaps, Relaxed);
        self.shared.cal.store(self.det.cal() as u8, Relaxed);
        self.shared.state.store(self.state, Relaxed);
    }
}

/// What the probe knows of the run the tone played in.
#[derive(Clone, Debug)]
pub(crate) struct Facts {
    /// The alignment the engine rendered with (the driver's input plus output latency), in frames.
    pub(crate) align: Frame,
    /// Frames per output callback.
    pub(crate) block: u32,
    /// The device frame the soak began at.
    pub(crate) soak_from: Frame,
    /// What interrupted the device while the tone played (a device event, another backend, block or
    /// rate at the tone's end): the run is no measurement then.
    pub(crate) interrupted: Option<String>,
}

/// The tone's closing block (`lines`) and its check: `pass` with `verdict` "tone: clean …", or a failed one with
/// "tone: discontinuities …" (the tone broke) or "tone: not measured …" (the instrument did not work).
pub(crate) struct Report {
    pub(crate) lines: Vec<String>,
    pub(crate) pass: bool,
    pub(crate) verdict: String,
}

/// A planted control: where it left, and the event it came back as.
#[derive(Clone, Copy, Debug, Default)]
struct Control {
    planted: Option<Frame>,
    event: Option<usize>,
    /// It came back within the tolerance of the alignment.
    found: bool,
    /// Its event holds more than the hole: something else merged into it.
    mixed: bool,
    /// The frames its residual cues span (the hole's, for a control that came back alone).
    span: Frame,
}

/// The slow callback nearest an event: its row, the event's onset against its first frame as it is (the
/// input's side) and less the round trip (the output's), in frames, and whether either is in the window.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Near {
    pub(crate) index: usize,
    pub(crate) out: Frame,
    pub(crate) input: Frame,
    pub(crate) inside: bool,
}

/// An event seen at input frame `onset` against a callback from frame `slow`: a defect that callback
/// left in its output comes back `round_trip` frames later, one on the input side arrives with it.
fn distances(onset: Frame, round_trip: Frame, slow: Frame) -> (Frame, Frame) {
    (onset - round_trip - slow, onset - slow)
}

pub(crate) fn nearest(onset: Frame, round_trip: Frame, slow: &[Slow], window: Frame) -> Option<Near> {
    slow.iter()
        .enumerate()
        .map(|(index, s)| {
            let (out, input) = distances(onset, round_trip, s.frame);
            Near { index, out, input, inside: out.abs().min(input.abs()) <= window }
        })
        .min_by_key(|n| n.out.abs().min(n.input.abs()))
}

impl ToneRig {
    /// Each control against the events: the first event from the frame it left to
    /// [`CONTROL_SEARCH_SECONDS`] after.
    fn controls(&self, align: Frame) -> [Control; 2] {
        let events = self.det.events();
        let tolerance = WINDOW as Frame + self.frames(CONTROL_TOLERANCE_MS / 1e3);
        let mut taken = None;
        self.holes.map(|planted| {
            let mut c = Control { planted, ..Control::default() };
            let Some(p) = planted else { return c };
            c.event = events.iter().enumerate().position(|(i, e)| Some(i) != taken && (p..=p + self.frames(CONTROL_SEARCH_SECONDS)).contains(&e.onset));
            if let Some(e) = c.event.map(|i| &events[i]) {
                taken = c.event;
                c.found = (e.onset - p - align).abs() <= tolerance;
                // The hole itself: both its edges fire the residual, `HOLE` frames apart.
                c.span = if e.cues & CUE_RESIDUAL != 0 { e.residual_to - e.residual_from } else { 0 };
                let hole_alone = (HOLE - CONTROL_SPAN_UNDER..=HOLE + CONTROL_SPAN_OVER).contains(&c.span);
                c.mixed = !hole_alone || e.phase_step != 0.0 || e.end - e.onset > HOLE + 3 * WINDOW as Frame;
            }
            c
        })
    }

    /// The closing block and the check (the device has closed). `facts`: `None` when the tone never
    /// started on a device.
    pub(crate) fn report(&self, facts: Option<Facts>) -> Report {
        let Some(facts) = facts.filter(|_| self.rate > 0) else {
            return Report { lines: Vec::new(), pass: false, verdict: "tone: not measured: the tone never started".to_string() };
        };
        let (det, rate, slow) = (&self.det, self.rate as f64, &self.shared.slow);
        let ms = |frames: Frame| frames as f64 * 1e3 / rate;
        let db = |x: f64| 20.0 * x.max(1e-9).log10();
        let l = det.learned();
        let mut lines = Vec::new();
        lines.push(format!(
            "tone: {} Hz, {:.1} Hz ({CYCLES} cycles in {PERIOD} frames), out {LEVEL:.3} ({:.1} dBFS), in {:.4} ({:.1} dBFS); noise: residual rms {:.5} of the level, a window's phase sd {:.5} rad, level sd {:.5}",
            self.rate,
            rate * CYCLES as f64 / PERIOD as f64,
            db(LEVEL as f64),
            l.amp,
            db(l.amp),
            l.residual_rms,
            l.phase_sd,
            l.amp_sd
        ));
        lines.push(format!(
            "tone: thresholds, frozen: residual {:.4} of the level, phase {:.4} rad ({:.2} frames), level ±{:.3}; a window is {WINDOW} frames, cues within {MERGE_SECONDS} s are one event",
            l.residual_bar,
            l.phase_bar,
            l.phase_bar / TAU * CYCLE_FRAMES,
            l.amp_bar
        ));
        let span = det.watch_to - det.watch_from;
        lines.push(format!(
            "tone: watched {span} frames ({:.1} s) from frame {}: generated {}, observed {}, coverage gaps {}",
            span as f64 / rate,
            det.watch_from,
            det.pushed,
            det.observed,
            det.counts().1
        ));

        let controls = self.controls(facts.align);
        let events = det.events();
        for (k, c) in controls.iter().enumerate() {
            let text = match (c.planted, c.event.map(|i| &events[i])) {
                (None, _) => "not planted".to_string(),
                (Some(p), None) => format!("planted at frame {p}, NOT FOUND"),
                (Some(p), Some(e)) => format!(
                    "planted at frame {p}, found at {}, offset {} frames ({:.2} ms), alignment {} frames, its residual cues span {} frames (the hole's {HOLE}, to {CONTROL_SPAN_OVER} more): {}{}",
                    e.onset,
                    e.onset - p,
                    ms(e.onset - p),
                    facts.align,
                    c.span,
                    if c.found { "found" } else { "OFF the alignment" },
                    if c.mixed { ", and its event holds more than the hole" } else { "" }
                ),
            };
            lines.push(format!("tone: control {} ({HOLE} frames of silence in the output tone, expected within ±{:.2} ms of the alignment) {text}", k + 1, CONTROL_TOLERANCE_MS + ms(WINDOW as Frame)));
        }
        // The first control's arrival is the round trip the coincidence check shifts by.
        let round_trip = controls[0].event.filter(|_| controls[0].found).map_or(facts.align, |i| events[i].onset - controls[0].planted.unwrap_or(0));
        let window = COINCIDENCE_PERIODS * facts.block as Frame + WINDOW as Frame;
        let rows = slow.rows();
        let control_of = |i: usize| controls.iter().position(|c| c.event == Some(i));
        let own: Vec<usize> = (0..events.len()).filter(|&i| control_of(i).is_none()).collect();
        let mixed = controls.iter().filter(|c| c.mixed).count();
        let spontaneous = own.len() + mixed;
        let coincide = own.iter().filter(|&&i| nearest(events[i].onset, round_trip, &rows, window).is_some_and(|n| n.inside)).count();
        let (long, late) = (slow.long.load(Relaxed), slow.late.load(Relaxed));
        let (long_over, late_over) = (slow.long_over.load(Relaxed), slow.late_over.load(Relaxed));
        let seen = slow.seen.load(Relaxed);
        let seen_floor = (LOG_SHARE * span as f64 / facts.block.max(1) as f64) as u64;
        lines.push(format!(
            "tone: {spontaneous} loopback discontinuities beside the controls, {seen} callbacks logged, {long} long (a period or more), {late} late entries ({LATE_PERIODS} periods or more behind); past the tables: events {}, gaps {}, long callbacks {long_over}, late entries {late_over}; round trip {round_trip} frames, coincidence window ±{:.2} ms",
            det.events_over,
            det.gaps_over,
            ms(window)
        ));

        let slow_text = |s: &Slow| {
            format!(
                "{} at frame {} ({:+.3} s), {} frames, {:.3} ms ({:.0} %), entered {} frames behind the best phase",
                match (s.long, s.late as u64 >= LATE_PERIODS * s.frames as u64) {
                    (true, true) => "long and late",
                    (true, false) => "long",
                    _ => "late",
                },
                s.frame,
                (s.frame - facts.soak_from) as f64 / rate,
                s.frames,
                s.nanos as f64 / 1e6,
                s.nanos as f64 * rate / 1e7 / s.frames.max(1) as f64,
                s.late
            )
        };
        for (i, e) in events.iter().enumerate() {
            let near = match nearest(e.onset, round_trip, &rows, window) {
                Some(n) => format!(
                    "nearest callback: {}: {:+.2} ms after it on the output side (less the round trip), {:+.2} ms on the input side, {} the window",
                    slow_text(&rows[n.index]),
                    ms(n.out),
                    ms(n.input),
                    if n.inside { "inside" } else { "outside" }
                ),
                None => "no long or late callback in the run".to_string(),
            };
            lines.push(format!(
                "tone event {}{}: {:+.3} s, frames {}..{} ({}), cues {}, peak residual {:.3}, phase step {:+.3} rad ({:+.2} frames modulo {CYCLE_FRAMES}), min level {:.3}; {near}",
                i + 1,
                control_of(i).map_or(String::new(), |k| format!(" (control {})", k + 1)),
                (e.onset - facts.soak_from) as f64 / rate,
                e.onset,
                e.end,
                e.end - e.onset + 1,
                e.cues_text(),
                e.peak_residual,
                e.phase_step,
                e.slip_frames(),
                e.min_amp
            ));
        }
        // A callback is at an event when it falls within the window of the event's span, on either side.
        let matched = |s: &Slow| {
            own.iter().find(|&&i| {
                let e = &events[i];
                [0, round_trip].iter().any(|shift| (e.onset - window..=e.end + window).contains(&(s.frame + shift)))
            })
        };
        let (mut late_lines, mut late_more, mut late_more_matched) = (0, 0, 0);
        for s in &rows {
            let event = matched(s);
            if !s.long {
                late_lines += 1;
                if late_lines > LATE_LINES {
                    late_more += 1;
                    late_more_matched += usize::from(event.is_some());
                    continue;
                }
            }
            lines.push(format!("tone callback: {}: {}", slow_text(s), event.map_or("no event".to_string(), |i| format!("event {}", i + 1))));
        }
        if late_more > 0 {
            lines.push(format!("tone callback: {late_more} more late entries, {late_more_matched} at an event"));
        }
        for g in det.gaps() {
            lines.push(format!(
                "tone gap: {} frames from frame {} ({:+.3} s): {}",
                g.frames,
                g.frame,
                (g.frame - facts.soak_from) as f64 / rate,
                if g.jump { "no callback carried the tone (the frame counter jumped)" } else { "the callback had no input" }
            ));
        }
        for s in &det.snippets[..det.n_snippets] {
            let onset = events[s.event].onset;
            let from = ((onset - SNIP_PRINT_BEFORE - s.first).max(0) as usize).min(s.len);
            let shown = &s.data[from..s.len.min(from + SNIP_PRINT)];
            lines.push(format!(
                "tone snippet, event {}: the input from frame {} (the onset is {}), % of the level: {}",
                s.event + 1,
                s.first + from as Frame,
                onset,
                shown.iter().map(|&x| format!("{:.0}", x as f64 / l.amp.max(1e-9) * 100.0)).collect::<Vec<_>>().join(" ")
            ));
        }

        // The instrument first: a run it did not measure is neither clean nor broken.
        let mut broken: Vec<String> = Vec::new();
        if let Some(reason) = &facts.interrupted {
            broken.push(format!("the device was interrupted while the tone played ({reason})"));
        }
        if let Some(fault) = det.cal().fault() {
            broken.push(format!("{fault} (level {:.4}, {:.1} dBFS)", l.amp, db(l.amp)));
        } else {
            if self.state != DONE {
                broken.push("the tone did not play to its end".to_string());
            } else if det.observed as Frame != span || det.counts().1 > 0 {
                broken.push(format!("{} coverage gaps: {} of {span} frames observed", det.counts().1, det.observed));
            }
            if seen == 0 || seen < seen_floor {
                broken.push(format!("the callback log saw {seen} callbacks, under {seen_floor} ({LOG_SHARE} of the watched span's at {} frames): no record of the long and late ones", facts.block));
            }
            if det.clipped > 0 {
                broken.push(format!("the input clipped ({} samples)", det.clipped));
            }
            for (k, c) in controls.iter().enumerate() {
                if !c.found {
                    broken.push(format!("control {} {}", k + 1, if c.event.is_some() { "came back off the alignment" } else { "was not found" }));
                }
            }
            let over = [("events", det.events_over), ("gaps", det.gaps_over), ("long callbacks", long_over), ("late entries", late_over)];
            for (name, n) in over.into_iter().filter(|(_, n)| *n > 0) {
                broken.push(format!("{n} {name} past the table"));
            }
        }
        let (pass, verdict) = if !broken.is_empty() {
            (false, format!("tone: not measured: {}", broken.join("; ")))
        } else if spontaneous > 0 {
            let merged = if mixed > 0 { format!(", {mixed} merged into a control") } else { String::new() };
            (
                false,
                format!(
                    "tone: discontinuities: {spontaneous} loopback discontinuities, {coincide} within ±{:.2} ms of a long or late callback, {} not{merged}; the run had {long} long callbacks and {late} late entries",
                    ms(window),
                    own.len() - coincide
                ),
            )
        } else {
            (true, format!("tone: clean: both controls found, full coverage, 0 loopback discontinuities over {long} long callbacks and {late} late entries"))
        };
        Report { lines, pass, verdict }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A callback's frames in these tests.
    const BLOCK: usize = 64;
    /// The round trip of the synthetic cable, in frames, and its gain.
    const LAG: Frame = 311;
    const CABLE: f64 = 0.8;
    /// The device frame the synthetic runs start at: no frame 0 anywhere.
    const T0: Frame = 123_457;
    /// Callbacks of every kind of size against the 89-frame window: one frame, sizes that neither
    /// divide it nor are its multiples, the window itself and its double.
    const MIXED: &[usize] = &[1, 7, 64, 89, 100, 128, 13, 178, 256, 3, 45];
    /// The reference phase the synthetic cable gives without an extra one.
    const NATURAL: f64 = -TAU * (CYCLES as Frame * LAG) as f64 / PERIOD as f64;

    fn facts(align: Frame, soak_from: Frame) -> Facts {
        Facts { align, block: BLOCK as u32, soak_from, interrupted: None }
    }

    #[derive(Clone, Copy, Debug)]
    enum Defect {
        /// Frames lost on the way: the input jumps ahead.
        Delete(Frame),
        /// Frames played twice: the input falls behind.
        Repeat(Frame),
        Zero(Frame),
        /// Faded to nothing and back: the span, and each ramp's frames.
        Dip(Frame, Frame),
    }

    /// A deterministic normal deviate (xorshift64*, Box-Muller).
    #[derive(Clone)]
    struct Noise(u64);

    impl Noise {
        fn uniform(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            ((self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 + 1.0) / (1u64 << 53) as f64
        }

        fn normal(&mut self) -> f64 {
            (-2.0 * self.uniform().ln()).sqrt() * (TAU * self.uniform()).cos()
        }
    }

    /// The tone as a cable returns it: `LAG` frames late at `CABLE` of its level, under noise of
    /// `sigma` RMS, with defects planted at input frames.
    #[derive(Clone)]
    struct Loop {
        wave: Wave,
        noise: Noise,
        sigma: f64,
        level: f64,
        defects: Vec<(Frame, Defect)>,
        det: Detector,
        next: Frame,
        /// The callbacks' sizes, in turn, and a phase added to the round trip's.
        slices: &'static [usize],
        turn: usize,
        extra: f64,
    }

    fn dbfs(db: f64) -> f64 {
        10f64.powf(db / 20.0)
    }

    impl Loop {
        fn new(rate: u32, sigma: f64) -> Loop {
            let mut det = Detector::new();
            det.start(rate, T0);
            Loop { wave: Wave::new(), noise: Noise(0x9E37_79B9_7F4A_7C15 ^ rate as u64), sigma, level: CABLE * LEVEL as f64, defects: Vec::new(), det, next: T0, slices: &[BLOCK], turn: 0, extra: 0.0 }
        }

        fn at(&mut self, frame: Frame) -> f32 {
            let (mut shift, mut gain) = (0, 1.0);
            for &(p, defect) in &self.defects {
                let k = frame - p;
                match defect {
                    Defect::Delete(d) if k >= 0 => shift += d,
                    Defect::Repeat(d) if k >= 0 => shift -= d,
                    Defect::Zero(d) if (0..d).contains(&k) => gain = 0.0,
                    Defect::Dip(d, ramp) if (0..d).contains(&k) => gain = (1.0 - (k.min(d - 1 - k) + 1) as f64 / ramp as f64).max(0.0),
                    _ => {}
                }
            }
            let (sin, cos) = self.wave.at(frame - LAG + shift);
            let clean = sin * self.extra.cos() + cos * self.extra.sin();
            (gain * self.level * clean + self.sigma * self.noise.normal()).clamp(-1.0, 1.0) as f32
        }

        /// Feed callbacks up to device frame `until`.
        fn run(&mut self, until: Frame) {
            while self.next < until {
                let n = self.slices[self.turn % self.slices.len()];
                self.turn += 1;
                let block: Vec<f32> = (0..n).map(|k| self.at(self.next + k as Frame)).collect();
                self.det.push(self.next, &block, true);
                self.next += n as Frame;
            }
        }

        /// A loop that has calibrated and watches.
        fn watching(rate: u32, sigma: f64) -> Loop {
            Loop::watching_with(rate, sigma, &[BLOCK], 0.0)
        }

        fn watching_with(rate: u32, sigma: f64, slices: &'static [usize], extra: f64) -> Loop {
            let mut l = Loop::new(rate, sigma);
            (l.slices, l.extra) = (slices, extra);
            l.run(T0 + 2 * rate as Frame);
            assert_eq!(l.det.cal(), Cal::Ok, "{rate} Hz: {:?}", l.det.learned());
            assert!(l.det.watching());
            l
        }
    }

    #[test]
    fn the_dip_ramps_to_nothing_and_back() {
        let mut l = Loop::new(48_000, 0.0);
        l.level = 1.0;
        l.defects.push((T0, Defect::Dip(64, 16)));
        let want = |k: Frame| match k {
            0..=15 => (15 - k) as f64 / 16.0,
            16..=47 => 0.0,
            48..=63 => (k - 48) as f64 / 16.0,
            _ => 1.0,
        };
        for k in -2..66 {
            let (x, clean) = (l.at(T0 + k) as f64, l.wave.at(T0 + k - LAG).0);
            assert!((x - want(k) * clean).abs() < 1e-6, "frame {k}: {x} of {clean}");
        }
    }

    #[test]
    fn a_clean_tone_has_no_event_in_a_minute() {
        for (rate, db) in [(44_100, -80.0), (48_000, -80.0), (48_000, -60.0)] {
            let mut l = Loop::watching(rate, dbfs(db));
            l.run(T0 + 62 * rate as Frame);
            l.det.finish();
            let d = &l.det;
            assert_eq!((d.counts(), d.clipped), ((0, 0), 0), "{rate} Hz at {db} dBFS: {:?}", d.events());
            assert_eq!(d.observed as Frame, d.watch_to - d.watch_from, "{rate} Hz: full coverage");
            assert!(d.observed > 60 * rate as u64);
            // The learned tone: the cable's level, the round trip's phase, and the floors while the noise is low.
            let learned = d.learned();
            assert!((learned.amp / (CABLE * LEVEL as f64) - 1.0).abs() < 1e-3, "{learned:?}");
            assert!(wrap(learned.phase - NATURAL).abs() < 1e-3, "{learned:?}");
            assert_eq!((learned.phase_bar, learned.amp_bar), (PHASE_MIN, AMP_MIN), "{learned:?}");
            assert_eq!(learned.residual_bar > RES_MIN, db > -70.0, "{learned:?}");
        }
    }

    /// The slip a phase step of `frames` reads as: modulo the tone's period, in (-22.25, 22.25].
    fn slip(frames: Frame) -> f64 {
        wrap(TAU * frames as f64 / CYCLE_FRAMES) / TAU * CYCLE_FRAMES
    }

    #[test]
    fn every_defect_is_one_event_wherever_it_starts() {
        for rate in [44_100, 48_000] {
            matrix(rate, &[BLOCK], 0.0, 32);
        }
    }

    #[test]
    fn every_defect_is_one_event_whatever_the_callbacks_sizes() {
        for rate in [44_100, 48_000] {
            matrix(rate, MIXED, 0.0, 32);
        }
    }

    #[test]
    fn a_round_trip_whose_phase_sits_at_the_wrap_calibrates_and_watches() {
        // On π (the windows' phases fall on both sides of the wrap) and just inside either side.
        for target in [PI, PI - 0.02, -PI + 0.02] {
            let extra = target - NATURAL;
            let mut l = Loop::watching_with(48_000, dbfs(-60.0), &[BLOCK], extra);
            assert!(wrap(l.det.learned().phase - target).abs() < 1e-3, "{target}: {:?}", l.det.learned());
            assert_eq!(l.det.learned().phase_bar, PHASE_MIN, "{target}: the wrap is no scatter");
            l.run(T0 + 12 * 48_000);
            assert_eq!(l.det.counts(), (0, 0), "{target}: {:?}", l.det.events());
            matrix(48_000, MIXED, extra, 8);
        }
    }

    /// Every defect of the matrix at `starts` places, each exactly one event with its slip.
    fn matrix(rate: u32, slices: &'static [usize], extra: f64, starts: Frame) {
        let sizes = [1, 64, 128, 192, 256];
        let mut defects: Vec<Defect> = sizes.iter().flat_map(|&d| [Defect::Delete(d), Defect::Repeat(d)]).collect();
        defects.extend([Defect::Zero(64), Defect::Zero(256), Defect::Dip(64, 16)]);
        {
            let base = Loop::watching_with(rate, dbfs(-80.0), slices, extra);
            // Starts 11 frames apart: as many phases of the tone, and places in the fit window.
            for start in 0..starts {
                for &defect in &defects {
                    let mut l = base.clone();
                    let at = l.next + 5000 + 11 * start;
                    l.defects.push((at, defect));
                    l.run(at + rate as Frame / 2);
                    l.det.finish();
                    let what = format!("{rate} Hz, slices {slices:?}, phase +{extra:.3}, {defect:?} at start {start}");
                    let events = l.det.events();
                    assert_eq!(events.len(), 1, "{what}: {events:?}");
                    let e = events[0];
                    // The onset within a window before the defect (a window's cue names its first frame), and
                    // at a splice or a hole no later than the residual's two frames; a fade has no edge.
                    let after = if let Defect::Dip(_, ramp) = defect { ramp + WINDOW as Frame } else { 2 };
                    assert!((at - WINDOW as Frame..=at + after).contains(&e.onset), "{what}: onset {} for {at}", e.onset);
                    let want = match defect {
                        Defect::Delete(d) => slip(d),
                        Defect::Repeat(d) => slip(-d),
                        _ => 0.0,
                    };
                    assert!((e.slip_frames() - want).abs() < 0.05, "{what}: slip {} frames, not {want}", e.slip_frames());
                    match defect {
                        Defect::Delete(_) | Defect::Repeat(_) => assert!(e.cues & CUE_PHASE != 0, "{what}: {e:?}"),
                        Defect::Zero(d) => {
                            assert!(e.cues & (CUE_AMPLITUDE | CUE_RESIDUAL) == CUE_AMPLITUDE | CUE_RESIDUAL && e.end - e.onset >= d - 1, "{what}: {e:?}");
                            assert!(e.min_amp < 0.7 && (d < 256 || e.min_amp < 0.01), "{what}: {e:?}");
                        }
                        Defect::Dip(..) => assert!(e.cues & CUE_AMPLITUDE != 0 && e.min_amp < 0.8, "{what}: {e:?}"),
                    }
                    assert_eq!(l.det.counts().1, 0, "{what}");
                }
            }
        }
    }

    #[test]
    fn a_slip_is_tracked_so_the_next_one_is_seen() {
        let rate = 48_000;
        for (apart, want) in [(rate as Frame, 2), (rate as Frame / 20, 1)] {
            let mut l = Loop::watching(rate, dbfs(-80.0));
            let at = l.next + 5000;
            l.defects.extend([(at, Defect::Delete(64)), (at + apart, Defect::Delete(1))]);
            l.run(at + 2 * rate as Frame);
            let events = l.det.events();
            assert_eq!(events.len(), want, "{apart} frames apart: {events:?}");
            // Apart: each event its own step. Merged: the two steps in one.
            let slips: Vec<f64> = events.iter().map(|e| e.slip_frames()).collect();
            let wants = if want == 2 { vec![slip(64), slip(1)] } else { vec![slip(65)] };
            assert!(slips.iter().zip(&wants).all(|(s, w)| (s - w).abs() < 0.05), "{apart} frames apart: {slips:?}, not {wants:?}");
        }
    }

    #[test]
    fn a_missing_a_clipped_and_an_unsteady_tone_are_named() {
        let rate = 48_000;
        let run = |level: f64, sigma: f64, wobble: bool| {
            let mut l = Loop::new(rate, sigma);
            l.level = level;
            if wobble {
                // A frame lost every 20 ms: no steady phase to learn.
                l.defects.extend((0..200).map(|k| (T0 + 960 * k, Defect::Delete(1))));
            }
            l.run(T0 + 3 * rate as Frame);
            l.det
        };
        let silent = run(0.0, dbfs(-60.0), false);
        assert_eq!((silent.cal(), silent.watching(), silent.counts()), (Cal::NoTone, false, (0, 0)));
        assert_eq!(run(1.2, dbfs(-80.0), false).cal(), Cal::Clipped);
        assert_eq!(run(CABLE * LEVEL as f64, dbfs(-80.0), true).cal(), Cal::Unsteady);
        assert_eq!(run(CABLE * LEVEL as f64, dbfs(-80.0), false).cal(), Cal::Ok);
        assert!(Cal::NoTone.fault().is_some_and(|f| f.contains("no tone")) && Cal::Clipped.fault().is_some_and(|f| f.contains("clipped")));
        assert!(Cal::Pending.fault().is_some() && Cal::Ok.fault().is_none());
    }

    #[test]
    fn a_slice_without_input_and_a_frame_jump_are_coverage_gaps_and_no_events() {
        let rate = 48_000;
        let mut l = Loop::watching(rate, dbfs(-80.0));
        let from = l.det.watch_from;
        l.run(l.next + 4800);
        // Two callbacks whose input never arrived: the zero fill, flagged.
        let invalid = l.next;
        l.det.push(l.next, &[0.0; BLOCK], false);
        l.det.push(l.next + BLOCK as Frame, &[0.0; BLOCK], false);
        l.next += 2 * BLOCK as Frame;
        l.run(l.next + 4800);
        // A callback no hook ran for: the frame counter moves on, the cable's tone with it.
        let jump = l.next;
        l.next += BLOCK as Frame;
        l.run(l.next + 4800);
        l.det.finish();
        let d = &l.det;
        assert_eq!(d.events().len(), 0, "{:?}", d.events());
        let gaps: Vec<(Frame, Frame, bool)> = d.gaps().iter().map(|g| (g.frame, g.frames, g.jump)).collect();
        assert_eq!(gaps, [(invalid, 2 * BLOCK as Frame, false), (jump, BLOCK as Frame, true)]);
        // The hook ran for the invalid slices and not for the jump; neither was observed.
        let span = (d.watch_to - from) as u64;
        assert_eq!((d.pushed, d.observed), (span - BLOCK as u64, span - 3 * BLOCK as u64));
    }

    #[test]
    fn an_event_coincides_with_a_callback_a_round_trip_before_it_or_at_it() {
        let slow = |frame| Slow { frame, frames: 64, nanos: 2_000_000, late: 0, long: true };
        let rows = [slow(10_000), slow(200_000)];
        let window = COINCIDENCE_PERIODS * 64 + WINDOW as Frame;
        // The output's side: the defect comes back a round trip after the callback.
        let n = nearest(10_000 + LAG + 20, LAG, &rows, window).unwrap();
        assert_eq!((n.index, n.out, n.input, n.inside), (0, 20, LAG + 20, true));
        // The input's side: it arrives with the callback.
        let n = nearest(200_000 - 30, LAG, &rows, window).unwrap();
        assert_eq!((n.index, n.out, n.input, n.inside), (1, -30 - LAG, -30, true));
        // Half a second away from both.
        let n = nearest(10_000 + 24_000, LAG, &rows, window).unwrap();
        assert_eq!((n.index, n.inside), (0, false));
        // The window's edge.
        assert!(nearest(10_000 + LAG + window, LAG, &rows, window).unwrap().inside);
        assert!(!nearest(10_000 + LAG + window + 1, LAG, &rows, window).unwrap().inside);
        assert!(nearest(5, LAG, &[], window).is_none());
    }

    #[test]
    fn the_log_takes_long_callbacks_and_entries_two_periods_late() {
        let log = SlowLog::new();
        let ms = Duration::from_millis(1);
        // 48 frames at 48 kHz: a period of exactly 1 ms.
        log.record(0, 48, 3 * ms, 500, 48_000);
        assert!(log.rows().is_empty() && log.seen.load(Relaxed) == 0, "off: nothing");
        log.on.store(true, Relaxed);
        // The whole period is long; a nanosecond under it is not.
        log.record(48, 48, ms - Duration::from_nanos(1), 0, 48_000);
        log.record(96, 48, ms, 0, 48_000);
        // Late from two periods behind; a frame under is not.
        log.record(144, 48, ms / 2, 95, 48_000);
        log.record(192, 48, ms / 2, 96, 48_000);
        log.record(240, 48, 2 * ms, 300, 48_000);
        log.record(288, 0, 2 * ms, 300, 48_000);
        let rows: Vec<(Frame, u32, u32, u64, bool)> = log.rows().iter().map(|s| (s.frame, s.frames, s.late, s.nanos, s.long)).collect();
        assert_eq!(rows, [(96, 48, 0, 1_000_000, true), (192, 48, 96, 500_000, false), (240, 48, 300, 2_000_000, true)]);
        assert_eq!((log.seen.load(Relaxed), log.long.load(Relaxed), log.late.load(Relaxed)), (5, 2, 2));
        // Late entries stop short of the table's end, long callbacks fill it, and both say what they lost.
        for k in 0..SLOW_CAP as Frame {
            log.record(1000 + k, 48, ms / 2, 200, 48_000);
        }
        assert_eq!((log.rows().len(), log.late_over.load(Relaxed), log.long_over.load(Relaxed)), (SLOW_CAP - LONG_RESERVE, 3 + LONG_RESERVE as u64, 0));
        for k in 0..LONG_RESERVE as Frame + 5 {
            log.record(90_000 + k, 48, ms, 0, 48_000);
        }
        assert_eq!((log.rows().len(), log.long_over.load(Relaxed)), (SLOW_CAP, 5));
        assert!(log.rows()[SLOW_CAP - 1].long && log.rows()[SLOW_CAP - 1].frame == 90_000 + LONG_RESERVE as Frame - 1);
        assert_eq!((log.long.load(Relaxed), log.late.load(Relaxed)), (2 + LONG_RESERVE as u64 + 5, 2 + SLOW_CAP as u64));
        log.on.store(false, Relaxed);
        log.record(0, 48, 3 * ms, 500, 48_000);
        assert_eq!(log.seen.load(Relaxed), 5 + SLOW_CAP as u64 + LONG_RESERVE as u64 + 5);
    }

    #[test]
    fn the_watched_span_ends_where_the_last_whole_window_began() {
        let base = Loop::watching(48_000, dbfs(-80.0));
        let stop = base.next + 9600;
        let mut clean = base.clone();
        clean.run(stop);
        clean.det.finish();
        let (from, to) = (clean.det.watch_from, clean.det.watch_to);
        // The last whole window and the frames of the one in progress are outside the span.
        assert!((to - from) % WINDOW as Frame == 0 && (WINDOW as Frame..2 * WINDOW as Frame).contains(&(stop - to)), "{from}..{to} of {stop}");
        assert_eq!((clean.det.observed as Frame, clean.det.pushed as Frame, clean.det.events().len()), (to - from, to - from, 0));
        // A frame lost at any phase, the residual's weakest among them: inside the span it is an event,
        // and where it may go unseen the span does not reach.
        for at in to - 2 * WINDOW as Frame..stop {
            let mut l = base.clone();
            l.defects.push((at, Defect::Delete(1)));
            l.run(stop);
            l.det.finish();
            assert_eq!((l.det.watch_to, l.det.observed as Frame), (to, to - from), "a frame lost at {at}");
            let events = l.det.events().len();
            assert!(events <= 1 && (events == 1 || at >= to), "a frame lost at {at}, inside the span to {to}: {events} events");
        }
    }

    #[test]
    fn a_snippet_holds_no_samples_from_across_a_frame_jump() {
        let mut l = Loop::watching(48_000, 0.0);
        let first = l.next + 640;
        l.defects.push((first, Defect::Zero(8)));
        l.run(first + 128);
        // A callback no hook ran for, while the first event's snippet still fills.
        let jump = l.next;
        l.next += BLOCK as Frame;
        l.run(l.next + 9600);
        // Another, and an event before the ring has filled again.
        let again = l.next + BLOCK as Frame;
        l.next = again;
        let second = again + 100;
        l.defects.push((second, Defect::Zero(8)));
        l.run(second + 1000);
        assert_eq!((l.det.events().len(), l.det.n_snippets), (2, 2), "{:?}", l.det.events());
        let snippets = l.det.snippets[..2].to_vec();
        assert!(snippets[0].first + snippets[0].len as Frame == jump && snippets[0].len > SNIP_HALF, "the first ends at the jump");
        assert!(snippets[1].first == again && snippets[1].len > SNIP_HALF, "the second starts after the jump before it");
        // Every sample under its own frame.
        for s in &snippets {
            for (i, &x) in s.data[..s.len].iter().enumerate() {
                assert_eq!(x, l.at(s.first + i as Frame), "frame {}", s.first + i as Frame);
            }
        }
    }

    /// The rig on a synthetic device: callbacks of `BLOCK` frames, its output side cabled back `LAG`
    /// frames later. `spoil` may change what the cable returns at a device frame.
    struct Device {
        shared: Arc<Shared>,
        rig: ToneRig,
        played: Vec<f32>,
        frame: Frame,
        /// The output callback logs itself after its block, as `Render::render` does.
        logged: bool,
    }

    impl Device {
        fn new() -> Device {
            let shared = Shared::new();
            Device { rig: ToneRig::new(shared.clone(), true), shared, played: Vec::new(), frame: T0, logged: true }
        }

        fn callback(&mut self, spoil: &impl Fn(Frame, f32) -> f32) {
            let at = self.played.len();
            let input: [f32; BLOCK] = std::array::from_fn(|k| {
                let x = (at + k).checked_sub(LAG as usize).map_or(0.0, |i| CABLE as f32 * self.played[i]);
                spoil(self.frame + k as Frame, x)
            });
            let (mut left, mut right) = ([0.0; BLOCK], [0.0; BLOCK]);
            self.rig.block(self.frame, &input, true, &mut left, &mut right);
            assert!(left.iter().all(|&x| x == 0.0), "the tone leaves on one side");
            self.played.extend(right);
            if self.logged {
                self.shared.slow.record(self.frame, BLOCK, Duration::from_micros(100), 0, 48_000);
            }
            self.frame += BLOCK as Frame;
        }

        /// The probe's order of work: start, wait for the first control, soak `seconds`, stop, fade out.
        fn run(&mut self, seconds: f64, spoil: impl Fn(Frame, f32) -> f32) -> Report {
            let rate = 48_000;
            for _ in 0..100 {
                self.callback(&spoil);
            }
            assert!(self.played.iter().all(|&x| x == 0.0), "silent until the probe starts it");
            self.shared.begin(rate);
            let mut budget = 4 * rate as usize / BLOCK;
            while self.shared.counts().0 == 0 && self.shared.cal() != Cal::NoTone && budget > 0 {
                self.callback(&spoil);
                budget -= 1;
            }
            let soak_from = self.frame;
            for _ in 0..(seconds * rate as f64) as usize / BLOCK {
                self.callback(&spoil);
            }
            self.shared.end();
            for _ in 0..rate as usize / BLOCK {
                self.callback(&spoil);
            }
            assert!(self.shared.done());
            let steps = self.played.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
            assert!(*self.played.last().unwrap() == 0.0, "faded out");
            // No step but the controls': a full-level tone moves 0.035 a frame, a hole's edge up to 0.25.
            let holes = self.rig.holes.iter().flatten().count();
            let edges = self.played.windows(2).filter(|w| (w[1] - w[0]).abs() > 0.04).count();
            assert!(steps <= LEVEL && edges <= 2 * holes, "{edges} steps for {holes} controls");
            self.rig.report(Some(facts(LAG, soak_from)))
        }
    }

    #[test]
    fn a_clean_run_finds_both_controls_a_round_trip_after_they_left() {
        let mut device = Device::new();
        let report = device.run(2.0, |_, x| x);
        assert!(report.pass, "{}\n{}", report.verdict, report.lines.join("\n"));
        assert!(report.verdict.starts_with("tone: clean: both controls found, full coverage, 0 loopback discontinuities over 0 long callbacks and 0 late entries"), "{}", report.verdict);
        let (rig, events) = (&device.rig, device.rig.det.events());
        assert_eq!(events.len(), 2, "{events:?}");
        for (hole, e) in rig.holes.iter().zip(events) {
            let offset = e.onset - hole.unwrap();
            assert!((LAG - 1..=LAG + 2).contains(&offset), "the control came back {offset} frames after it left");
            assert_eq!(e.phase_step, 0.0);
        }
        // The second control left when the probe stopped the soak, well after the first.
        assert!(rig.holes[1].unwrap() - rig.holes[0].unwrap() > 2 * 48_000);
        let text = report.lines.join("\n");
        assert!(text.contains("tone: 48000 Hz, 1078.7 Hz") && text.contains(&format!("offset {LAG} frames")) && text.contains("tone snippet, event 1"), "{text}");
        assert!(text.matches("(control ").count() == 2 && !text.contains("tone gap"), "{text}");
    }

    #[test]
    fn a_control_whose_hole_came_back_longer_is_a_discontinuity() {
        let mut plain = Device::new();
        assert!(plain.run(2.0, |_, x| x).pass);
        // A dropout of 96 frames right behind the first control's hole: one event, no phase step, short.
        let back = plain.rig.holes[0].unwrap() + LAG + HOLE;
        let mut device = Device::new();
        let report = device.run(2.0, move |f, x| if (back..back + 96).contains(&f) { 0.0 } else { x });
        let text = report.lines.join("\n");
        assert_eq!(device.rig.det.events().len(), 2, "{text}");
        let e = device.rig.det.events()[0];
        assert!(e.phase_step == 0.0 && e.end - e.onset <= HOLE + 3 * WINDOW as Frame, "{e:?}");
        assert!(!report.pass && report.verdict.starts_with("tone: discontinuities: 1 loopback discontinuities, 0 within") && report.verdict.contains("1 merged into a control"), "{}\n{text}", report.verdict);
        assert!(text.contains("its residual cues span 161 frames") && text.contains("and its event holds more than the hole"), "{text}");
        // The plain controls' spans are the hole's.
        let spans: Vec<Frame> = plain.rig.det.events().iter().map(|e| e.residual_to - e.residual_from).collect();
        assert!(spans.iter().all(|s| (HOLE - 1..=HOLE + 1).contains(s)), "{spans:?}");
    }

    #[test]
    fn a_broken_tone_is_counted_beside_the_controls_and_tied_to_its_callback() {
        // 32 frames of nothing on the cable twice in the soak; a long callback a round trip before the first.
        let mut device = Device::new();
        let (first, second) = (T0 + 150_000, T0 + 200_000);
        device.shared.slow.on.store(true, Relaxed);
        device.shared.slow.record(first - LAG, BLOCK, Duration::from_millis(2), 0, 48_000);
        device.shared.slow.on.store(false, Relaxed);
        let hole = |f: Frame, x: f32| if (first..first + 32).contains(&f) || (second..second + 32).contains(&f) { 0.0 } else { x };
        let report = device.run(2.0, hole);
        let text = report.lines.join("\n");
        assert!(!report.pass, "{text}");
        assert!(
            report.verdict.starts_with("tone: discontinuities: 2 loopback discontinuities, 1 within ±7.19 ms of a long or late callback, 1 not; the run had 1 long callbacks and 0 late entries"),
            "{}\n{text}",
            report.verdict
        );
        assert_eq!(device.rig.det.events().len(), 4, "{text}");
        assert!(text.contains("+0.00 ms after it on the output side") && text.contains("inside the window") && text.contains("outside the window"), "{text}");
        assert!(text.contains("tone callback: long at frame") && text.contains(": event 2"), "{text}");
        assert!(!text.contains("audible") && !text.contains("output glitch"), "{text}");
    }

    #[test]
    fn a_run_the_instrument_did_not_measure_is_neither_clean_nor_broken() {
        // No cable.
        let report = Device::new().run(1.0, |_, _| 0.0);
        assert!(!report.pass && report.verdict.starts_with("tone: not measured: no tone on the input"), "{}", report.verdict);
        // A cable that goes dead in the soak: one event without end, and the second control is not found.
        let report = Device::new().run(2.0, |f, x| if f > T0 + 150_000 { 0.0 } else { x });
        assert!(!report.pass && report.verdict.starts_with("tone: not measured:") && report.verdict.contains("control 2 was not found"), "{}", report.verdict);
        // A round trip the driver did not report: the controls come back off the alignment.
        let mut device = Device::new();
        device.run(1.0, |_, x| x);
        let report = device.rig.report(Some(facts(LAG + 400, T0)));
        // A device that was interrupted under the tone, whatever the detector saw.
        let report = (report, device.rig.report(Some(Facts { interrupted: Some("Lost".to_string()), ..facts(LAG, T0) })));
        assert!(!report.1.pass && report.1.verdict.starts_with("tone: not measured: the device was interrupted while the tone played (Lost)"), "{}", report.1.verdict);
        assert!(device.rig.report(Some(facts(LAG, T0))).pass, "the same run, uninterrupted");
        let report = report.0;
        // A callback log that never recorded: no word on the long and late callbacks.
        let mut unlogged = Device::new();
        unlogged.logged = false;
        let quiet = unlogged.run(1.0, |_, x| x);
        assert!(!quiet.pass && quiet.verdict.starts_with("tone: not measured: the callback log saw 0 callbacks"), "{}", quiet.verdict);
        // One that recorded a small part of the span.
        unlogged.shared.slow.seen.store(300, Relaxed);
        let part = unlogged.rig.report(Some(facts(LAG, T0)));
        assert!(!part.pass && part.verdict.contains("the callback log saw 300 callbacks, under"), "{}", part.verdict);
        assert!(!report.pass && report.verdict.contains("control 1 came back off the alignment"), "{}", report.verdict);
        // A callback that never reached the hook: a coverage gap.
        let mut device = Device::new();
        device.shared.begin(48_000);
        let clean = |_: Frame, x: f32| x;
        for k in 0..3 * 48_000 / BLOCK {
            if k == 1800 {
                device.played.extend([0.0; BLOCK]);
                device.frame += BLOCK as Frame;
            }
            device.callback(&clean);
        }
        device.shared.end();
        for _ in 0..48_000 / BLOCK {
            device.callback(&clean);
        }
        let report = device.rig.report(Some(facts(LAG, T0)));
        assert!(!report.pass && report.verdict.starts_with("tone: not measured: 1 coverage gaps"), "{}", report.verdict);
        assert!(report.lines.iter().any(|l| l.starts_with("tone gap: 64 frames") && l.contains("the frame counter jumped")), "{}", report.lines.join("\n"));
        // The tone that never started.
        let report = Device::new().rig.report(None);
        assert!(!report.pass && report.verdict == "tone: not measured: the tone never started");
    }
}
