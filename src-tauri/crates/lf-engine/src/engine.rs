//! OWNS: the engine's callback: the command and event rings, the block split, and the bus topology.
//! Input → the plugin inserts → the wet signal. The record tap is the wet signal plus the built-in
//! instruments (`instruments`, delayed onto the guitar's grid). Each lane plays through its FX chain,
//! whose reverb sends meet on one bus (`effects`); the chains, the reverb bus, the instruments and the
//! click sum on the stereo master bus: master volume, then the master limiter (`dsp::compressor`) →
//! stereo out. The wet signal joins the output after the limiter, under the same master volume: the
//! played instrument is heard without the limiter's pre-delay, as the native monitor is today, and like
//! it is not limited. Ported from `src/audio/engine.ts` and `src/audio/master.ts`.
//!
//! The limiter delays everything it carries by its pre-delay, the click included, so a take's alignment
//! is `align_frames` + the plugin's latency + the limiter's.
//!
//! `process` renders a block in chunks that end wherever something happens: a command's frame, a
//! scheduled looper event, a beat, an AUTO trigger, and a render quantum's end (the FX and the reverb
//! bus work per quantum). Every looper state change therefore lands on its exact frame, and the same
//! commands give bit-identical output at any block size. A note sounds a fixed lead after its frame
//! (`instruments`); a wheel or an FX change sounds from the next 128-frame quantum boundary (Blink
//! with no look-ahead, `dsp::param::context_frame`).

use std::sync::Arc;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::api::{Command, Event, Inserts, ProcessContext, TimedCommand, TRACK_COUNT};
use crate::clock::Clock;
use crate::dsp::buffer_source::AudioBuffer;
use crate::dsp::compressor::Compressor;
use crate::dsp::noise::NoiseTables;
use crate::dsp::param::QUANTUM;
use crate::dsp::rng::Mulberry32;
use crate::effects::LaneFx;
use crate::grid::Frame;
use crate::instruments::Instruments;
use crate::looper::{Applied, Cx, Looper};

/// Commands the engine holds for a future frame (MIDI press frames, a wait for a block job).
const MAX_PENDING: usize = 64;
/// The seed of Tone's noise tables (a `Math.random` stand-in): fixed, so a render repeats.
const NOISE_SEED: u32 = 7;
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

/// [`Engine::taps`]: the last block, before the limiter.
pub struct Taps<'a> {
    /// The master bus's two sides: everything but the wet signal, under the master volume.
    pub bus: [&'a [f32]; 2],
    /// The wet signal under the master volume.
    pub monitor: &'a [f32],
    /// The lanes before their FX plus the click, under the master volume: what the looper plays,
    /// without the FX's colour (a bypassed chain is not bit-transparent, as in Tone).
    pub looper: &'a [f32],
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
    fx: LaneFx,
    instruments: Instruments,
    wet: Vec<f32>,
    /// The record tap: the wet signal plus the instruments' record path.
    record: Vec<f32>,
    /// The instruments' frames rendered so far in this block (they render ahead of an AUTO scan).
    instruments_done: usize,
    instrument: [Vec<f32>; 2],
    lanes: [[f32; QUANTUM]; TRACK_COUNT],
    click: [f32; QUANTUM],
    /// The stereo master bus: the lanes through their FX, the reverb bus, the instruments and the click,
    /// then under the master volume (the limiter's input).
    mix: [Vec<f32>; 2],
    /// The wet signal under the master volume: what joins the output after the limiter.
    monitor: Vec<f32>,
    /// The looper's own mix under the master volume: the lanes before their FX, and the click.
    looper_mix: Vec<f32>,
    /// The frames `process` rendered last: the length of the taps.
    rendered: usize,
    limiter: Compressor,
    master_volume: f32,
    master_muted: bool,
    master_gain: f64,
    master_coef: f64,
    started: bool,
    /// The frame the next block should start at, and the device frames skipped so far (the DSP clock's
    /// offset: see `effects`).
    next_frame: Frame,
    skipped: Frame,
    commands_dropped: u64,
    xruns: u64,
}

impl Engine {
    pub fn new(config: EngineConfig) -> (Engine, EngineHandle) {
        let (cmd_tx, cmd_rx) = RingBuffer::new(config.command_capacity);
        let (evt_tx, evt_rx) = RingBuffer::new(config.event_capacity);
        let capacity = (config.max_loop_seconds * config.sample_rate as f64).ceil() as Frame;
        // Tone generates its noise tables once; the drum kit and the reverb IR share them.
        let NoiseTables { white, pink } = NoiseTables::generate(&mut Mulberry32::new(NOISE_SEED));
        let table = |[l, r]: [Vec<f32>; 2]| Arc::new(AudioBuffer::new(config.sample_rate as f32, vec![l, r]));
        let (white, pink) = (table(white), table(pink));
        let engine = Engine {
            config,
            clock: Clock::new(config.sample_rate),
            looper: Looper::new(config.sample_rate, capacity),
            feed: Feed { tx: evt_tx, dropped: 0 },
            commands: cmd_rx,
            pending: [None; MAX_PENDING],
            seq: 0,
            fx: LaneFx::new(config.sample_rate, &white),
            instruments: Instruments::new(config.sample_rate, config.max_block, &white, &pink),
            wet: vec![0.0; config.max_block],
            record: vec![0.0; config.max_block],
            instruments_done: 0,
            instrument: [vec![0.0; config.max_block], vec![0.0; config.max_block]],
            lanes: [[0.0; QUANTUM]; TRACK_COUNT],
            click: [0.0; QUANTUM],
            mix: [vec![0.0; config.max_block], vec![0.0; config.max_block]],
            monitor: vec![0.0; config.max_block],
            looper_mix: vec![0.0; config.max_block],
            rendered: 0,
            // Built here, so its start-up gain dip passes on the first frames the device renders.
            limiter: Compressor::master_limiter(config.sample_rate as f32),
            master_volume: 1.0,
            master_muted: false,
            master_gain: 1.0,
            master_coef: (-1.0 / (MASTER_TAU_SECONDS * config.sample_rate as f64)).exp(),
            started: false,
            next_frame: 0,
            skipped: 0,
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

    pub fn fx(&self) -> &LaneFx {
        &self.fx
    }

    pub fn instruments(&self) -> &Instruments {
        &self.instruments
    }

    /// True while a command waits for a block job.
    pub fn holding(&self) -> bool {
        self.pending.iter().flatten().any(|p| p.held)
    }

    /// Frames the limiter delays the master bus by (Blink's 6 ms pre-delay, truncated).
    pub fn limiter_latency(&self) -> Frame {
        self.limiter.latency() as Frame
    }

    /// The last block before the limiter. The output is `limiter(bus) + monitor` on each side.
    pub fn taps(&self) -> Taps<'_> {
        let n = self.rendered;
        Taps { bus: [&self.mix[0][..n], &self.mix[1][..n]], monitor: &self.monitor[..n], looper: &self.looper_mix[..n] }
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
            self.skipped = start;
        } else if start != self.next_frame {
            self.skipped += start - self.next_frame;
        }
        self.next_frame = end;
        self.fx.set_offset(self.skipped);
        self.instruments.set_offset(self.skipped);
        if ctx.xrun {
            self.xruns += 1;
            self.looper.input_gap(start);
        }
        // Take no more than the table holds: the rest waits in the ring for the next block, late but
        // never dropped (a lost NoteOff would hang its note).
        while self.pending.iter().any(Option::is_none) {
            let Ok(cmd) = self.commands.pop() else { break };
            self.hold(cmd.frame.unwrap_or(start).max(start), cmd.command);
        }
        inserts.process(start, input, &mut self.wet[..n]);
        let align = ctx.align_frames + inserts.latency() + self.limiter.latency() as Frame;
        let record_delay = ctx.input_frames + inserts.latency();
        self.rendered = n;
        self.record[..n].copy_from_slice(&self.wet[..n]);
        self.instruments_done = 0;
        let [mix_l, mix_r] = &mut self.mix;
        let (mix_l, mix_r) = (&mut mix_l[..n], &mut mix_r[..n]);

        let mut f = start;
        while f < end {
            let mut cx = Cx { now: f, align, clock: &mut self.clock, feed: &mut self.feed, fx: &mut self.fx };
            self.looper.events(&mut cx);
            while let Some(k) = due(&self.pending, f) {
                let p = self.pending[k].take().unwrap();
                let mut at = Apply { instruments: &mut self.instruments, master_volume: &mut self.master_volume, master_muted: &mut self.master_muted };
                match apply(&mut self.looper, &mut cx, &mut at, p.command) {
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
            self.fx.follow_grid(self.looper.anchor(), self.looper.master(), self.clock.bpm(), f);

            let quantum_end = f - (f - self.skipped).rem_euclid(QUANTUM as Frame) + QUANTUM as Frame;
            let mut next = end.min(quantum_end);
            for at in [first_pending(&self.pending, f), self.looper.next_event(), self.clock.next_beat_frame()].into_iter().flatten() {
                if at > f {
                    next = next.min(at);
                }
            }
            self.looper.advance_jobs(next);
            let k0 = (f - start) as usize;
            // The instruments reach the record tap before the AUTO scan reads it: render them up to
            // `next`, and a trigger that ends the chunk early leaves the rest for the next chunk.
            let k_next = (next - start) as usize;
            if self.instruments_done < k_next {
                let (from, [il, ir]) = (self.instruments_done, &mut self.instrument);
                let at = start + from as Frame;
                self.instruments.render(at, record_delay, &mut il[from..k_next], &mut ir[from..k_next], &mut self.record[from..k_next]);
                self.instruments_done = k_next;
            }
            let mut cx = Cx { now: f, align, clock: &mut self.clock, feed: &mut self.feed, fx: &mut self.fx };
            if let Some(at) = self.looper.scan_auto(&mut cx, f, &self.record[k0..k_next]) {
                next = f + at as Frame;
            }
            let k1 = (next - start) as usize;
            let len = k1 - k0;
            let (l, r) = (&mut mix_l[k0..k1], &mut mix_r[k0..k1]);
            l.copy_from_slice(&self.instrument[0][k0..k1]);
            r.copy_from_slice(&self.instrument[1][k0..k1]);
            self.looper.render(f, len, &mut self.lanes);
            self.fx.render(f, &self.lanes, l, r);
            let click = &mut self.click[..len];
            click.fill(0.0);
            self.clock.render_click(f, click);
            let looper_mix = &mut self.looper_mix[k0..k1];
            looper_mix.copy_from_slice(click);
            for lane in &self.lanes {
                for (m, &x) in looper_mix.iter_mut().zip(&lane[..len]) {
                    *m += x;
                }
            }
            for ((l, r), &c) in l.iter_mut().zip(r.iter_mut()).zip(click.iter()) {
                *l += c;
                *r += c;
            }
            self.looper.capture(f, &self.record[k0..k1]);
            let target = if self.master_muted { 0.0 } else { self.master_volume as f64 };
            for ((((l, r), o), m), &w) in l.iter_mut().zip(r.iter_mut()).zip(&mut self.monitor[k0..k1]).zip(looper_mix.iter_mut()).zip(&self.wet[k0..k1]) {
                let g = self.master_gain;
                *l = (g * *l as f64) as f32;
                *r = (g * *r as f64) as f32;
                *o = (g * w as f64) as f32;
                *m = (g * *m as f64) as f32;
                self.master_gain = target + (self.master_gain - target) * self.master_coef;
            }
            f = next;
        }
        left.copy_from_slice(mix_l);
        right.copy_from_slice(mix_r);
        self.limiter.process(start, left, right);
        for ((l, r), m) in left.iter_mut().zip(right.iter_mut()).zip(&self.monitor[..n]) {
            *l += m;
            *r += m;
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
/// it waits behind that one. Due commands run in the order they were sent. A command for the
/// instruments never waits behind the looper: it touches nothing a block job moves, and a note must
/// sound when it is played (in the web app notes go straight to the synth).
fn due(pending: &[Option<Pending>; MAX_PENDING], f: Frame) -> Option<usize> {
    let barrier = pending.iter().flatten().filter(|p| p.held && p.frame > f).map(|p| p.seq).min();
    pending
        .iter()
        .enumerate()
        .filter_map(|(k, p)| {
            p.filter(|p| p.frame <= f && (barrier.is_none_or(|b| p.seq < b) || p.command.is_instrument())).map(|p| (p.seq, k))
        })
        .min()
        .map(|(_, k)| k)
}

/// The next frame after `f` a command is stamped for (a command already due but waiting behind a held
/// one is not a boundary; the held one's frame is).
fn first_pending(pending: &[Option<Pending>; MAX_PENDING], f: Frame) -> Option<Frame> {
    pending.iter().flatten().map(|p| p.frame).filter(|&at| at > f).min()
}

fn lane(i: u8) -> Option<usize> {
    let i = i as usize;
    (i < TRACK_COUNT).then_some(i)
}

/// What a command reaches besides the looper and its context.
struct Apply<'a> {
    instruments: &'a mut Instruments,
    master_volume: &'a mut f32,
    master_muted: &'a mut bool,
}

fn apply(looper: &mut Looper, cx: &mut Cx, at: &mut Apply, command: Command) -> Applied {
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
            match apply(looper, cx, at, Command::ActionOn(i, action)) {
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
            *at.master_volume = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
            Applied::Done
        }
        Command::SetMasterMute(on) => {
            *at.master_muted = on;
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
        Command::SetFxParam(i, param, value) => {
            if let Some(i) = lane(i) {
                cx.fx.set_param(i, param, value, now);
            }
            Applied::Done
        }
        Command::SetFxBypass(i, kind, bypassed) => {
            if let Some(i) = lane(i) {
                cx.fx.set_bypass(i, kind, bypassed, now);
            }
            Applied::Done
        }
        Command::SelectInstrument(instrument) => {
            at.instruments.select(instrument, now);
            Applied::Done
        }
        Command::NoteOn(note, velocity) => {
            at.instruments.note_on(note, velocity, now);
            Applied::Done
        }
        Command::NoteOff(note) => {
            at.instruments.note_off(note, now);
            Applied::Done
        }
        Command::PitchBend(semitones) => {
            at.instruments.set_pitch_bend(semitones, now);
            Applied::Done
        }
        Command::Modulation(depth) => {
            at.instruments.set_modulation(depth, now);
            Applied::Done
        }
        Command::AllNotesOff => {
            at.instruments.all_notes_off(now);
            Applied::Done
        }
    }
}
