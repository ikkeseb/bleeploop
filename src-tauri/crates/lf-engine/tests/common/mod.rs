//! The engine's test rig: one `Engine` driven like the device drives it. Commands carry frames; the
//! input is a function of the absolute frame (a frame code names exactly which frame landed where);
//! blocks render at a chosen size; events and, on request, the output are kept for assertions.
//! The Rust twin of `verify/harness/rig.ts`.
#![allow(dead_code)]

pub mod refs;

use assert_no_alloc::assert_no_alloc;
use lf_engine::grid::{frames_per_bar, Frame};
use lf_engine::{Command, Engine, EngineConfig, EngineHandle, Event, Inserts, LaneInfo, LaneState, ProcessContext, TimedCommand};

// Every `process` call in every scenario runs under assert_no_alloc: the audio path never allocates.
// The check runs in debug builds; the crate's default `disable_release` makes it a no-op in release.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// Allocations inside `assert_no_alloc` so far (debug builds count them, the `warn_debug` feature;
/// release builds do not check, so 0).
pub fn violation_count() -> u32 {
    #[cfg(debug_assertions)]
    return assert_no_alloc::violation_count();
    #[cfg(not(debug_assertions))]
    0
}

/// A take's frame code: exact in f32 and unique over 2^22 frames.
pub fn code(frame: Frame) -> f32 {
    ((frame.rem_euclid(1 << 22)) + 1) as f32 / (1 << 23) as f32
}

/// The absolute frame nearest `near` whose code is `sample`.
pub fn frame_of(sample: f32, near: Frame) -> Frame {
    let f = (sample * (1 << 23) as f32).round() as Frame - 1;
    f + ((near - f) as f64 / (1 << 22) as f64).round() as Frame * (1 << 22)
}

/// A plugin stand-in: the wet signal is the input `latency` frames late.
pub struct Delay {
    pub latency: Frame,
    line: Vec<f32>,
}

impl Delay {
    pub fn new(latency: Frame) -> Self {
        Delay { latency, line: vec![0.0; latency as usize] }
    }
}

impl Inserts for Delay {
    fn process(&mut self, _frame: Frame, input: &[f32], wet: &mut [f32]) {
        for (w, &x) in wet.iter_mut().zip(input) {
            if self.line.is_empty() {
                *w = x;
            } else {
                *w = self.line.remove(0);
                self.line.push(x);
            }
        }
    }

    fn latency(&self) -> Frame {
        self.latency
    }
}

pub struct Rig {
    pub engine: Engine,
    handle: EngineHandle,
    pub sr: u32,
    /// The next frame to render.
    pub frame: Frame,
    pub block: usize,
    pub align: Frame,
    input: Box<dyn Fn(Frame) -> f32>,
    pub inserts: Box<dyn Inserts>,
    pub events: Vec<Event>,
    /// Rendered left channel from `keep_output`, and the frame it starts at.
    pub output: Option<(Frame, Vec<f32>)>,
    gap_next: bool,
    in_buf: Vec<f32>,
    left: Vec<f32>,
    right: Vec<f32>,
}

pub struct Opts {
    pub sr: u32,
    pub start: Frame,
    pub loop_seconds: f64,
    pub block: usize,
    pub align: Frame,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { sr: 48000, start: 48000, loop_seconds: 20.0, block: 128, align: 0 }
    }
}

impl Rig {
    pub fn new() -> Rig {
        Rig::with(Opts::default())
    }

    pub fn at(sr: u32) -> Rig {
        Rig::with(Opts { sr, start: sr as Frame, ..Opts::default() })
    }

    pub fn with(o: Opts) -> Rig {
        let config = EngineConfig { max_loop_seconds: o.loop_seconds, ..EngineConfig::new(o.sr) };
        let (engine, handle) = Engine::new(config);
        Rig {
            engine,
            handle,
            sr: o.sr,
            frame: o.start,
            block: o.block,
            align: o.align,
            input: Box::new(|_| 0.0),
            inserts: Box::new(lf_engine::Dry),
            events: Vec::new(),
            output: None,
            gap_next: false,
            in_buf: vec![0.0; 4096],
            left: vec![0.0; 4096],
            right: vec![0.0; 4096],
        }
    }

    pub fn set_input(&mut self, signal: impl Fn(Frame) -> f32 + 'static) {
        self.input = Box::new(signal);
    }

    pub fn set_level(&mut self, level: f32) {
        self.input = Box::new(move |_| level);
    }

    pub fn keep_output(&mut self) {
        self.output = Some((self.frame, Vec::new()));
    }

    /// The next rendered block starts after an input gap (an xrun).
    pub fn gap(&mut self) {
        self.gap_next = true;
    }

    /// The device never delivers the next `frames`: the frame counter jumps and the input has a gap.
    pub fn skip(&mut self, frames: Frame) {
        self.frame += frames;
        self.gap_next = true;
    }

    pub fn send_at(&mut self, frame: Frame, command: Command) {
        self.handle.commands.push(TimedCommand { frame: Some(frame), command }).expect("command ring full");
    }

    /// Apply `command` at the current frame, then render that one frame.
    pub fn press(&mut self, command: Command) {
        self.send_at(self.frame, command);
        self.advance(1);
    }

    /// Apply a setting at the current frame without rendering.
    pub fn set(&mut self, command: Command) {
        self.press(command);
    }

    pub fn advance(&mut self, frames: Frame) {
        let mut left = frames;
        while left > 0 {
            let n = (self.block as Frame).min(left) as usize;
            self.render(n);
            left -= n as Frame;
        }
    }

    /// Render until no block job runs and no held command waits: what a UI would see settle.
    pub fn idle(&mut self) {
        while self.engine.looper().busy() || self.engine.holding() {
            self.advance(1);
        }
    }

    pub fn advance_to(&mut self, frame: Frame) {
        if frame > self.frame {
            self.advance(frame - self.frame);
        }
    }

    pub fn seconds(&self, s: f64) -> Frame {
        (s * self.sr as f64).round() as Frame
    }

    fn render(&mut self, n: usize) {
        for k in 0..n {
            self.in_buf[k] = (self.input)(self.frame + k as Frame);
        }
        let ctx = ProcessContext { frame: self.frame, xrun: std::mem::take(&mut self.gap_next), align_frames: self.align };
        let violations = violation_count();
        let (engine, inserts) = (&mut self.engine, self.inserts.as_mut());
        let (input, left, right) = (&self.in_buf[..n], &mut self.left[..n], &mut self.right[..n]);
        assert_no_alloc(|| engine.process(&ctx, input, left, right, inserts));
        assert_eq!(violation_count(), violations, "process allocated at frame {}", self.frame);
        if let Some((_, out)) = self.output.as_mut() {
            out.extend_from_slice(&self.left[..n]);
        }
        self.frame += n as Frame;
        while let Ok(e) = self.handle.events.pop() {
            self.events.push(e);
        }
    }

    pub fn lane(&self, i: usize) -> LaneInfo {
        self.engine.looper().info(i)
    }

    pub fn state(&self, i: usize) -> LaneState {
        self.lane(i).state
    }

    pub fn master(&self) -> Frame {
        self.engine.looper().master()
    }

    pub fn anchor(&self) -> Frame {
        self.engine.looper().anchor()
    }

    pub fn bpm(&self) -> u32 {
        self.engine.clock().bpm()
    }

    pub fn locked(&self) -> bool {
        self.engine.clock().locked()
    }

    pub fn fpb(&self) -> Frame {
        frames_per_bar(self.bpm() as f64, self.sr)
    }

    /// The recorder's lane and window.
    pub fn window(&self) -> Option<(usize, Option<Frame>, Option<Frame>)> {
        self.engine.looper().recorder()
    }

    pub fn start_frame(&self) -> Frame {
        self.window().and_then(|w| w.1).expect("no armed window")
    }

    pub fn end_frame(&self) -> Frame {
        self.window().and_then(|w| w.2).expect("no window end")
    }

    pub fn pcm(&self, i: usize) -> Vec<f32> {
        self.engine.looper().loop_pcm(i)
    }

    /// Every beat fired since event index `mark`: (frame, beat in bar, count left, clicked).
    pub fn beats_since(&self, mark: usize) -> Vec<(Frame, u8, u8, bool)> {
        self.events[mark..]
            .iter()
            .filter_map(|e| match *e {
                Event::Beat { frame, beat_in_bar, count_left, clicked } => Some((frame, beat_in_bar, count_left, clicked)),
                _ => None,
            })
            .collect()
    }

    pub fn beats(&self) -> Vec<(Frame, u8, u8, bool)> {
        self.beats_since(0)
    }

    pub fn clicks_since(&self, mark: usize) -> Vec<(Frame, bool)> {
        self.beats_since(mark).into_iter().filter(|b| b.3).map(|b| (b.0, b.1 == 0)).collect()
    }

    /// The count-in "1": the first beat with four beats left since `mark`.
    pub fn count_one(&self, mark: usize) -> Frame {
        self.beats_since(mark).iter().find(|b| b.2 == 4).expect("no count-in started").0
    }

    /// REC on `lane`, the count-in, `bars` bars of `level`, REC `after` frames past the last bar
    /// line, then the tail. Returns the master length.
    pub fn record_first_take(&mut self, lane: u8, bars: Frame, after: Frame) -> Frame {
        let mark = self.events.len();
        self.press(Command::RecDub(lane));
        let one = self.count_one(mark);
        let beat = 60.0 / self.bpm() as f64 * self.sr as f64;
        let downbeat = one + (4.0 * beat).round() as Frame;
        self.advance_to(downbeat + self.align + bars * self.fpb() + after);
        self.press(Command::RecDub(lane));
        self.advance(self.seconds(0.25));
        self.master()
    }

    /// The next master boundary strictly after the current frame.
    pub fn next_boundary(&self) -> Frame {
        let (a, m) = (self.anchor(), self.master());
        a + ((self.frame - a).div_euclid(m) + 1) * m
    }

    pub fn rejected(&self) -> usize {
        self.events.iter().filter(|e| matches!(e, Event::TakeRejected { .. })).count()
    }
}
