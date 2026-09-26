//! OWNS: the input sends (tester report F15): ECHO and REVERB on the wet signal (the live slot's
//! output: an amp-sim, or the dry input through an empty live slot), before the record tap. The engine
//! adds their mono sum to the record tap and to the monitor on the frame it renders them, so the player
//! hears what is recorded, and the dry play path gains no latency.
//!
//! Wet only: the dry signal never passes through a send. A send reads the wet signal and renders only
//! its own output (no dry/wet CrossFade, which would change the dry bits and leak its wet path at
//! −56 dB: `dsp::crossfade`), so with the sends on or off the dry part of the record tap and of the
//! monitor is the same bits, and a take's alignment has no FX term (`tests/input_fx.rs`).
//!
//! - ECHO: a feedback delay line (Blink's DelayNode kernel, [`DelayNode`]) whose time is a division of
//!   the clock's beat (the lane delay's divisions, [`DIVISIONS`]), fed `gate · x + feedback · y` and
//!   heard as `level · √(1 − feedback²) · y` (the levels below). The feedback reaches the line on the
//!   frame it is read ([`DelayNode::read_frame`]), so the repeats land on exact multiples of the delay
//!   time (a lane's `DelayFx`, a port of Tone on Blink, feeds it back a quantum late).
//! - REVERB: its own convolver ([`Convolver`]) over the lanes' bus IR (`effects::reverb_ir`), fed
//!   `gate · x` and heard as `level · (L + R) / 2`: the stereo tail summed to mono as the instruments'
//!   record path sums a stereo synth.
//!
//! The levels are linear gains, 0..1 like the lanes' reverb send, and the monitor has no limiter, so
//! at 1 neither send should clip a normal guitar signal. The echo line's gain grows with its feedback:
//! `1 / (1 − fb²)` in energy for a broadband input (+10 dB at 0.95), so the echo's level is scaled by
//! `√(1 − fb²)`: at level 1 the whole train of repeats carries the input's energy at any feedback (the
//! first echo at 0.92 of the input at the default 0.4, 0.31 at 0.95). A sustained pitch whose period
//! divides the delay time still adds up in phase, `√((1 + fb) / (1 − fb))` (×6.2 at 0.95): a corner a
//! played note, which decays and wavers, rarely holds. The reverb's normalized IR (−58 dB calibration,
//! `dsp::convolver`) keeps its tail well under the input.
//!
//! # Control
//!
//! As the lane FX (`effects`): every value is a [`ToneParam`] and every change a 20 ms ramp ([`RAMP`])
//! from the frame it lands on, computed per 128-frame quantum on the DSP clock, so a change sounds from
//! the next quantum boundary and any block split renders the same bits. The echo's time follows the
//! clock's tempo at once from the next quantum boundary (a lane delay's `set_timing`); a division change
//! ramps.
//!
//! # Off, and idle
//!
//! On and off move the send's input gate between 0 and 1 over [`RAMP`]: switching off fades what goes
//! in, not what comes out, so the tail rings out at its level with no click. A send that is off and
//! has decayed costs one gate check per quantum and outputs exact zeros:
//!
//! - the echo writes a sample under `FLUSH` (−120 dBFS) into its line as 0 and counts the zeros it
//!   wrote in a row; once they span the line's buffer with the gate shut, everything in reach is 0, and
//!   it renders nothing until the gate opens;
//! - the reverb hands its convolver no input while the gate is shut: the convolver goes silent once
//!   the IR's length has passed without input (Blink's tail rule) and skips its work. Before the gate
//!   first opens it is never called.
//!
//! While both are silent [`InputFx::render`] says so and the engine adds nothing: the record tap and
//! the monitor are then bit-identical to never having had a send on. Everything is allocated in
//! [`InputFx::new`]; rendering and the controls never allocate.

use crate::api::{InputSend, InputSendParam};
use crate::dsp::convolver::Convolver;
use crate::dsp::delay::DelayNode;
use crate::dsp::fx::{clamp_index, division_beats, Ctl, DIVISIONS, MAX_FEEDBACK, RAMP};
use crate::dsp::param::{AudioParam, Rate, ToneParam, Units, QUANTUM};
use crate::grid::{frames_per_bar, Frame};

/// The echo line's longest delay: a quarter note at 40 BPM, the slowest tempo, is 1.5 s.
const MAX_DELAY: f64 = 2.0;
/// The echo writes a sample below this (−120 dBFS) into its line as 0 (see the module doc).
const FLUSH: f32 = 1e-6;
/// The tempo the echo's time starts from, until the clock hands it one.
const START_BEAT_PERIOD: f64 = 0.5;

/// A gain param starting at `value`.
fn gain(sample_rate: f32, units: Units, value: f64, frame: u64) -> ToneParam {
    let native = AudioParam::new(sample_rate as f64, 1.0, f32::MIN, f32::MAX, Rate::A);
    ToneParam::new(native, units, Some(value), frame)
}

/// `param`'s values for the quantum at `q`: sample-accurate while it is automated, else its one value.
/// False when every value is 0.
fn fill(param: &mut ToneParam, q: u64, values: &mut [f32; QUANTUM]) -> bool {
    let native = &mut param.native;
    if native.has_sample_accurate_values(q) {
        native.calculate_sample_accurate_values(q, values, None);
        values.iter().any(|&v| v != 0.0)
    } else {
        let v = native.value(q);
        values.fill(v);
        v != 0.0
    }
}

struct Echo {
    on: bool,
    /// Index into [`DIVISIONS`].
    time: usize,
    feedback: f64,
    level: f64,
    beat_period: f64,
    sample_rate: f32,
    gate: ToneParam,
    feedback_gain: ToneParam,
    level_gain: ToneParam,
    delay: DelayNode,
    gates: [f32; QUANTUM],
    feedbacks: [f32; QUANTUM],
    /// The heard level, compensated for the feedback (module doc).
    levels: [f32; QUANTUM],
    /// Zeros written into the line in a row, up to `span`, the line's buffer length.
    quiet: usize,
    span: usize,
    /// The quantum being rendered renders nothing.
    idle: bool,
}

impl Echo {
    fn new(sample_rate: f32, ctl: Ctl) -> Self {
        let time = InputSendParam::EchoTime.range().2 as usize;
        let feedback = InputSendParam::EchoFeedback.range().2;
        let level = InputSendParam::EchoLevel.range().2;
        let delay = DelayNode::new(sample_rate, START_BEAT_PERIOD * division_beats(time), MAX_DELAY, ctl.frame);
        let span = delay.delay.buffer_frames();
        Echo {
            on: false,
            time,
            feedback,
            level,
            beat_period: START_BEAT_PERIOD,
            sample_rate,
            gate: gain(sample_rate, Units::Gain, 0.0, ctl.frame),
            feedback_gain: gain(sample_rate, Units::NormalRange, feedback, ctl.frame),
            level_gain: gain(sample_rate, Units::Gain, level, ctl.frame),
            delay,
            gates: [0.0; QUANTUM],
            feedbacks: [0.0; QUANTUM],
            levels: [0.0; QUANTUM],
            quiet: span,
            span,
            idle: true,
        }
    }

    fn set_on(&mut self, on: bool, ctl: Ctl) {
        self.on = on;
        self.gate.ramp_to(if on { 1.0 } else { 0.0 }, RAMP, ctl.now, ctl.frame);
    }

    fn delay_seconds(&self) -> f64 {
        self.beat_period * division_beats(self.time)
    }

    fn set_param(&mut self, param: InputSendParam, value: f64, ctl: Ctl) {
        match param {
            InputSendParam::EchoTime => {
                self.time = clamp_index(value, DIVISIONS.len());
                self.delay.delay_time.ramp_to(self.delay_seconds(), RAMP, ctl.now, ctl.frame);
            }
            InputSendParam::EchoFeedback => {
                self.feedback = value.clamp(0.0, MAX_FEEDBACK);
                self.feedback_gain.ramp_to(self.feedback, RAMP, ctl.now, ctl.frame);
            }
            _ => {
                self.level = value.clamp(0.0, 1.0);
                self.level_gain.ramp_to(self.level, RAMP, ctl.now, ctl.frame);
            }
        }
    }

    /// A new beat replaces the delay time at once, from the context time (`DelayFx::set_timing`).
    fn set_beat_period(&mut self, beat_period: f64, ctl: Ctl) {
        self.beat_period = beat_period;
        let now = ctl.context_time(self.sample_rate);
        self.delay.delay_time.cancel_scheduled_values(now, ctl.frame);
        self.delay.delay_time.set_value_at_time(self.delay_seconds(), now, ctl.frame);
    }

    fn begin_quantum(&mut self, q: u64) {
        let open = fill(&mut self.gate, q, &mut self.gates);
        self.idle = !open && self.quiet >= self.span;
        if !self.idle {
            fill(&mut self.feedback_gain, q, &mut self.feedbacks);
            fill(&mut self.level_gain, q, &mut self.levels);
            // The feedback's energy compensation (module doc), per frame so a feedback ramp stays smooth.
            for (level, &fb) in self.levels.iter_mut().zip(&self.feedbacks) {
                *level *= (1.0 - fb * fb).sqrt();
            }
            self.delay.begin_quantum(q, None);
        }
    }

    /// Frames `at..at + input.len()` of the quantum begun into `out`; false (and zeros) while idle.
    fn process(&mut self, at: usize, input: &[f32], out: &mut [f32]) -> bool {
        if self.idle {
            out.fill(0.0);
            return false;
        }
        for (i, (&x, o)) in input.iter().zip(out.iter_mut()).enumerate() {
            let k = at + i;
            let y = self.delay.read_frame(k);
            let mut send = x * self.gates[k] + y * self.feedbacks[k];
            if send.abs() < FLUSH {
                send = 0.0;
                self.quiet = (self.quiet + 1).min(self.span);
            } else {
                self.quiet = 0;
            }
            self.delay.write_frame(k, send);
            *o = y * self.levels[k];
        }
        true
    }
}

struct Reverb {
    on: bool,
    level: f64,
    gate: ToneParam,
    level_gain: ToneParam,
    convolver: Convolver,
    gates: [f32; QUANTUM],
    levels: [f32; QUANTUM],
    gated: [f32; QUANTUM],
    wet: [[f32; QUANTUM]; 2],
    /// The gate is shut for the whole quantum being rendered: the convolver gets no input.
    shut: bool,
    /// The gate has opened once; before, the convolver is never called.
    used: bool,
    /// The last frames rendered were silent.
    silent: bool,
}

impl Reverb {
    fn new(sample_rate: f32, ir: [&[f32]; 2], ctl: Ctl) -> Self {
        let level = InputSendParam::ReverbLevel.range().2;
        Reverb {
            on: false,
            level,
            gate: gain(sample_rate, Units::Gain, 0.0, ctl.frame),
            level_gain: gain(sample_rate, Units::Gain, level, ctl.frame),
            convolver: Convolver::new(ir, sample_rate, true),
            gates: [0.0; QUANTUM],
            levels: [0.0; QUANTUM],
            gated: [0.0; QUANTUM],
            wet: [[0.0; QUANTUM]; 2],
            shut: true,
            used: false,
            silent: true,
        }
    }

    fn set_on(&mut self, on: bool, ctl: Ctl) {
        self.on = on;
        self.gate.ramp_to(if on { 1.0 } else { 0.0 }, RAMP, ctl.now, ctl.frame);
    }

    fn set_level(&mut self, value: f64, ctl: Ctl) {
        self.level = value.clamp(0.0, 1.0);
        self.level_gain.ramp_to(self.level, RAMP, ctl.now, ctl.frame);
    }

    fn begin_quantum(&mut self, q: u64) {
        self.shut = !fill(&mut self.gate, q, &mut self.gates);
        self.used |= !self.shut;
        if self.used {
            fill(&mut self.level_gain, q, &mut self.levels);
        }
    }

    /// Frames `frame..frame + input.len()` (DSP clock; `at` into the quantum begun) into `out`; false
    /// (and zeros) while silent.
    fn process(&mut self, frame: u64, at: usize, input: &[f32], out: &mut [f32]) -> bool {
        let n = input.len();
        self.silent = !self.used;
        if self.used {
            let source = if self.shut {
                None
            } else {
                for ((g, &x), &k) in self.gated.iter_mut().zip(input).zip(&self.gates[at..at + n]) {
                    *g = x * k;
                }
                Some(&self.gated[..n])
            };
            let [l, r] = &mut self.wet;
            self.silent = self.convolver.process(frame, source, &mut l[..n], &mut r[..n]);
        }
        if self.silent {
            out.fill(0.0);
            return false;
        }
        let [l, r] = &self.wet;
        for (((o, &a), &b), &g) in out.iter_mut().zip(&l[..n]).zip(&r[..n]).zip(&self.levels[at..at + n]) {
            *o = 0.5 * (a + b) * g;
        }
        true
    }
}

/// The two input sends and the tempo their echo follows.
pub struct InputFx {
    sample_rate: u32,
    /// Device frames the DSP clock is behind (`effects`).
    offset: Frame,
    /// The tempo the echo holds; 0 before the clock hands it one.
    bpm: u32,
    echo: Echo,
    reverb: Reverb,
    /// The quantum whose control values are computed.
    prepared: Option<u64>,
    mono: [f32; QUANTUM],
}

impl InputFx {
    /// Allocates (the echo's line, the reverb's convolver over `ir`, `effects::reverb_ir`): build it off
    /// the audio thread. Both sends start off.
    pub fn new(sample_rate: u32, ir: [&[f32]; 2]) -> Self {
        let sr = sample_rate as f32;
        let start = Ctl::at(0, sr);
        InputFx {
            sample_rate,
            offset: 0,
            bpm: 0,
            echo: Echo::new(sr, start),
            reverb: Reverb::new(sr, ir, start),
            prepared: None,
            mono: [0.0; QUANTUM],
        }
    }

    pub fn set_offset(&mut self, offset: Frame) {
        self.offset = offset;
    }

    fn dsp(&self, frame: Frame) -> u64 {
        (frame - self.offset) as u64
    }

    fn ctl(&self, frame: Frame) -> Ctl {
        Ctl::at(self.dsp(frame), self.sample_rate as f32)
    }

    pub fn is_on(&self, send: InputSend) -> bool {
        match send {
            InputSend::Echo => self.echo.on,
            InputSend::Reverb => self.reverb.on,
        }
    }

    pub fn param(&self, param: InputSendParam) -> f64 {
        match param {
            InputSendParam::EchoTime => self.echo.time as f64,
            InputSendParam::EchoFeedback => self.echo.feedback,
            InputSendParam::EchoLevel => self.echo.level,
            InputSendParam::ReverbLevel => self.reverb.level,
        }
    }

    /// Neither send rendered anything in the last frames rendered (both off and decayed, or never on).
    pub fn idle(&self) -> bool {
        self.echo.idle && self.reverb.silent
    }

    pub fn set_on(&mut self, send: InputSend, on: bool, frame: Frame) {
        let ctl = self.ctl(frame);
        match send {
            InputSend::Echo => self.echo.set_on(on, ctl),
            InputSend::Reverb => self.reverb.set_on(on, ctl),
        }
    }

    /// Clamped to the param's range; a value that is not finite is ignored.
    pub fn set_param(&mut self, param: InputSendParam, value: f64, frame: Frame) {
        if !value.is_finite() {
            return;
        }
        let ctl = self.ctl(frame);
        match param.send() {
            InputSend::Echo => self.echo.set_param(param, value, ctl),
            InputSend::Reverb => self.reverb.set_level(value, ctl),
        }
    }

    /// The clock's tempo at `frame`: a new one re-times the echo from the next quantum boundary. The
    /// beat is a quarter of the grid's bar, as the lane delays take it (`LaneFx::follow_grid`).
    pub fn follow_tempo(&mut self, bpm: u32, frame: Frame) {
        if bpm == self.bpm || bpm == 0 {
            return;
        }
        self.bpm = bpm;
        let sr = self.sample_rate as f64;
        let beat_period = frames_per_bar(bpm as f64, self.sample_rate) as f64 / sr / 4.0;
        let ctl = self.ctl(frame);
        self.echo.set_beat_period(beat_period, ctl);
    }

    /// Render frames `frame..frame + input.len()` of the wet signal `input` into `out`, the two sends'
    /// mono sum. Returns false when nothing sounded (then `out` is all zeros and the engine adds
    /// nothing). Blocks follow each other on the DSP clock.
    pub fn render(&mut self, frame: Frame, input: &[f32], out: &mut [f32]) -> bool {
        debug_assert_eq!(input.len(), out.len());
        let q = QUANTUM as u64;
        let start = self.dsp(frame);
        let mut sounded = false;
        let mut done = 0;
        while done < input.len() {
            let f = start + done as u64;
            let quantum_start = f - f % q;
            if self.prepared != Some(quantum_start) {
                self.echo.begin_quantum(quantum_start);
                self.reverb.begin_quantum(quantum_start);
                self.prepared = Some(quantum_start);
            }
            let at = (f - quantum_start) as usize;
            let n = (QUANTUM - at).min(input.len() - done);
            let span = done..done + n;
            let echo = self.echo.process(at, &input[span.clone()], &mut out[span.clone()]);
            let mono = &mut self.mono[..n];
            if self.reverb.process(f, at, &input[span.clone()], mono) {
                for (o, &r) in out[span].iter_mut().zip(mono.iter()) {
                    *o += r;
                }
                sounded = true;
            }
            sounded |= echo;
            done += n;
        }
        sounded
    }
}
