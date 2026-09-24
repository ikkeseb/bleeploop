//! OWNS: the looper: the five lanes and their buffers, the single recorder (a take, a RETAKE roll or an
//! overdub), every EMPTY → RECORDING → PLAYING ⇄ OVERDUBBING (+ STOPPED) transition, the master loop
//! (its length and grid anchor), the action gates with their refusals, and the block jobs that move
//! loop-sized data. Ported from `src/audio/looper/{machine,state,capture,playback,mixer}.ts` and
//! `src/ui/looper/gates.ts`.
//!
//! One clock: input frame `x` is captured at device frame `x`, and a lane plays loop position
//! `(f - anchor) mod master` at device frame `f`. A take starts `align` frames after its downbeat (the
//! driver's input + output latency, plus the plugin's), so audio played to the click lands on the grid;
//! an overdub sums input frame `x` onto position `(x - align - anchor) mod master`, in place, which is
//! at or behind the read head, so a layer is heard from the next pass on.
//!
//! Buffers are a pool allocated once: each lane owns a live and a spare buffer, plus one free buffer the
//! recorder borrows (a RETAKE's kept pass, an overdub's previous undo target). Undo swaps a lane's live
//! and spare; a kept retake pass swaps in from the free buffer; reverse is an index-mapping flag. What
//! must be copied is a block job (see [`Job`]); nothing does loop-sized work in one callback.

use crate::api::{Action, Event, LaneInfo, LaneState, Refusal, TRACK_COUNT};
use crate::autorec::{self, Detector};
use crate::clock::Clock;
use crate::engine::Feed;
use crate::grid::{
    clamp_bars, commit_anchor, frames_per_bar, loop_pos, max_whole_bars, next_boundary, plan_commit, plan_free_stop,
    plan_later_stop, plan_retake_stop, Frame, Grid, RetakeStop, TakeFill, COUNT_IN_BEATS,
};

pub const MAX_FIXED_BARS: Frame = 32;
/// Buffer positions a block job moves per rendered frame. A job always starts where a read or write head
/// will next touch, so at this rate it stays ahead of both; 60 s at 48 kHz takes about 59 ms.
pub const JOB_RATE: Frame = 1024;
/// The hands-free CLEAR confirm window (`CONFIRM_WINDOW_MS`).
const CONFIRM_WINDOW_MS: Frame = 2500;
const MAX_JOBS: usize = 4;
/// Lane volume smoothing: the Web Audio setTargetAtTime time constant.
const GAIN_TAU_SECONDS: f64 = 0.01;

/// What the lane's playback reads: a buffer and its orientation. It follows the lane's logical
/// buffer at once while stopped, and at the next loop boundary while playing (an undo or reverse swaps
/// there, as the Web Audio source swap did).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Audible {
    buf: usize,
    reversed: bool,
}

#[derive(Clone, Copy, Debug)]
struct Lane {
    state: LaneState,
    live: usize,
    spare: usize,
    /// The committed loop length (= master), 0 before a commit.
    length: Frame,
    /// Frames the take in flight has captured.
    written: Frame,
    armed: bool,
    auto_armed: bool,
    /// Orientation of `live`.
    reversed: bool,
    /// The spare buffer holds the one-level undo target, in this orientation.
    undo_valid: bool,
    spare_reversed: bool,
    audible: Audible,
    switch_at: Option<Frame>,
    stop_at: Option<Frame>,
    volume: f32,
    muted: bool,
    gain: f64,
}

impl Lane {
    fn new(live: usize, spare: usize) -> Self {
        Lane {
            state: LaneState::Empty,
            live,
            spare,
            length: 0,
            written: 0,
            armed: false,
            auto_armed: false,
            reversed: false,
            undo_valid: false,
            spare_reversed: false,
            audible: Audible { buf: live, reversed: false },
            switch_at: None,
            stop_at: None,
            volume: 1.0,
            muted: false,
            gain: 1.0,
        }
    }

    fn logical(&self) -> Audible {
        Audible { buf: self.live, reversed: self.reversed }
    }

    fn committed(&self) -> bool {
        matches!(self.state, LaneState::Playing | LaneState::Stopped)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Take,
    Overdub,
}

/// A RETAKE roll: the take slides one pass per window end instead of committing.
#[derive(Clone, Copy, Debug)]
struct Roll {
    /// The 1-based pass in flight.
    pass: u32,
    /// The last complete clean pass sits in the free buffer.
    kept: bool,
}

/// The single recorder slot: the one take or overdub capturing now.
#[derive(Clone, Copy, Debug)]
struct Recorder {
    lane: usize,
    kind: Kind,
    /// The capture window, in input frames: `[start, end)`. `start` is `None` while AUTO listens, `end`
    /// while an overdub or AUTO runs open.
    start: Option<Frame>,
    end: Option<Frame>,
    /// Input frames a take starts after its downbeat (sampled at the arm).
    align: Frame,
    /// PLAY/STOP ended the capture: the lane lands STOPPED and is silent meanwhile.
    stop_playback: bool,
    /// A first take's counted downbeat: the grid the master is phase-locked to.
    downbeat: Option<Frame>,
    roll: Option<Roll>,
    /// An input gap fell inside the window (inside the pass in flight, for a roll).
    damaged: bool,
    /// RETAKE: the lane whose REC approved the roll records next, from its pass edge.
    handoff: Option<usize>,
    /// Overdub: the loop position of its first input frame, frames summed so far, and the undo state
    /// before it (restored if the layer is discarded).
    first_pos: Frame,
    summed: Frame,
    prev_undo_valid: bool,
    prev_spare_reversed: bool,
}

impl Recorder {
    fn new(lane: usize, kind: Kind, align: Frame) -> Self {
        Recorder {
            lane,
            kind,
            start: None,
            end: None,
            align,
            stop_playback: false,
            downbeat: None,
            roll: None,
            damaged: false,
            handoff: None,
            first_pos: 0,
            summed: 0,
            prev_undo_valid: false,
            prev_spare_reversed: false,
        }
    }
}

/// Buffer positions a job visits, in order: `(lo + (off + s) % span) % modulus` for `s` in `0..span`.
#[derive(Clone, Copy, Debug)]
struct Visit {
    lo: Frame,
    span: Frame,
    off: Frame,
    modulus: Frame,
}

impl Visit {
    fn at(&self, s: Frame) -> usize {
        ((self.lo + (self.off + s) % self.span) % self.modulus) as usize
    }
}

#[derive(Clone, Copy, Debug)]
enum JobKind {
    /// Rewrite a committed take's padding and tiling in place (grid `TakeFill`).
    Fill { buf: usize, fill: TakeFill },
    /// Copy `src` into `dst` position by position.
    Copy { src: usize, dst: usize },
    /// A discarded overdub layer: copy the pre-layer loop back, then return the previous undo target.
    Restore { src: usize, dst: usize, prev_undo_valid: bool, prev_spare_reversed: bool },
    /// COPY into lane `dst`: done, it becomes STOPPED, or PLAYING when its source played.
    LaneCopy { src: usize, dst: usize, from: usize, to: usize, resume: bool },
}

/// Loop-sized work spread over frames: `JOB_RATE` positions per rendered frame from `start`, so its
/// progress, and the frame it is done on, are the same whatever the block size.
#[derive(Clone, Copy, Debug)]
struct Job {
    kind: JobKind,
    lane: usize,
    visit: Visit,
    start: Frame,
    progress: Frame,
}

impl Job {
    fn done_frame(&self) -> Frame {
        self.start + job_frames(self.visit.span)
    }
}

/// Rendered frames a block job over `span` buffer positions takes: `JOB_RATE` positions a frame.
pub fn job_frames(span: Frame) -> Frame {
    (span + JOB_RATE - 1) / JOB_RATE
}

/// What a command answers: done, or it waits for the lane's block jobs until a frame (`Held`: as a
/// different command, bound to what it resolved when pressed).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Applied {
    Done,
    WaitUntil(Frame),
    Held(Frame, crate::api::Command),
}

/// The time and wiring a looper operation runs with.
pub struct Cx<'a> {
    pub now: Frame,
    /// Input frames a take arming now starts after its downbeat.
    pub align: Frame,
    pub clock: &'a mut Clock,
    pub feed: &'a mut Feed,
}

pub struct Looper {
    sample_rate: u32,
    capacity: Frame,
    bufs: Vec<Vec<f32>>,
    free: usize,
    lanes: [Lane; TRACK_COUNT],
    rec: Option<Recorder>,
    master: Frame,
    anchor: Frame,
    jobs: [Option<Job>; MAX_JOBS],
    /// The most buffer positions one job moved in one step: what "no loop-sized work in one callback"
    /// is checked against.
    job_step_max: Frame,
    detector: Detector,
    gain_coef: f64,
    loop_end_stop: bool,
    fixed_length: bool,
    fixed_bars: Frame,
    retake: bool,
    auto_record: bool,
    auto_sensitivity: f64,
    selected: usize,
    clear_armed: Option<(usize, Frame)>,
    published: [Option<LaneInfo>; TRACK_COUNT],
    published_transport: Option<(Frame, u32, bool)>,
}

impl Looper {
    pub fn new(sample_rate: u32, capacity: Frame) -> Self {
        // Written, not calloc'ed: a zeroed allocation maps its pages on first touch, which would then
        // fault inside the callback.
        let bufs = (0..2 * TRACK_COUNT + 1)
            .map(|_| {
                let mut buf = Vec::with_capacity(capacity as usize);
                buf.resize(capacity as usize, 0.0f32);
                buf
            })
            .collect();
        Looper {
            sample_rate,
            capacity,
            bufs,
            free: 2 * TRACK_COUNT,
            lanes: std::array::from_fn(|i| Lane::new(2 * i, 2 * i + 1)),
            rec: None,
            master: 0,
            anchor: 0,
            jobs: [None; MAX_JOBS],
            job_step_max: 0,
            detector: Detector::new(sample_rate),
            gain_coef: (-1.0 / (GAIN_TAU_SECONDS * sample_rate as f64)).exp(),
            loop_end_stop: false,
            fixed_length: false,
            fixed_bars: 4,
            retake: false,
            auto_record: false,
            auto_sensitivity: autorec::DEFAULT_SENSITIVITY,
            selected: 0,
            clear_armed: None,
            published: [None; TRACK_COUNT],
            published_transport: None,
        }
    }

    // ── Read-only views (tests, the Stage 5 feed) ─────────────────────────────────────────────────────

    pub fn capacity(&self) -> Frame {
        self.capacity
    }

    pub fn master(&self) -> Frame {
        self.master
    }

    /// The master grid's anchor: loop position 0 plays at `anchor + k * master`.
    pub fn anchor(&self) -> Frame {
        self.anchor
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn info(&self, i: usize) -> LaneInfo {
        let t = &self.lanes[i];
        let rolling = self.rec.filter(|r| r.lane == i && !t.armed).and_then(|r| r.roll);
        LaneInfo {
            state: t.state,
            length: t.length,
            armed: t.armed,
            auto_armed: t.auto_armed,
            can_undo: t.undo_valid && t.committed(),
            can_reverse: t.committed(),
            reversed: t.reversed,
            stop_at: t.stop_at,
            retake_pass: rolling.map_or(0, |r| r.pass),
        }
    }

    /// The recorder's lane and capture window, when one records.
    pub fn recorder(&self) -> Option<(usize, Option<Frame>, Option<Frame>)> {
        self.rec.map(|r| (r.lane, r.start, r.end))
    }

    /// Frames lane `i` has captured in its take in flight.
    pub fn written(&self, i: usize) -> Frame {
        self.lanes[i].written
    }

    pub fn volume(&self, i: usize) -> (f32, bool) {
        (self.lanes[i].volume, self.lanes[i].muted)
    }

    /// The committed loop of lane `i` as it plays forward: its logical buffer, read through its
    /// orientation (empty before a commit). Not for the audio thread: it allocates.
    pub fn loop_pcm(&self, i: usize) -> Vec<f32> {
        let t = &self.lanes[i];
        self.oriented(t.live, t.reversed, t.length)
    }

    /// The undo target of lane `i`, oriented, when it has one.
    pub fn undo_pcm(&self, i: usize) -> Option<Vec<f32>> {
        let t = &self.lanes[i];
        t.undo_valid.then(|| self.oriented(t.spare, t.spare_reversed, t.length))
    }

    /// Lane `i`'s whole live buffer, past its loop too.
    pub fn live_buffer(&self, i: usize) -> &[f32] {
        &self.bufs[self.lanes[i].live]
    }

    /// The take in flight on lane `i`: its captured frames, in capture order.
    pub fn take_pcm(&self, i: usize) -> Vec<f32> {
        let t = &self.lanes[i];
        self.bufs[t.live][..t.written.min(self.capacity) as usize].to_vec()
    }

    fn oriented(&self, buf: usize, reversed: bool, len: Frame) -> Vec<f32> {
        let data = &self.bufs[buf][..len as usize];
        if reversed { data.iter().rev().copied().collect() } else { data.to_vec() }
    }

    /// The most buffer positions one block job moved in a single step since the engine started.
    pub fn job_step_max(&self) -> Frame {
        self.job_step_max
    }

    /// True while any block job runs.
    pub fn busy(&self) -> bool {
        self.jobs.iter().any(Option::is_some)
    }

    // ── Settings ───────────────────────────────────────────────────────────────────────────────────

    pub fn set_loop_end_stop(&mut self, on: bool) {
        self.loop_end_stop = on;
    }

    pub fn set_fixed_length(&mut self, on: bool) {
        self.fixed_length = on;
    }

    pub fn set_fixed_bars(&mut self, bars: f64) {
        self.fixed_bars = if bars.is_finite() { (bars.round() as Frame).clamp(1, MAX_FIXED_BARS) } else { 1 };
    }

    pub fn fixed_bars(&self) -> Frame {
        self.fixed_bars
    }

    pub fn set_retake(&mut self, on: bool) {
        self.retake = on;
    }

    pub fn set_auto_record(&mut self, on: bool) {
        self.auto_record = on;
    }

    pub fn set_auto_sensitivity(&mut self, s: f64) {
        let s = if s.is_finite() { s } else { self.auto_sensitivity };
        self.auto_sensitivity = s.round().clamp(autorec::MIN_SENSITIVITY, autorec::MAX_SENSITIVITY);
    }

    /// The largest FIXED bar count the next take can use: the master's bars, else 32.
    pub fn next_take_max_bars(&self, bpm: u32) -> Frame {
        if self.master > 0 {
            MAX_FIXED_BARS.min(max_whole_bars(self.master, frames_per_bar(bpm as f64, self.sample_rate)))
        } else {
            MAX_FIXED_BARS
        }
    }

    pub fn set_volume(&mut self, i: usize, v: f32) {
        self.lanes[i].volume = if v.is_finite() { v.clamp(0.0, 1.5) } else { 0.0 };
    }

    pub fn set_mute(&mut self, i: usize, on: bool) {
        self.lanes[i].muted = on;
    }

    // ── Commands ───────────────────────────────────────────────────────────────────────────────────

    /// When a command on `lane` may run: it waits for the block jobs on that lane, and for a pending
    /// Restore when it may take the free buffer. Ending a capture or silencing a lane never waits, and a
    /// command is judged when it is pressed: one that would do nothing then is dropped rather than held.
    fn wait_for(&self, lane: Option<usize>, needs_free: bool) -> Applied {
        let until = self
            .jobs
            .iter()
            .flatten()
            .filter(|j| lane.is_none_or(|l| j.lane == l || matches!(j.kind, JobKind::LaneCopy { from, .. } if from == l)) || (needs_free && matches!(j.kind, JobKind::Restore { .. })))
            .map(Job::done_frame)
            .max();
        until.map_or(Applied::Done, Applied::WaitUntil)
    }

    fn capturing(&self, i: usize) -> bool {
        matches!(self.lanes[i].state, LaneState::Recording | LaneState::Overdubbing)
    }

    pub fn rec_dub(&mut self, cx: &mut Cx, i: usize) -> Applied {
        if self.lanes[i].stop_at.is_some() || self.lanes[i].state == LaneState::Stopped {
            return Applied::Done;
        }
        if !self.capturing(i) {
            let wait = self.wait_for(Some(i), true);
            if wait != Applied::Done {
                return wait;
            }
        }
        match self.lanes[i].state {
            LaneState::Empty => {
                // RETAKE: REC on another lane approves the rolling take; this lane records from its pass
                // end. With nothing kept yet the press is ignored, like any second recorder.
                if let Some(rec) = self.rec.filter(|r| r.roll.is_some() && !self.lanes[r.lane].armed) {
                    if self.retake_plan(cx, &rec) != RetakeStop::StopNow {
                        self.rec.as_mut().unwrap().handoff = Some(i);
                        self.stop_capture(cx, rec.lane);
                    }
                } else {
                    self.start_recording(cx, i, None);
                }
            }
            LaneState::Recording | LaneState::Overdubbing => self.stop_capture(cx, i),
            LaneState::Playing => self.start_overdub(cx, i),
            LaneState::Stopped => {}
        }
        Applied::Done
    }

    pub fn play_stop(&mut self, cx: &mut Cx, i: usize) -> Applied {
        if self.lanes[i].state == LaneState::Empty {
            return Applied::Done;
        }
        if self.lanes[i].state == LaneState::Stopped {
            // Only a resume waits: playback would start reading where a job has not been yet.
            let wait = self.wait_for(Some(i), false);
            if wait != Applied::Done {
                return wait;
            }
        }
        if self.lanes[i].stop_at.is_some() {
            // A second press silences now.
            self.stop(cx, i);
            return Applied::Done;
        }
        match self.lanes[i].state {
            LaneState::Empty => {}
            LaneState::Recording | LaneState::Overdubbing => {
                if let Some(rec) = self.rec.as_mut() {
                    rec.stop_playback = true;
                }
                self.stop_capture(cx, i);
            }
            LaneState::Playing => {
                if self.loop_end_stop {
                    let when = next_boundary(self.anchor, self.master, cx.now);
                    self.lanes[i].stop_at = Some(when);
                } else {
                    self.stop(cx, i);
                }
            }
            LaneState::Stopped => {
                self.resume(cx, i, None);
            }
        }
        Applied::Done
    }

    /// The Stop command: abort whatever the lane captures (nothing is kept but a committed loop) and
    /// silence it now. Silence never waits for a block job.
    pub fn stop_command(&mut self, cx: &mut Cx, i: usize) -> Applied {
        self.stop(cx, i);
        Applied::Done
    }

    pub fn undo(&mut self, cx: &mut Cx, i: usize) -> Applied {
        let t = &self.lanes[i];
        if !t.undo_valid || t.stop_at.is_some() || !t.committed() || self.master == 0 {
            return Applied::Done;
        }
        let wait = self.wait_for(Some(i), false);
        if wait != Applied::Done {
            return wait;
        }
        let t = &mut self.lanes[i];
        std::mem::swap(&mut t.live, &mut t.spare);
        std::mem::swap(&mut t.reversed, &mut t.spare_reversed);
        self.follow_logical(cx.now, i);
        Applied::Done
    }

    pub fn reverse(&mut self, cx: &mut Cx, i: usize) -> Applied {
        let t = &self.lanes[i];
        if t.stop_at.is_some() || !t.committed() || self.master == 0 {
            return Applied::Done;
        }
        let wait = self.wait_for(Some(i), false);
        if wait != Applied::Done {
            return wait;
        }
        let t = &mut self.lanes[i];
        t.reversed = !t.reversed;
        self.follow_logical(cx.now, i);
        Applied::Done
    }

    /// Playback follows the lane's logical buffer: now when stopped, on the next boundary when playing.
    fn follow_logical(&mut self, now: Frame, i: usize) {
        let t = &mut self.lanes[i];
        if t.state == LaneState::Playing {
            t.switch_at = Some(next_boundary(self.anchor, self.master, now));
        } else {
            t.audible = t.logical();
            t.switch_at = None;
        }
    }

    pub fn copy(&mut self, cx: &mut Cx, i: usize) -> Applied {
        let src = self.lanes[i];
        if !src.committed() {
            return Applied::Done;
        }
        let wait = self.wait_for(Some(i), false);
        if wait != Applied::Done {
            return wait;
        }
        let recorder = self.rec.map(|r| r.lane);
        let Some(j) = (0..TRACK_COUNT).find(|&k| self.lanes[k].state == LaneState::Empty && Some(k) != recorder) else {
            return Applied::Done;
        };
        let dst = &mut self.lanes[j];
        dst.length = self.master;
        dst.written = self.master;
        dst.reversed = src.reversed;
        dst.audible = dst.logical();
        dst.volume = src.volume;
        dst.muted = src.muted;
        dst.state = LaneState::Stopped;
        let resume = src.state == LaneState::Playing && src.stop_at.is_none();
        let kind = JobKind::LaneCopy { src: src.live, dst: dst.live, from: i, to: j, resume };
        self.push_job(cx.now, j, kind, Visit { lo: 0, span: self.master, off: 0, modulus: self.master });
        Applied::Done
    }

    pub fn clear(&mut self, cx: &mut Cx, i: usize) -> Applied {
        if self.lanes[i].state == LaneState::Empty {
            return Applied::Done;
        }
        let wait = self.wait_for(Some(i), false);
        if wait != Applied::Done {
            return wait;
        }
        self.clear_now(cx, i);
        Applied::Done
    }

    fn clear_now(&mut self, cx: &mut Cx, i: usize) {
        self.release_recorder(cx, i);
        let t = &mut self.lanes[i];
        *t = Lane { gain: t.gain, ..Lane::new(t.live, t.spare) };
        self.reset_master_if_blank(cx);
    }

    pub fn play_all(&mut self, cx: &mut Cx) -> Applied {
        let wait = self.wait_for(None, false);
        if wait != Applied::Done {
            return wait;
        }
        let stopped: [bool; TRACK_COUNT] = std::array::from_fn(|i| self.lanes[i].state == LaneState::Stopped);
        if stopped.iter().any(|&s| s) {
            let restart = self.restart_if_idle(cx);
            for i in (0..TRACK_COUNT).filter(|&i| stopped[i]) {
                self.resume(cx, i, Some(restart));
            }
        }
        Applied::Done
    }

    /// Stop every live lane: PLAYING honours the loop-end stop, captures commit and stop at once.
    pub fn stop_all(&mut self, cx: &mut Cx) -> Applied {
        let when = self.loop_end_stop.then(|| next_boundary(self.anchor, self.master, cx.now));
        let force_now = self.lanes.iter().any(|t| t.stop_at.is_some());
        for i in 0..TRACK_COUNT {
            match self.lanes[i].state {
                LaneState::Playing => match when {
                    Some(at) if !force_now => self.lanes[i].stop_at = Some(at),
                    _ => self.stop(cx, i),
                },
                LaneState::Recording | LaneState::Overdubbing => {
                    self.play_stop(cx, i);
                }
                _ => {}
            }
        }
        Applied::Done
    }

    pub fn clear_all(&mut self, cx: &mut Cx) -> Applied {
        let wait = self.wait_for(None, false);
        if wait != Applied::Done {
            return wait;
        }
        for i in 0..TRACK_COUNT {
            self.clear_now(cx, i);
        }
        self.reset_master(cx);
        Applied::Done
    }

    pub fn select(&mut self, i: usize) {
        self.clear_armed = None;
        self.selected = i.min(TRACK_COUNT - 1);
    }

    /// A hands-free press on lane `i` (the selected one when pressed): the UI's gates, spoken as
    /// refusals.
    pub fn action(&mut self, cx: &mut Cx, i: usize, action: Action) -> Applied {
        if action != Action::Clear {
            self.clear_armed = None;
        }
        let refuse = |cx: &mut Cx, reason: Refusal| cx.feed.push(Event::Refused { frame: cx.now, lane: i as u8, reason });
        match action {
            Action::RecDub => match self.rec_dub_gate(i) {
                Ok(()) => return self.rec_dub(cx, i),
                Err(reason) => refuse(cx, reason),
            },
            Action::PlayStop => match self.lanes[i].state {
                LaneState::Empty => refuse(cx, Refusal::Empty),
                _ => return self.play_stop(cx, i),
            },
            Action::Undo => {
                let info = self.info(i);
                if !info.can_undo {
                    refuse(cx, Refusal::NoUndo);
                } else if info.stop_at.is_some() {
                    refuse(cx, Refusal::Stopping);
                } else {
                    return self.undo(cx, i);
                }
            }
            Action::Clear => {
                if self.lanes[i].state == LaneState::Empty {
                    refuse(cx, Refusal::NoClear);
                } else if self.clear_armed.is_some_and(|(lane, at)| lane == i && (cx.now - at) * 1000 < CONFIRM_WINDOW_MS * self.sample_rate as Frame) {
                    let wait = self.wait_for(Some(i), false);
                    if wait != Applied::Done {
                        return wait;
                    }
                    self.clear_armed = None;
                    self.clear_now(cx, i);
                } else {
                    self.clear_armed = Some((i, cx.now));
                    refuse(cx, Refusal::ConfirmClear);
                }
            }
            Action::NextTrack => self.select((i + 1) % TRACK_COUNT),
            Action::PrevTrack => self.select((i + TRACK_COUNT - 1) % TRACK_COUNT),
            Action::PlayAll => return self.play_all(cx),
            Action::StopAll => return self.stop_all(cx),
        }
        Applied::Done
    }

    /// May the REC/DUB core of lane `i` act now (gates.ts `recDubGate`)?
    pub fn rec_dub_gate(&self, i: usize) -> Result<(), Refusal> {
        let t = &self.lanes[i];
        if self.capturing(i) {
            return Ok(());
        }
        if t.stop_at.is_some() {
            return Err(Refusal::Stopping);
        }
        if t.state == LaneState::Stopped {
            return Err(Refusal::PlayFirst);
        }
        if t.state == LaneState::Playing && t.reversed {
            return Err(Refusal::Reversed);
        }
        let other = (0..TRACK_COUNT).any(|j| j != i && self.capturing(j));
        let rolling = self.rec.is_some_and(|r| r.roll.is_some() && !self.lanes[r.lane].armed);
        if other && !(t.state == LaneState::Empty && rolling) {
            return Err(Refusal::OtherRecording);
        }
        Ok(())
    }

    // ── Takes ──────────────────────────────────────────────────────────────────────────────────────

    fn fpb(&self, cx: &Cx) -> Frame {
        frames_per_bar(cx.clock.bpm() as f64, self.sample_rate)
    }

    /// `seam`: a RETAKE handoff begins exactly where the approved take's pass ended.
    fn start_recording(&mut self, cx: &mut Cx, i: usize, seam: Option<Frame>) {
        if self.rec.is_some() {
            return;
        }
        let t = &mut self.lanes[i];
        t.written = 0;
        t.armed = false;
        t.auto_armed = false;
        let mut rec = Recorder::new(i, Kind::Take, cx.align);
        t.state = LaneState::Recording;
        if self.master == 0 {
            if self.auto_record {
                // AUTO replaces only the first-track count-in: no click, no tempo lock, no length until
                // the input triggers.
                self.detector.reset();
                t.auto_armed = true;
                self.rec = Some(rec);
                return;
            }
            // One bar of count-in from the press; the take starts on the downbeat after it.
            cx.clock.start_count_in(cx.now, COUNT_IN_BEATS, cx.now);
            cx.clock.set_locked(true);
            let downbeat = Grid::tempo(cx.now, 0, cx.clock.bpm(), self.sample_rate).beat_frame(COUNT_IN_BEATS);
            t.armed = true;
            rec.downbeat = Some(downbeat);
            rec.start = Some(downbeat + rec.align);
        } else {
            t.armed = true;
            let boundary = next_boundary(self.anchor, self.master, cx.now);
            rec.start = Some(seam.unwrap_or(boundary + rec.align));
        }
        self.rec = Some(rec);
        self.configure_end(cx);
    }

    /// Bound the take at the master, the FIXED bar count or the buffer; RETAKE rolls a known length.
    fn configure_end(&mut self, cx: &Cx) {
        let master = self.master;
        let fixed = self.fixed_length && (master == 0 || !self.retake);
        let frames = if fixed {
            let fpb = self.fpb(cx);
            clamp_bars(self.fixed_bars, max_whole_bars(if master > 0 { master } else { self.capacity }, fpb)) * fpb
        } else if master > 0 {
            master
        } else {
            self.capacity
        };
        let roll = self.retake && (master > 0 || self.fixed_length);
        let rec = self.rec.as_mut().unwrap();
        rec.end = Some(rec.start.unwrap() + frames);
        if roll {
            rec.roll = Some(Roll { pass: 1, kept: false });
        }
    }

    fn retake_plan(&self, cx: &Cx, rec: &Recorder) -> RetakeStop {
        plan_retake_stop(cx.now + rec.align, rec.end.unwrap(), self.fpb(cx), rec.roll.is_some_and(|r| r.kept))
    }

    /// A stop gesture on a capture: tighten the window end once (a repeated gesture cannot extend it).
    fn stop_capture(&mut self, cx: &mut Cx, i: usize) {
        let t = self.lanes[i];
        if t.auto_armed || t.armed {
            self.stop(cx, i); // nothing retained yet: cancel the count-in, the boundary arm or AUTO
            return;
        }
        let Some(mut rec) = self.rec.filter(|r| r.lane == i) else { return };
        let mut end = cx.now + rec.align;
        let mut bar_plan = true;
        if rec.roll.is_some() {
            let plan = self.retake_plan(cx, &rec);
            rec.roll = None;
            match plan {
                RetakeStop::KeepLast => {
                    // The kept pass replaces the one in flight, whose damage dies with it.
                    let t = &mut self.lanes[i];
                    std::mem::swap(&mut t.live, &mut self.free);
                    t.audible = t.logical();
                    t.written = rec.end.unwrap() - rec.start.unwrap();
                    rec.damaged = false;
                    self.rec = Some(rec);
                    self.finish_capture(cx, i);
                    return;
                }
                RetakeStop::FinishPass => {
                    end = rec.end.unwrap();
                    bar_plan = false; // it ends on its own pass edge
                }
                RetakeStop::StopNow => {}
            }
        }
        let start = rec.start.unwrap();
        if t.state == LaneState::Recording && bar_plan {
            // Whole bars from musical time, with the quarter-beat grace. A first take shorter than a bar
            // keeps its audio through the press and pads to one bar at the commit.
            let fpb = self.fpb(cx);
            if self.master == 0 {
                let elapsed = rec.downbeat.map_or(0, |d| cx.now - d);
                if let Some(target) = plan_free_stop(elapsed, fpb, self.capacity) {
                    end = start + target;
                }
            } else if self.master % fpb == 0 {
                end = start + plan_later_stop(cx.now, start, rec.align, fpb, self.master / fpb);
            }
        }
        rec.end = Some(rec.end.map_or(end, |e| e.min(end)));
        self.rec = Some(rec);
        if rec.end.unwrap() <= cx.now {
            self.finish_capture(cx, i);
        }
    }

    /// The capture window closed: commit (or reject a damaged one), release the recorder, and hand it
    /// on to a lane that approved a RETAKE.
    fn finish_capture(&mut self, cx: &mut Cx, i: usize) {
        let Some(rec) = self.rec.filter(|r| r.lane == i) else { return };
        let seam = rec.end;
        if rec.damaged {
            self.reject(cx, i, &rec);
        } else if rec.kind == Kind::Overdub {
            self.lanes[i].state = if rec.stop_playback { LaneState::Stopped } else { LaneState::Playing };
        } else {
            self.commit_take(cx, i, &rec);
        }
        self.release_recorder(cx, i);
        self.reset_master_if_blank(cx);
        if let (Some(next), Some(seam)) = (rec.handoff, seam) {
            if self.lanes[i].state == LaneState::Playing {
                self.start_recording(cx, next, Some(seam));
            }
        }
    }

    fn commit_take(&mut self, cx: &mut Cx, i: usize, rec: &Recorder) {
        let raw = self.lanes[i].written.min(rec.end.unwrap() - rec.start.unwrap());
        let fill = if self.master == 0 {
            let plan = plan_commit(raw, cx.clock.bpm() as f64, self.sample_rate, self.capacity);
            self.master = plan.master;
            self.anchor = commit_anchor(rec.downbeat, plan.master, cx.now);
            cx.clock.set_locked(true);
            cx.clock.start_master(self.anchor, plan.master, plan.bars, cx.now);
            TakeFill::first(raw, plan.master)
        } else {
            TakeFill::later(raw, self.fpb(cx), self.master)
        };
        let master = self.master;
        let reader = loop_pos(cx.now, self.anchor, master);
        let t = &mut self.lanes[i];
        t.length = master;
        t.written = master;
        t.armed = false;
        t.audible = t.logical();
        t.state = if rec.stop_playback { LaneState::Stopped } else { LaneState::Playing };
        let lo = fill.untouched();
        if lo < master {
            // Start where playback reads next, so the fill stays ahead of it.
            let off = if reader >= lo { reader - lo } else { 0 };
            let buf = t.live;
            self.push_job(cx.now, i, JobKind::Fill { buf, fill }, Visit { lo, span: master - lo, off, modulus: master });
        }
    }

    /// A take or layer whose window saw an input gap: discard it, keep what was there before.
    fn reject(&mut self, cx: &mut Cx, i: usize, rec: &Recorder) {
        let overdub = rec.kind == Kind::Overdub;
        cx.feed.push(Event::TakeRejected { frame: cx.now, lane: i as u8, overdub });
        if overdub {
            self.discard_layer(cx, i, rec);
            self.lanes[i].state = if rec.stop_playback { LaneState::Stopped } else { LaneState::Playing };
            return;
        }
        let t = &mut self.lanes[i];
        t.armed = false;
        t.written = 0;
        t.length = 0;
        t.state = LaneState::Empty; // a first take leaves a blank session: finish_capture resets it
    }

    /// RETAKE: the rolling take reached its window end. Set the pass aside when it is clean (a damaged
    /// pass drops the older kept pass too: approving must never hand back a pass the player did not just
    /// play) and slide the whole take one pass forward.
    fn complete_pass(&mut self, cx: &mut Cx) {
        let mut rec = self.rec.unwrap();
        let i = rec.lane;
        let frames = rec.end.unwrap() - rec.start.unwrap();
        let mut roll = rec.roll.unwrap();
        roll.kept = !rec.damaged;
        if roll.kept {
            std::mem::swap(&mut self.lanes[i].live, &mut self.free);
        } else {
            cx.feed.push(Event::PassDropped { frame: cx.now, lane: i as u8, pass: roll.pass });
        }
        roll.pass += 1;
        rec.roll = Some(roll);
        rec.damaged = false;
        rec.start = rec.end;
        rec.end = Some(rec.start.unwrap() + frames);
        rec.downbeat = rec.downbeat.map(|d| d + frames);
        let t = &mut self.lanes[i];
        t.audible = t.logical();
        t.written = 0;
        self.rec = Some(rec);
    }

    /// AUTO: the onset retained in `live[..copied]` began at input frame `onset`. The grid anchors where
    /// it was performed: `align` frames earlier.
    fn begin_auto(&mut self, cx: &mut Cx, onset: Frame) {
        let rec = self.rec.as_mut().unwrap();
        rec.align = cx.align;
        rec.start = Some(onset);
        let downbeat = onset - rec.align;
        rec.downbeat = Some(downbeat);
        self.lanes[rec.lane].auto_armed = false;
        cx.clock.set_locked(true);
        self.configure_end(cx);
        cx.clock.start_auto_record(downbeat, cx.now);
    }

    // ── Overdub ────────────────────────────────────────────────────────────────────────────────────

    fn start_overdub(&mut self, cx: &mut Cx, i: usize) {
        if self.rec.is_some() || self.lanes[i].reversed || self.master == 0 {
            return;
        }
        let master = self.master;
        let first_pos = loop_pos(cx.now, self.anchor, master);
        let mut rec = Recorder::new(i, Kind::Overdub, cx.align);
        let t = &mut self.lanes[i];
        t.audible = t.logical();
        t.switch_at = None;
        rec.prev_undo_valid = t.undo_valid;
        rec.prev_spare_reversed = t.spare_reversed;
        // The previous undo target waits in the free buffer until the layer commits or is discarded;
        // the spare becomes a copy of the loop as it is now, filled ahead of the layer's first write.
        std::mem::swap(&mut t.spare, &mut self.free);
        t.undo_valid = true;
        t.spare_reversed = t.reversed;
        t.state = LaneState::Overdubbing;
        let (src, dst) = (t.live, t.spare);
        rec.start = Some(cx.now + rec.align);
        rec.first_pos = first_pos;
        self.rec = Some(rec);
        self.push_job(cx.now, i, JobKind::Copy { src, dst }, Visit { lo: 0, span: master, off: first_pos, modulus: master });
    }

    /// Put the loop back as it was before the overdub, and the undo target from before it.
    fn discard_layer(&mut self, cx: &mut Cx, i: usize, rec: &Recorder) {
        self.jobs.iter_mut().filter(|j| j.is_some_and(|j| j.lane == i)).for_each(|j| *j = None);
        let master = self.master;
        let span = rec.summed.min(master);
        let t = self.lanes[i];
        if span == 0 {
            self.restore_undo(i, rec.prev_undo_valid, rec.prev_spare_reversed);
            return;
        }
        // Restore from where playback reads next when that lies inside the layer, so it stays ahead.
        let reader = loop_pos(cx.now, self.anchor, master);
        let into = (reader - rec.first_pos).rem_euclid(master);
        let off = if into < span { into } else { 0 };
        let kind = JobKind::Restore { src: t.spare, dst: t.live, prev_undo_valid: rec.prev_undo_valid, prev_spare_reversed: rec.prev_spare_reversed };
        self.lanes[i].undo_valid = rec.prev_undo_valid;
        self.push_job(cx.now, i, kind, Visit { lo: rec.first_pos, span, off, modulus: master });
    }

    /// Hand the previous undo target back from the free buffer.
    fn restore_undo(&mut self, i: usize, undo_valid: bool, spare_reversed: bool) {
        let t = &mut self.lanes[i];
        std::mem::swap(&mut t.spare, &mut self.free);
        t.undo_valid = undo_valid;
        t.spare_reversed = spare_reversed;
    }

    // ── Stop, resume, reset ────────────────────────────────────────────────────────────────────────

    /// Silence the lane now and keep its loop. A capture is aborted: an uncommitted take is dropped,
    /// an overdub layer discarded, a first-take count-in hands the pulse back to free-run.
    fn stop(&mut self, cx: &mut Cx, i: usize) {
        let t = self.lanes[i];
        if t.state == LaneState::Empty {
            return;
        }
        self.lanes[i].stop_at = None;
        let discard = t.state == LaneState::Recording; // a take in flight never has a loop yet
        if self.capturing(i) {
            // An aborted first take leaves a blank session, whose reset below hands the count-in pulse
            // back to free-run.
            self.lanes[i].armed = false;
            if let Some(rec) = self.rec.filter(|r| r.kind == Kind::Overdub) {
                self.discard_layer(cx, i, &rec);
            }
            self.release_recorder(cx, i);
        }
        let t = &mut self.lanes[i];
        if discard {
            t.written = 0;
        }
        t.audible = t.logical();
        t.switch_at = None;
        t.state = if t.length > 0 { LaneState::Stopped } else { LaneState::Empty };
        if discard {
            self.reset_master_if_blank(cx);
        }
    }

    /// An idle transport (a master, nothing playing or recording) restarts from the top: the grid
    /// re-anchors at the press and the pulse with it.
    fn restart_if_idle(&mut self, cx: &mut Cx) -> Option<Frame> {
        if self.master == 0 || self.rec.is_some() || self.lanes.iter().any(|t| t.state == LaneState::Playing) {
            return None;
        }
        self.anchor = cx.now;
        let bars = self.master / self.fpb(cx);
        cx.clock.start_master(cx.now, self.master, bars.max(1), cx.now);
        Some(cx.now)
    }

    /// Resume a STOPPED lane: from the top on an idle transport, else joining the live phase.
    /// `restart`: the fan-out's shared decision (PLAY ALL); `None` decides here.
    fn resume(&mut self, cx: &mut Cx, i: usize, restart: Option<Option<Frame>>) -> bool {
        if self.lanes[i].length == 0 {
            return false;
        }
        if restart.is_none() {
            self.restart_if_idle(cx);
        }
        let t = &mut self.lanes[i];
        t.stop_at = None;
        t.audible = t.logical();
        t.switch_at = None;
        t.state = LaneState::Playing;
        true
    }

    /// Release the single recorder slot if lane `i` owns it; a first take gives the tempo back.
    fn release_recorder(&mut self, cx: &mut Cx, i: usize) {
        if self.rec.is_none_or(|r| r.lane != i) {
            return;
        }
        self.rec = None;
        self.lanes[i].auto_armed = false;
        if self.master == 0 {
            cx.clock.set_locked(false);
        }
    }

    fn reset_master(&mut self, cx: &mut Cx) {
        self.master = 0;
        self.anchor = 0;
        cx.clock.set_locked(false);
        cx.clock.stop_master(cx.now);
    }

    fn reset_master_if_blank(&mut self, cx: &mut Cx) {
        if self.rec.is_none() && self.lanes.iter().all(|t| t.state == LaneState::Empty) {
            self.reset_master(cx);
        }
    }

    // ── Time ───────────────────────────────────────────────────────────────────────────────────────

    /// The earliest frame something is scheduled for (a window edge, a loop-end stop, a boundary swap,
    /// a job completing); the caller skips what is not after the frame it renders.
    pub fn next_event(&self) -> Option<Frame> {
        let lanes = self.lanes.iter().flat_map(|t| [t.stop_at, t.switch_at]).flatten();
        let rec = self.rec.iter().flat_map(|r| [r.start.filter(|_| self.lanes[r.lane].armed), r.end]).flatten();
        let jobs = self.jobs.iter().flatten().map(Job::done_frame);
        lanes.chain(rec).chain(jobs).min()
    }

    /// Run everything scheduled for `cx.now`.
    pub fn events(&mut self, cx: &mut Cx) {
        let now = cx.now;
        for i in 0..TRACK_COUNT {
            let t = &mut self.lanes[i];
            if t.switch_at.is_some_and(|at| at <= now) {
                t.audible = t.logical();
                t.switch_at = None;
            }
            if t.stop_at.is_some_and(|at| at <= now) {
                self.stop(cx, i);
            }
        }
        for k in 0..MAX_JOBS {
            if let Some(job) = self.jobs[k].filter(|j| j.done_frame() <= now) {
                self.jobs[k] = None;
                self.complete_job(cx, job);
            }
        }
        // A commit can hand the recorder on at the very same frame: loop until nothing is due.
        for _ in 0..4 {
            let Some(rec) = self.rec else { break };
            let t = self.lanes[rec.lane];
            if t.armed && rec.start.is_some_and(|s| s <= now) {
                let t = &mut self.lanes[rec.lane];
                t.armed = false;
                t.written = 0;
            }
            if rec.end.is_some_and(|e| e <= now) && !t.auto_armed {
                if rec.roll.is_some() {
                    self.complete_pass(cx);
                } else {
                    self.finish_capture(cx, rec.lane);
                }
                continue;
            }
            break;
        }
    }

    fn push_job(&mut self, now: Frame, lane: usize, kind: JobKind, visit: Visit) {
        let slot = self.jobs.iter_mut().find(|j| j.is_none()).expect("block job slots exhausted");
        *slot = Some(Job { kind, lane, visit, start: now, progress: 0 });
    }

    fn complete_job(&mut self, cx: &mut Cx, job: Job) {
        let mut job = job;
        self.run_job(&mut job, Frame::MAX);
        match job.kind {
            JobKind::Restore { prev_undo_valid, prev_spare_reversed, .. } => {
                self.restore_undo(job.lane, prev_undo_valid, prev_spare_reversed);
            }
            JobKind::LaneCopy { from, to, resume, .. } => {
                cx.feed.push(Event::Copied { frame: cx.now, from: from as u8, to: to as u8 });
                if resume && self.lanes[to].state == LaneState::Stopped {
                    self.resume(cx, to, None);
                }
            }
            JobKind::Fill { .. } | JobKind::Copy { .. } => {}
        }
    }

    /// Advance every job to the progress it owes by frame `until`.
    pub fn advance_jobs(&mut self, until: Frame) {
        for k in 0..MAX_JOBS {
            if let Some(mut job) = self.jobs[k] {
                self.run_job(&mut job, until);
                self.jobs[k] = Some(job);
            }
        }
    }

    fn run_job(&mut self, job: &mut Job, until: Frame) {
        let target = if until == Frame::MAX { job.visit.span } else { job.visit.span.min(JOB_RATE.saturating_mul(until - job.start)) };
        let (from, to) = (job.progress, target);
        if to <= from {
            return;
        }
        job.progress = to;
        self.job_step_max = self.job_step_max.max(to - from);
        let visit = job.visit;
        match job.kind {
            JobKind::Fill { buf, fill } => {
                let data = &mut self.bufs[buf];
                for s in from..to {
                    let p = visit.at(s);
                    data[p] = fill.source(p as Frame).map_or(0.0, |q| data[q as usize]);
                }
            }
            JobKind::Copy { src, dst } | JobKind::Restore { src, dst, .. } | JobKind::LaneCopy { src, dst, .. } => {
                let (s_buf, d_buf) = pair(&mut self.bufs, src, dst);
                for s in from..to {
                    let p = visit.at(s);
                    d_buf[p] = s_buf[p];
                }
            }
        }
    }

    /// The transport is audibly alive (the click is a transport mode): until `Frame::MAX` while a lane
    /// records or overdubs, else until the latest loop-end stop of a playing lane; `None` when idle.
    pub fn transport_until(&self) -> Option<Frame> {
        let mut until: Option<Frame> = None;
        for (i, t) in self.lanes.iter().enumerate() {
            if self.rec.is_some_and(|r| r.stop_playback && r.lane == i) && self.master > 0 {
                continue;
            }
            if (t.state == LaneState::Recording && !t.auto_armed) || t.state == LaneState::Overdubbing {
                return Some(Frame::MAX);
            }
            if t.state == LaneState::Playing {
                until = until.max(Some(t.stop_at.unwrap_or(Frame::MAX)));
            }
        }
        until
    }

    // ── Audio ──────────────────────────────────────────────────────────────────────────────────────

    /// An input gap just before `frame`: a capture whose window it falls inside is damaged; AUTO drops
    /// the history it can no longer join.
    pub fn input_gap(&mut self, frame: Frame) {
        let Some(rec) = self.rec.as_mut() else { return };
        if self.lanes[rec.lane].auto_armed {
            self.detector.reset();
        } else if rec.start.is_some_and(|s| frame > s) && rec.end.is_none_or(|e| frame < e) {
            rec.damaged = true;
        }
    }

    /// AUTO listening: scan `input` (from frame `f0`). On a trigger, the take begins and the frames
    /// before the returned index are the last ones rendered under the listening state.
    pub fn scan_auto(&mut self, cx: &mut Cx, f0: Frame, input: &[f32]) -> Option<usize> {
        let rec = self.rec?;
        if !self.lanes[rec.lane].auto_armed {
            return None;
        }
        let live = self.lanes[rec.lane].live;
        let at = self.detector.scan(input, autorec::threshold(self.auto_sensitivity), &mut self.bufs[live])?;
        let onset = f0 + at as Frame - self.detector.copied() as Frame;
        self.lanes[rec.lane].written = self.detector.copied() as Frame;
        self.begin_auto(cx, onset);
        Some(at)
    }

    /// Write the record tap for frames `[f0, f0 + input.len())` into the recorder's window.
    pub fn capture(&mut self, f0: Frame, input: &[f32]) {
        let Some(rec) = self.rec.as_mut() else { return };
        let Some(start) = rec.start else { return };
        let lane = &mut self.lanes[rec.lane];
        if lane.auto_armed {
            return;
        }
        let lo = f0.max(start);
        let hi = (f0 + input.len() as Frame).min(rec.end.unwrap_or(Frame::MAX));
        if lo >= hi {
            return;
        }
        let data = &mut self.bufs[lane.live];
        match rec.kind {
            Kind::Take => {
                // A take's window never outgrows the buffer (configure_end), so neither does this write.
                data[(lo - start) as usize..(hi - start) as usize].copy_from_slice(&input[(lo - f0) as usize..(hi - f0) as usize]);
                lane.written = lane.written.max(hi - start);
            }
            Kind::Overdub => {
                let master = self.master;
                let mut pos = loop_pos(lo - rec.align, self.anchor, master);
                for &x in &input[(lo - f0) as usize..(hi - f0) as usize] {
                    data[pos as usize] += x;
                    pos += 1;
                    if pos == master {
                        pos = 0;
                    }
                }
                rec.summed += hi - lo;
            }
        }
    }

    /// Add every audible lane for frames `[f0, f0 + out.len())` into `out`.
    pub fn render(&mut self, f0: Frame, out: &mut [f32]) {
        let master = self.master;
        let silent_lane = self.rec.filter(|r| r.stop_playback).map(|r| r.lane);
        for i in 0..TRACK_COUNT {
            let t = &mut self.lanes[i];
            let target = if t.muted { 0.0 } else { t.volume as f64 };
            // Only a committed lane plays, and a lane commits only onto a master.
            let playing = t.state == LaneState::Playing || (t.state == LaneState::Overdubbing && silent_lane != Some(i));
            if !playing {
                for _ in 0..out.len() {
                    t.gain = target + (t.gain - target) * self.gain_coef;
                }
                continue;
            }
            let data = &self.bufs[t.audible.buf];
            let mut pos = loop_pos(f0, self.anchor, master);
            for sample in out.iter_mut() {
                let idx = if t.audible.reversed { master - 1 - pos } else { pos };
                *sample += (t.gain * data[idx as usize] as f64) as f32;
                t.gain = target + (t.gain - target) * self.gain_coef;
                pos += 1;
                if pos == master {
                    pos = 0;
                }
            }
        }
    }

    // ── Feed ───────────────────────────────────────────────────────────────────────────────────────

    /// Emit what changed since the last publish: lane infos and the transport.
    pub fn publish(&mut self, cx: &mut Cx) {
        for i in 0..TRACK_COUNT {
            let info = self.info(i);
            if self.published[i] != Some(info) {
                self.published[i] = Some(info);
                cx.feed.push(Event::Lane { frame: cx.now, lane: i as u8, info });
            }
        }
        let transport = (self.master, cx.clock.bpm(), cx.clock.locked());
        if self.published_transport != Some(transport) {
            self.published_transport = Some(transport);
            cx.feed.push(Event::Transport { frame: cx.now, master: transport.0, bpm: transport.1, locked: transport.2 });
        }
    }
}

/// Two distinct buffers of the pool, one to read and one to write.
fn pair(bufs: &mut [Vec<f32>], src: usize, dst: usize) -> (&[f32], &mut [f32]) {
    assert_ne!(src, dst);
    if src < dst {
        let (a, b) = bufs.split_at_mut(dst);
        (&a[src], &mut b[0])
    } else {
        let (a, b) = bufs.split_at_mut(src);
        (&b[0], &mut a[dst])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_takes_one_frame_per_job_rate_positions_rounded_up() {
        assert_eq!(job_frames(1), 1);
        assert_eq!(job_frames(JOB_RATE), 1);
        assert_eq!(job_frames(JOB_RATE + 1), 2);
        assert_eq!(job_frames(2 * JOB_RATE - 1), 2);
        assert_eq!(job_frames(2 * JOB_RATE), 2);
    }
}
