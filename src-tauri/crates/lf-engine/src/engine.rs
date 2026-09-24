//! OWNS: the engine's callback: the command and event rings, the block split, and the bus topology.
//! Input → the plugin inserts → the input bus, which is the record tap (pre-limiter) and joins the
//! lanes and the click on the master bus (master volume, then the limiter slot) → stereo out. Ported
//! from `src/audio/engine.ts` and `src/audio/master.ts`.
//!
//! `process` renders a block in chunks that end wherever something happens: a command's frame, a
//! scheduled looper event, a beat, an AUTO trigger. Every state change therefore lands on its exact
//! frame, and the same commands give bit-identical output at any block size.

use rtrb::{Consumer, Producer, RingBuffer};

use crate::api::{Command, Event, Inserts, ProcessContext, TimedCommand, TRACK_COUNT};
use crate::clock::Clock;
use crate::grid::Frame;
use crate::looper::{Applied, Cx, Looper};

/// Commands the engine holds for a future frame (MIDI press frames, a wait for a block job).
const MAX_PENDING: usize = 64;
/// Master volume smoothing (master.ts `RAMP_TC`).
const MASTER_TAU_SECONDS: f64 = 0.012;

#[derive(Clone, Copy, Debug)]
pub struct EngineConfig {
    pub sample_rate: u32,
    /// Lane buffer length. Every buffer is allocated in `Engine::new`.
    pub max_loop_seconds: f64,
    /// The largest block `process` accepts.
    pub max_block: usize,
    pub command_capacity: usize,
    pub event_capacity: usize,
}

impl EngineConfig {
    pub fn new(sample_rate: u32) -> Self {
        EngineConfig { sample_rate, max_loop_seconds: 60.0, max_block: 4096, command_capacity: 256, event_capacity: 4096 }
    }
}

/// The non-RT side: send commands, read the feed.
pub struct EngineHandle {
    pub commands: Producer<TimedCommand>,
    pub events: Consumer<Event>,
}

/// The event ring's producer. A full ring drops the event and counts it: the audio never waits.
pub struct Feed {
    tx: Producer<Event>,
    dropped: u64,
}

impl Feed {
    pub fn push(&mut self, event: Event) {
        if self.tx.push(event).is_err() {
            self.dropped += 1;
        }
    }
}

/// Counters the owner reports (Stage 4 mirrors them into the diag).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Diag {
    pub events_dropped: u64,
    pub commands_dropped: u64,
    pub xruns: u64,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    frame: Frame,
    seq: u64,
    command: Command,
    /// Waiting for a block job: every command sent after it waits behind it, so presses keep their order.
    held: bool,
}

pub struct Engine {
    config: EngineConfig,
    clock: Clock,
    looper: Looper,
    feed: Feed,
    commands: Consumer<TimedCommand>,
    pending: [Option<Pending>; MAX_PENDING],
    seq: u64,
    wet: Vec<f32>,
    mix: Vec<f32>,
    master_volume: f32,
    master_muted: bool,
    master_gain: f64,
    master_coef: f64,
    started: bool,
    commands_dropped: u64,
    xruns: u64,
}

impl Engine {
    pub fn new(config: EngineConfig) -> (Engine, EngineHandle) {
        let (cmd_tx, cmd_rx) = RingBuffer::new(config.command_capacity);
        let (evt_tx, evt_rx) = RingBuffer::new(config.event_capacity);
        let capacity = (config.max_loop_seconds * config.sample_rate as f64).ceil() as Frame;
        let engine = Engine {
            config,
            clock: Clock::new(config.sample_rate),
            looper: Looper::new(config.sample_rate, capacity),
            feed: Feed { tx: evt_tx, dropped: 0 },
            commands: cmd_rx,
            pending: [None; MAX_PENDING],
            seq: 0,
            wet: vec![0.0; config.max_block],
            mix: vec![0.0; config.max_block],
            master_volume: 1.0,
            master_muted: false,
            master_gain: 1.0,
            master_coef: (-1.0 / (MASTER_TAU_SECONDS * config.sample_rate as f64)).exp(),
            started: false,
            commands_dropped: 0,
            xruns: 0,
        };
        (engine, EngineHandle { commands: cmd_tx, events: evt_rx })
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub fn looper(&self) -> &Looper {
        &self.looper
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// True while a command waits for a block job.
    pub fn holding(&self) -> bool {
        self.pending.iter().flatten().any(|p| p.held)
    }

    pub fn diag(&self) -> Diag {
        Diag { events_dropped: self.feed.dropped, commands_dropped: self.commands_dropped, xruns: self.xruns }
    }

    /// Render one block: `input` is the mono device input, `left`/`right` the output (same length).
    pub fn process(&mut self, ctx: &ProcessContext, input: &[f32], left: &mut [f32], right: &mut [f32], inserts: &mut dyn Inserts) {
        let n = input.len();
        assert!(n <= self.config.max_block && left.len() == n && right.len() == n, "block larger than max_block");
        let start = ctx.frame;
        let end = start + n as Frame;
        if !self.started {
            self.started = true;
            self.clock.ensure_running(start);
        }
        if ctx.xrun {
            self.xruns += 1;
            self.looper.input_gap(start);
        }
        while let Ok(cmd) = self.commands.pop() {
            self.hold(cmd.frame.unwrap_or(start).max(start), cmd.command);
        }
        inserts.process(start, input, &mut self.wet[..n]);
        let align = ctx.align_frames + inserts.latency();
        let mix = &mut self.mix[..n];
        mix.fill(0.0);

        let mut f = start;
        while f < end {
            let mut cx = Cx { now: f, align, clock: &mut self.clock, feed: &mut self.feed };
            self.looper.events(&mut cx);
            while let Some(k) = due(&self.pending, f) {
                let p = self.pending[k].take().unwrap();
                match apply(&mut self.looper, &mut cx, &mut self.master_volume, &mut self.master_muted, p.command) {
                    Applied::WaitUntil(at) => insert(&mut self.pending, &mut self.commands_dropped, Pending { frame: at.max(f + 1), held: true, ..p }),
                    Applied::Held(at, command) => {
                        insert(&mut self.pending, &mut self.commands_dropped, Pending { frame: at.max(f + 1), command, held: true, ..p })
                    }
                    Applied::Done => {}
                }
                self.looper.events(&mut cx);
            }
            while let Some(beat) = cx.clock.fire_due(f, self.looper.transport_until()) {
                cx.feed.push(Event::Beat { frame: f, beat_in_bar: beat.beat_in_bar, count_left: beat.count_left, clicked: beat.clicked });
            }
            self.looper.publish(&mut cx);

            let mut next = end;
            for at in [first_pending(&self.pending), self.looper.next_event(f), cx.clock.next_beat_frame()].into_iter().flatten() {
                if at > f {
                    next = next.min(at);
                }
            }
            self.looper.advance_jobs(next);
            let k0 = (f - start) as usize;
            if let Some(at) = self.looper.scan_auto(&mut cx, f, &self.wet[k0..(next - start) as usize]) {
                next = f + at as Frame;
            }
            let k1 = (next - start) as usize;
            let chunk = &mut mix[k0..k1];
            self.looper.render(f, chunk);
            self.clock.render_click(f, chunk);
            for (m, w) in chunk.iter_mut().zip(&self.wet[k0..k1]) {
                *m += w;
            }
            self.looper.capture(f, &self.wet[k0..k1]);
            let target = if self.master_muted { 0.0 } else { self.master_volume as f64 };
            for k in k0..k1 {
                let out = (self.master_gain * mix[k] as f64) as f32;
                self.master_gain = target + (self.master_gain - target) * self.master_coef;
                // The limiter slot: Blink's DynamicsCompressorKernel is ported literally in Stage 3.
                left[k] = out;
                right[k] = out;
            }
            f = next;
        }
    }

    fn hold(&mut self, frame: Frame, command: Command) {
        self.seq += 1;
        insert(&mut self.pending, &mut self.commands_dropped, Pending { frame, seq: self.seq, command, held: false });
    }
}

fn insert(pending: &mut [Option<Pending>; MAX_PENDING], dropped: &mut u64, p: Pending) {
    match pending.iter_mut().find(|slot| slot.is_none()) {
        Some(slot) => *slot = Some(p),
        None => *dropped += 1,
    }
}

/// The first-sent command due at or before `f`, unless an earlier-sent one waits for a block job: then
/// it waits behind that one. Due commands run in the order they were sent.
fn due(pending: &[Option<Pending>; MAX_PENDING], f: Frame) -> Option<usize> {
    let barrier = pending.iter().flatten().filter(|p| p.held && p.frame > f).map(|p| p.seq).min();
    pending
        .iter()
        .enumerate()
        .filter_map(|(k, p)| p.filter(|p| p.frame <= f && barrier.is_none_or(|b| p.seq < b)).map(|p| (p.seq, k)))
        .min()
        .map(|(_, k)| k)
}

fn first_pending(pending: &[Option<Pending>; MAX_PENDING]) -> Option<Frame> {
    pending.iter().flatten().map(|p| p.frame).min()
}

fn lane(i: u8) -> Option<usize> {
    let i = i as usize;
    (i < TRACK_COUNT).then_some(i)
}

fn apply(looper: &mut Looper, cx: &mut Cx, master_volume: &mut f32, master_muted: &mut bool, command: Command) -> Applied {
    let now = cx.now;
    match command {
        Command::RecDub(i) => lane(i).map_or(Applied::Done, |i| looper.rec_dub(cx, i)),
        Command::PlayStop(i) => lane(i).map_or(Applied::Done, |i| looper.play_stop(cx, i)),
        Command::Stop(i) => lane(i).map_or(Applied::Done, |i| looper.stop_command(cx, i)),
        Command::Undo(i) => lane(i).map_or(Applied::Done, |i| looper.undo(cx, i)),
        Command::Reverse(i) => lane(i).map_or(Applied::Done, |i| looper.reverse(cx, i)),
        Command::Copy(i) => lane(i).map_or(Applied::Done, |i| looper.copy(cx, i)),
        Command::Clear(i) => lane(i).map_or(Applied::Done, |i| looper.clear(cx, i)),
        Command::PlayAll => looper.play_all(cx),
        Command::StopAll => looper.stop_all(cx),
        Command::ClearAll => looper.clear_all(cx),
        Command::Action(action) => {
            let i = looper.selected() as u8;
            match apply(looper, cx, master_volume, master_muted, Command::ActionOn(i, action)) {
                Applied::WaitUntil(at) => Applied::Held(at, Command::ActionOn(i, action)),
                done => done,
            }
        }
        Command::ActionOn(i, action) => {
            let Some(i) = lane(i) else { return Applied::Done };
            let before = looper.selected();
            let applied = looper.action(cx, i, action);
            if looper.selected() != before {
                cx.feed.push(Event::Selected { frame: now, lane: looper.selected() as u8 });
            }
            applied
        }
        Command::SelectTrack(i) => {
            looper.select(i as usize);
            cx.feed.push(Event::Selected { frame: now, lane: looper.selected() as u8 });
            Applied::Done
        }
        Command::SetBpm(bpm) => {
            cx.clock.set_bpm(bpm, now);
            Applied::Done
        }
        Command::SetMetronome(on) => {
            cx.clock.set_metronome(on);
            Applied::Done
        }
        Command::SetClickVolume(v) => {
            cx.clock.set_click_volume(v);
            Applied::Done
        }
        Command::SetMasterVolume(v) => {
            *master_volume = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
            Applied::Done
        }
        Command::SetMasterMute(on) => {
            *master_muted = on;
            Applied::Done
        }
        Command::SetLoopEndStop(on) => {
            looper.set_loop_end_stop(on);
            Applied::Done
        }
        Command::SetFixedLength(on) => {
            looper.set_fixed_length(on);
            Applied::Done
        }
        Command::SetFixedBars(bars) => {
            looper.set_fixed_bars(bars);
            Applied::Done
        }
        Command::SetRetake(on) => {
            looper.set_retake(on);
            Applied::Done
        }
        Command::SetAutoRecord(on) => {
            looper.set_auto_record(on);
            Applied::Done
        }
        Command::SetAutoSensitivity(s) => {
            looper.set_auto_sensitivity(s);
            Applied::Done
        }
        Command::SetVolume(i, v) => {
            if let Some(i) = lane(i) {
                looper.set_volume(i, v);
            }
            Applied::Done
        }
        Command::SetMute(i, on) => {
            if let Some(i) = lane(i) {
                looper.set_mute(i, on);
            }
            Applied::Done
        }
    }
}
