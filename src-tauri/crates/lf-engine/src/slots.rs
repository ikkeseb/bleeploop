//! OWNS: the slots' chains inside the callback: the units the host installs and takes back through each
//! slot's [`SlotPort`] (each under its [`UnitId`], each message answered by an [`Event::Seat`]), their
//! crossfades, the notes a slot holds, the live flag and gain, and where a slot's output goes. The
//! processors themselves are the host's (`src-tauri/src/host`, which implements [`UnitProcessor`] for
//! CLAP, VST3 and VST2) or a test's.
//!
//! Every signal here is a stereo pair, planar: a slot's input is one channel or a pair as the device
//! side picked it (a mono pick is that channel on both sides), and every unit takes and gives two
//! channels (`docs/unit-contract.md` §7). Mono is the same samples twice, and renders bit for bit what
//! the mono slot rendered.
//!
//! A slot is a **chain** of at most [`MAX_UNITS`] seats, in order: `input → LIVE gate → unit → unit →
//! … → slot gain → the input OTT`. Its role is its head's ([`SlotKind`]): a chain headed by an effect
//! (or empty) passes its input on while it is live and gets silence otherwise, through its live gate: a
//! linear ramp over the frames of `LIVE_SECONDS` from the command's frame (STATUS D23), on the input
//! alone, so an effect's own tail rings out after it goes off. Its output is the wet signal, which the
//! engine hears after the limiter at once and records at the take's alignment: the largest latency sum
//! of a live input-fed chain, so a live chain with less (an empty one, a quicker effect) reaches the
//! record tap that much later and two live inputs land together. That record compensation is latched
//! ([`Rack::latch_record`]): it holds while a capture runs, so a slot going live or not, or a unit
//! coming or going, never moves what records mid-take (a slot off live feeds its line silence, and what
//! was on the way drains in place). A chain headed by a source takes no input and plays the notes while
//! its slot is the note target; its output joins the master bus and goes to the record tap delayed like
//! a built-in instrument's, less the chain's own latency (`instruments`). An install that would sum a
//! chain past [`max_chain_latency`] is refused, and every line holds the device's input side plus that
//! plus a block ([`Rack::new`]), so no delay is ever clipped.
//!
//! Each slot runs its own instance of the input OTT (`dsp::ott`, stereo, its detectors linked) on its
//! output, after the slot's gain and before that output splits into the wet signal, the bus and the
//! record lines: an effect chain's, a source chain's and an empty live slot's dry input alike, so what
//! is heard and what is recorded are the same processed signal. A source chain's input fading out of
//! the wet over its head's install or removal does not pass it. Every instance always runs (off and
//! settled, bit-transparent) and takes the engine's one OTT setting on the same frame
//! ([`Rack::set_ott`]).
//!
//! Lifecycle never stops the audio. The host builds and activates a unit off the audio thread and
//! installs it at the chain's end: the seat crossfades in from its input (what the chain passed there
//! before) over the frames of `FADE_SECONDS`, counted, so both ends land on an exact frame and are
//! bit-exact. A source's output joins the bus, not the wet, so there it fades in from silence while the
//! gated input fades out of the wet over the same frames (and back at its removal): a live slot's dry
//! signal never steps. A removal first releases the notes the slot holds and crossfades the seat to its
//! input; the seat is gone from the frame the fade ends, and the units after it take its input. The
//! release reaches every unit in the fade's first `process`, [`UnitProcessor::stop`] follows the
//! leaving unit's last, and the unit goes back on the port's return ring with its id. A message leaves
//! its ring only once its answer has a place among those the slot owes the event ring and, an install,
//! once the return ring has a place for it beside every unit the slot holds: a full ring keeps the
//! message waiting, in order, and nothing owed is ever dropped. With no device running the ports are
//! serviced at once, without fades, and one silent frame of `process` carries a removed unit's
//! release. Nothing here drops a unit: an eviction hands back every seat and every install its ring
//! has room for (the host reads its returns and evicts again for the rest), a refused install goes
//! straight back with its reason, never started or stopped, and a unit's methods run only while a seat holds it, so a
//! panic the callback's guard catches unwinds past no unit held as a local. A call whose output holds a
//! NaN or an infinity is silenced, counted, so nothing downstream (the mix, the input sends, the
//! limiter, a capture) ever sees it.
//!
//! E1 seats at the chain's end and removes; a move, a bypass, a held edit and a source replaced in
//! place are E2's (`docs/unit-contract.md` §9). A removal releases every note the slot holds, in every
//! seat.
//!
//! The engine renders the slots once per range, not per chunk: from where they stopped up to the next
//! frame a slot command waits for (a note, the target, a slot's live flag or gain, the OTT's switch or
//! a control), so a plugin sees one call per device block unless a stamped slot command splits it.
//! Those commands therefore apply at the start of a render call (every event offset is 0 in practice);
//! an install or removal applies at the next block start. A live ramp's end splits nothing: the gate is
//! evaluated per frame.

use std::sync::atomic::{AtomicU64, Ordering::{Acquire, Release}};
use std::sync::Arc;

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::api::{max_chain_latency, Event, Pair, SeatOp, SeatOutcome, SeatRefusal, SlotEvent, SlotEventKind, SlotKind, UnitCx, UnitId, UnitInfo, UnitKind, UnitProcessor, MAX_UNITS, SLOT_COUNT};
use crate::dsp::ott::{Setting, StereoOtt};
use crate::engine::Feed;
use crate::grid::{Frame, Meter};

/// The crossfade into and out of a seat: 10 ms, the web monitor's declick (`audio_output.rs`).
const FADE_SECONDS: f64 = 0.010;
/// The live gate's ramp: 5 ms (STATUS D23).
const LIVE_SECONDS: f64 = 0.005;
/// Slot gain smoothing, as the master volume's.
const GAIN_TAU_SECONDS: f64 = 0.012;
/// Note events one slot queues between two render calls; more are dropped and counted.
pub const MAX_SLOT_EVENTS: usize = 256;
/// Messages one slot's port holds: a whole chain's installs and removals with room to spare.
const MSG_CAPACITY: usize = 2 * MAX_UNITS;
/// Units on their way back: every seat and every message's install at once.
const RETURN_CAPACITY: usize = MAX_UNITS + MSG_CAPACITY;
/// Answers ([`Event::Seat`]) one slot owes the event ring: one per message.
const OWED_CAPACITY: usize = 2 * MSG_CAPACITY;

type Unit = Box<dyn UnitProcessor>;

enum SlotMsg {
    Install { id: UnitId, unit: Unit },
    Remove { id: UnitId },
}

/// A unit on its way back to the host, refused at its install or not.
type Back = (UnitId, Unit, Option<SeatRefusal>);

/// The host's end of one slot (`EngineHandle::slots`). Not real-time: the host blocks on it, the engine
/// never does.
pub struct SlotPort {
    tx: Producer<SlotMsg>,
    rx: Consumer<Back>,
    /// Messages pushed, and those the engine has taken (stored after it applied each and handed back
    /// what it refused).
    sent: u64,
    taken: Arc<AtomicU64>,
}

/// One message on a slot's port ([`SlotPort::install`]): whether the engine has taken it yet.
#[derive(Clone)]
pub struct Ticket {
    seq: u64,
    taken: Arc<AtomicU64>,
}

impl Ticket {
    /// The engine has taken the message and applied it: a refused install is on the port's return ring
    /// ([`SlotPort::refused`]), a seated one is in its chain.
    pub fn answered(&self) -> bool {
        self.taken.load(Acquire) >= self.seq
    }

    /// Whether `port` is the one this message went to (a rebuild replaces the ports with the engine).
    pub fn of(&self, port: &SlotPort) -> bool {
        Arc::ptr_eq(&self.taken, &port.taken)
    }
}

impl SlotPort {
    /// Hand an activated unit to the slot under `id`, to seat at its chain's end; it crossfades in from
    /// the next block and is answered by an [`Event::Seat`] (a refused one comes back on
    /// [`SlotPort::returned`], its reason on [`SlotPort::refused`]). The ticket tells when the engine
    /// has taken it. Gives the unit back when the message ring is full (the engine is not rendering and
    /// has not been serviced, or the host has not read its returns).
    pub fn install(&mut self, id: UnitId, unit: Unit) -> Result<Ticket, Unit> {
        self.tx.push(SlotMsg::Install { id, unit }).map_err(|PushError::Full(msg)| match msg {
            SlotMsg::Install { unit, .. } => unit,
            SlotMsg::Remove { .. } => unreachable!(),
        })?;
        self.sent += 1;
        Ok(Ticket { seq: self.sent, taken: self.taken.clone() })
    }

    /// Ask for unit `id` back: the slot releases its notes, the seat crossfades to its input, the unit
    /// stops and arrives on [`SlotPort::returned`]. Answered by an [`Event::Seat`]; nothing comes back
    /// for an id the slot does not seat. False when the ring is full.
    pub fn remove(&mut self, id: UnitId) -> bool {
        let pushed = self.tx.push(SlotMsg::Remove { id }).is_ok();
        self.sent += pushed as u64;
        pushed
    }

    /// A unit the engine handed back, with its id: removed, evicted ([`crate::Engine::evict_slots`]) or
    /// refused.
    pub fn returned(&mut self) -> Option<(UnitId, Unit)> {
        self.rx.pop().ok().map(|(id, unit, _)| (id, unit))
    }

    /// Unit `id` with why the engine refused its install, if that is what the port hands back next.
    pub fn refused(&mut self, id: UnitId) -> Option<(Unit, SeatRefusal)> {
        if !self.rx.peek().is_ok_and(|(i, _, why)| *i == id && why.is_some()) {
            return None;
        }
        let (_, unit, why) = self.rx.pop().ok()?;
        why.map(|why| (unit, why))
    }
}

/// The engine's end of one slot.
struct SlotEnd {
    rx: Consumer<SlotMsg>,
    tx: Producer<Back>,
    taken: Arc<AtomicU64>,
}

fn slot_channel() -> (SlotPort, SlotEnd) {
    let (msg_tx, msg_rx) = RingBuffer::new(MSG_CAPACITY);
    let (ret_tx, ret_rx) = RingBuffer::new(RETURN_CAPACITY);
    let taken = Arc::new(AtomicU64::new(0));
    (SlotPort { tx: msg_tx, rx: ret_rx, sent: 0, taken: taken.clone() }, SlotEnd { rx: msg_rx, tx: ret_tx, taken })
}

/// One place in a chain. Every seat's buffers are allocated with the rack; a seat holds a unit exactly
/// while it is one of its chain's first `len`.
struct Seat {
    id: UnitId,
    unit: Option<Unit>,
    /// Read once, at install.
    info: UnitInfo,
    /// Frames into the linear input (0) ↔ unit (`Rack::fade_frames`) crossfade, counted so both ends land
    /// on an exact frame; `engaged` is the end it heads for.
    fade: u32,
    engaged: bool,
    /// The unit leaves once the fade reaches its input.
    removing: bool,
    /// The unit's output over a render call, then the seat's (the crossfade applied in place).
    out: [Vec<f32>; 2],
}

/// An [`Event::Seat`] not yet taken by the event ring.
#[derive(Clone, Copy)]
struct Answer {
    frame: Frame,
    unit: UnitId,
    op: SeatOp,
    outcome: SeatOutcome,
}

struct Slot {
    end: SlotEnd,
    /// `MAX_UNITS` seats; the chain is the first `len`, head first.
    seats: Vec<Seat>,
    len: usize,
    /// The chain's latency as the record line reads it: its sum at the last install or removal that
    /// left it seating a unit (an emptied chain keeps it, so a source's delayed tail plays out once).
    line_latency: Frame,
    /// GO LIVE as asked: the live gate's target, and what the record latency follows.
    live: bool,
    /// The live gate ramps linearly from `live_gain` at frame `live_end - Rack::live_frames` to the
    /// target (1 live, 0 off), which it holds exactly from `live_end` on (`Frame::MIN`: settled).
    live_end: Frame,
    live_gain: f32,
    /// While not 0: the gate is shut, whatever `live` says, until the command taken at this count (an
    /// idle unload's live-off: [`Rack::shut`]) has applied.
    shut: u64,
    /// The gated input over a render call that a ramp touches.
    gated: [Vec<f32>; 2],
    /// A source chain's gated input fading out of the wet against its head's fade in.
    dry: [Vec<f32>; 2],
    gain_target: f64,
    gain: f64,
    /// The notes this slot's chain holds (bit per key).
    held: [u64; 2],
    events: Vec<SlotEvent>,
    /// A source chain's record path: its output, `delay - line_latency` frames late.
    line: Vec<Pair>,
    /// A slot's wet on its way to the record tap, `dw` frames late: the latched record latency less
    /// the chain's own, as [`Rack::latch_record`] last took them.
    wet_line: Vec<Pair>,
    dw: usize,
    write: usize,
    /// The input OTT on this slot's output, after its gain.
    ott: StereoOtt,
    /// Answers the event ring has not taken yet, oldest first.
    owed: Vec<Answer>,
}

/// The live gate at frame `f` heading for `live` (1 or 0), its ramp of `n` frames ending at `end` from
/// `from`: `from + (to - from) * u`, `u = (f - (end - n)) / n` from integer frames, so it lands on the
/// same frame at any block size; from `end` on, the target exactly.
fn live_gate(live: bool, end: Frame, from: f32, f: Frame, n: Frame) -> f32 {
    let to = if live { 1.0 } else { 0.0 };
    if f >= end {
        return to;
    }
    let u = (f - (end - n)).max(0) as f32 / n as f32;
    from + (to - from) * u
}

/// The crossfade `fade` frames of `n` from `from` (the seat's input) towards `to` (the unit): exactly
/// either at its ends.
fn crossfade(from: f32, to: f32, fade: u32, n: u32) -> f32 {
    if fade == n {
        to
    } else if fade == 0 {
        from
    } else {
        from + (fade as f32 / n as f32) * (to - from)
    }
}

/// One frame of a seat's fade: towards the unit while engaged, towards its input otherwise.
fn step(fade: &mut u32, engaged: bool, n: u32) {
    if engaged && *fade < n {
        *fade += 1;
    } else if !engaged && *fade > 0 {
        *fade -= 1;
    }
}

impl Slot {
    /// The chain's head is a source: it takes no input and goes where an instrument goes.
    fn source_head(&self) -> bool {
        self.len > 0 && self.seats[0].info.source
    }

    /// The chain's latency: its seats' sum, a leaving seat's included.
    fn latency(&self) -> Frame {
        self.seats[..self.len].iter().map(|s| s.info.latency).sum()
    }

    /// A seat takes notes: one is seated and not leaving.
    fn plays(&self) -> bool {
        self.seats[..self.len].iter().any(|s| !s.removing)
    }

    /// Hand a unit back on the return ring. Never full: a message is taken only while the ring has a
    /// place for every unit the slot holds and one more ([`Rack::service`]). Should that ever fail, the
    /// unit leaks, counted, rather than drop (a drop frees memory and calls into the plugin's DLL).
    fn give_back(&mut self, back: Back, errors: &mut u64) {
        if let Err(PushError::Full(back)) = self.end.tx.push(back) {
            *errors += 1;
            std::mem::forget(back);
        }
    }

    /// Whether the slot can take its next message now: the event ring's answer has a place among the
    /// owed, and an install a place on the return ring beside every unit the slot holds (so a refusal,
    /// a removal and an eviction always find one). A message that cannot waits in its ring.
    fn can_take(&self) -> bool {
        let Ok(msg) = self.end.rx.peek() else { return false };
        self.owed.len() < OWED_CAPACITY && (matches!(msg, SlotMsg::Remove { .. }) || self.end.tx.slots() > self.len)
    }

    fn queue(&mut self, kind: SlotEventKind, offset: u32, dropped: &mut u64) {
        if self.events.len() < MAX_SLOT_EVENTS {
            self.events.push(SlotEvent { offset, kind });
        } else {
            *dropped += 1;
        }
    }

    fn release_all(&mut self, offset: u32, dropped: &mut u64) {
        for key in 0..128u8 {
            if self.held[(key / 64) as usize] & (1 << (key % 64)) != 0 {
                self.queue(SlotEventKind::NoteOff { key }, offset, dropped);
            }
        }
        self.held = [0; 2];
    }

    /// Owe `answer` to the event ring: [`Slot::can_take`] reserved its place.
    fn answer(&mut self, answer: Answer, errors: &mut u64) {
        if self.owed.len() < OWED_CAPACITY {
            self.owed.push(answer);
        } else {
            *errors += 1;
        }
    }

    /// The seat at `k` leaves: its unit stopped while the seat still holds it, then handed back, and the
    /// seats after it close up. A `stop` that panics unwinds with the unit left in its seat, for the
    /// owner's eviction (or the engine's drop, off the audio thread).
    fn finish_removal(&mut self, k: usize, errors: &mut u64) {
        if let Some(unit) = self.seats[k].unit.as_mut() {
            unit.stop();
        }
        let seat = &mut self.seats[k];
        if let Some(unit) = seat.unit.take() {
            let id = seat.id;
            self.give_back((id, unit, None), errors);
        }
        let seat = &mut self.seats[k];
        seat.removing = false;
        seat.engaged = false;
        seat.fade = 0;
        self.seats[k..self.len].rotate_left(1);
        self.len -= 1;
        if self.len > 0 {
            self.line_latency = self.latency();
        } else {
            self.held = [0; 2];
            self.events.clear();
        }
    }

    /// An idle removal or an eviction: no block will carry the notes the units were just released from,
    /// so one silent frame from `frame` (the next the engine would render) does, to every seat leaving,
    /// and a restarted unit holds no note.
    fn flush(&mut self, cx: &UnitCx, zeros: &[f32]) {
        if self.events.is_empty() {
            return;
        }
        for seat in self.seats[..self.len].iter_mut().filter(|s| s.removing) {
            if let Some(unit) = seat.unit.as_mut() {
                let [l, r] = &mut seat.out;
                unit.process(cx, [&zeros[..1], &zeros[..1]], &self.events, [&mut l[..1], &mut r[..1]]);
            }
        }
        self.events.clear();
    }

    /// Every leaving seat, at once (no device runs): flushed, stopped, handed back.
    fn finish_idle(&mut self, cx: &UnitCx, zeros: &[f32], errors: &mut u64) {
        self.flush(cx, zeros);
        let mut k = 0;
        while k < self.len {
            if self.seats[k].removing {
                self.finish_removal(k, errors);
            } else {
                k += 1;
            }
        }
    }
}

/// The engine's clock the units read ([`UnitCx`]): set once per block.
#[derive(Clone, Copy)]
pub(crate) struct UnitClock {
    /// Device frames the DSP clock is behind.
    pub(crate) skipped: Frame,
    pub(crate) bpm: f64,
    pub(crate) meter: Meter,
}

/// The two slots, as the engine holds them.
pub(crate) struct Rack {
    slots: [Slot; SLOT_COUNT],
    zeros: Vec<f32>,
    fade_frames: u32,
    /// The live gate's ramp in frames: `LIVE_SECONDS` at the rate, at least 1.
    live_frames: Frame,
    /// The most a chain's latency may sum to ([`max_chain_latency`]).
    max_latency: Frame,
    /// A render call has run: from here a live toggle ramps (before it, it sets the starting state).
    rendered: bool,
    gain_coef: f64,
    /// The slot that takes the notes, if a slot is the note target.
    target: Option<usize>,
    /// The live latency the record tap is aligned on ([`Rack::latch_record`]).
    record_latency: Frame,
    /// The device frame the next render call starts at.
    cursor: Frame,
    clock: UnitClock,
    pub(crate) events_dropped: u64,
    pub(crate) protocol_errors: u64,
}

impl Rack {
    /// Allocates: build it off the audio thread. Every record line holds `max_input` (the device's
    /// input side at most: `EngineConfig::max_input_frames`) plus [`max_chain_latency`] plus a block.
    pub(crate) fn new(sample_rate: u32, max_block: usize, max_input: Frame) -> (Rack, [SlotPort; SLOT_COUNT]) {
        let max_latency = max_chain_latency(sample_rate);
        let line = (max_input.max(0) + max_latency) as usize + max_block;
        let block = max_block.max(1);
        let mut ports = Vec::with_capacity(SLOT_COUNT);
        let slots = std::array::from_fn(|_| {
            let (port, end) = slot_channel();
            ports.push(port);
            Slot {
                end,
                seats: (0..MAX_UNITS)
                    .map(|_| Seat {
                        id: 0,
                        unit: None,
                        info: UnitInfo { kind: UnitKind::Plugin, source: false, latency: 0 },
                        fade: 0,
                        engaged: false,
                        removing: false,
                        out: [vec![0.0; block], vec![0.0; block]],
                    })
                    .collect(),
                len: 0,
                line_latency: 0,
                live: false,
                live_end: Frame::MIN,
                live_gain: 0.0,
                shut: 0,
                gated: [vec![0.0; block], vec![0.0; block]],
                dry: [vec![0.0; block], vec![0.0; block]],
                gain_target: 1.0,
                gain: 1.0,
                held: [0; 2],
                events: Vec::with_capacity(MAX_SLOT_EVENTS),
                line: vec![[0.0; 2]; line],
                wet_line: vec![[0.0; 2]; line],
                dw: 0,
                write: 0,
                ott: StereoOtt::new(sample_rate),
                owed: Vec::with_capacity(OWED_CAPACITY),
            }
        });
        let ports: [SlotPort; SLOT_COUNT] = match ports.try_into() {
            Ok(ports) => ports,
            Err(_) => unreachable!(),
        };
        let rack = Rack {
            slots,
            zeros: vec![0.0; block],
            fade_frames: (FADE_SECONDS * sample_rate as f64).round().max(1.0) as u32,
            live_frames: (LIVE_SECONDS * sample_rate as f64).round().max(1.0) as Frame,
            max_latency,
            rendered: false,
            gain_coef: (-1.0 / (GAIN_TAU_SECONDS * sample_rate as f64)).exp(),
            target: None,
            record_latency: 0,
            cursor: 0,
            clock: UnitClock { skipped: 0, bpm: 120.0, meter: Meter::FOUR_FOUR },
            events_dropped: 0,
            protocol_errors: 0,
        };
        (rack, ports)
    }

    fn unit_cx(&self, frame: Frame) -> UnitCx {
        UnitCx { frame, dsp_frame: frame - self.clock.skipped, bpm: self.clock.bpm, meter: self.clock.meter }
    }

    /// Whether `id` is seated in either slot.
    fn seated(&self, id: UnitId) -> bool {
        self.slots.iter().any(|s| s.seats[..s.len].iter().any(|seat| seat.id == id))
    }

    /// Apply the ports' messages at `frame`, a block start (or, idle, the frame the next block would
    /// start at), each answered. `engaged` installs without a fade (nothing has sounded yet, or no device
    /// runs).
    fn service(&mut self, frame: Frame, engaged: bool, fade: bool) {
        let cx = self.unit_cx(frame);
        for i in 0..SLOT_COUNT {
            // Each message's answer and return have their places before it leaves its ring: nothing it
            // owes is ever dropped, and one that has no place yet waits there, in order.
            while self.slots[i].can_take() {
                let Ok(msg) = self.slots[i].end.rx.pop() else { break };
                let (unit, op, outcome) = match msg {
                    SlotMsg::Install { id, unit } => (id, SeatOp::Install, self.install(i, id, unit, engaged)),
                    SlotMsg::Remove { id } => (id, SeatOp::Remove, self.remove(i, id, fade)),
                };
                let errors = &mut self.protocol_errors;
                self.slots[i].answer(Answer { frame, unit, op, outcome }, errors);
                self.slots[i].end.taken.fetch_add(1, Release);
            }
            if !fade {
                let (s, zeros, errors) = (&mut self.slots[i], &self.zeros, &mut self.protocol_errors);
                s.finish_idle(&cx, zeros, errors);
            }
        }
    }

    /// Seat `unit` at slot `i`'s end, or hand it straight back, refused (never started or stopped).
    fn install(&mut self, i: usize, id: UnitId, unit: Unit, engaged: bool) -> SeatOutcome {
        let refused = if id == 0 || self.seated(id) {
            Some(SeatRefusal::Id)
        } else if self.slots[i].len == MAX_UNITS {
            Some(SeatRefusal::Full)
        } else {
            None
        };
        let (fade_frames, max_latency) = (self.fade_frames, self.max_latency);
        let s = &mut self.slots[i];
        if let Some(why) = refused {
            s.give_back((id, unit, Some(why)), &mut self.protocol_errors);
            return SeatOutcome::Refused(why);
        }
        // The chain seats the unit before any of its methods runs: one that panics unwinds with the unit
        // in its seat (for the owner's eviction), never dropped as a local.
        let (k, before) = (s.len, s.latency());
        let seat = &mut s.seats[k];
        (seat.id, seat.removing, seat.engaged, seat.fade) = (id, false, true, if engaged { fade_frames } else { 0 });
        seat.info = UnitInfo { kind: UnitKind::Plugin, source: false, latency: 0 };
        seat.unit = Some(unit);
        s.len += 1;
        let info = s.seats[k].unit.as_ref().map(|u| u.info()).unwrap_or(s.seats[k].info);
        let info = UnitInfo { latency: info.latency.max(0), ..info };
        let refused = if info.source && k > 0 {
            Some(SeatRefusal::Placement)
        } else if info.latency > max_latency - before {
            Some(SeatRefusal::Latency)
        } else {
            None
        };
        if let Some(why) = refused {
            s.len -= 1;
            if let Some(unit) = s.seats[k].unit.take() {
                s.give_back((id, unit, Some(why)), &mut self.protocol_errors);
            }
            return SeatOutcome::Refused(why);
        }
        if k == 0 {
            // A chain that held nothing holds no note.
            s.held = [0; 2];
            s.events.clear();
        }
        s.seats[k].info = info;
        s.line_latency = s.latency();
        SeatOutcome::Applied
    }

    /// Start unit `id`'s removal from slot `i`: the slot's notes released, its seat fading to its input
    /// (at once with `fade` off: the caller finishes it).
    fn remove(&mut self, i: usize, id: UnitId, fade: bool) -> SeatOutcome {
        let s = &mut self.slots[i];
        let Some(k) = s.seats[..s.len].iter().position(|seat| seat.id == id && !seat.removing) else {
            return SeatOutcome::Refused(SeatRefusal::Unknown);
        };
        s.release_all(0, &mut self.events_dropped);
        let seat = &mut s.seats[k];
        seat.removing = true;
        seat.engaged = false;
        if !fade {
            seat.fade = 0;
        }
        SeatOutcome::Applied
    }

    /// The answers the event ring has not taken, offered in order until it refuses one.
    pub(crate) fn offer(&mut self, feed: &mut Feed) {
        for (i, s) in self.slots.iter_mut().enumerate() {
            let taken = s.owed.iter().take_while(|a| feed.push(Event::Seat { frame: a.frame, slot: i as u8, unit: a.unit, op: a.op, outcome: a.outcome })).count();
            s.owed.drain(..taken);
        }
    }

    /// At a block start: service the ports and start the render cursor.
    pub(crate) fn begin_block(&mut self, start: Frame, first: bool, clock: UnitClock) {
        self.clock = clock;
        self.service(start, first, true);
        self.cursor = start;
    }

    /// With no device running (the host holds the engine): apply the ports' messages at once, at `frame`
    /// (the next the engine would render), no fades. A removed unit stops here and goes straight back.
    pub(crate) fn service_idle(&mut self, frame: Frame) {
        self.cursor = frame;
        self.service(frame, true, false);
    }

    /// Stop every unit and hand it back (a sample-rate change: the host re-activates and reinstalls),
    /// with every install still waiting on a port that the return ring has room for: the host reads its
    /// returns and evicts again until none comes back, so nothing is left for the engine's drop.
    pub(crate) fn evict(&mut self, frame: Frame) {
        self.service_idle(frame);
        let cx = self.unit_cx(frame);
        for s in self.slots.iter_mut() {
            if s.len > 0 {
                s.release_all(0, &mut self.events_dropped);
                for seat in s.seats[..s.len].iter_mut() {
                    seat.removing = true;
                    seat.engaged = false;
                    seat.fade = 0;
                }
                s.finish_idle(&cx, &self.zeros, &mut self.protocol_errors);
            }
        }
    }

    /// The largest latency of a live input-fed chain now: what [`Rack::latch_record`] takes while open.
    pub(crate) fn live_latency(&self) -> Frame {
        self.slots.iter().filter(|s| s.live && s.shut == 0 && s.len > 0 && !s.source_head()).map(Slot::latency).max().unwrap_or(0)
    }

    /// At a block start, after the ports were serviced: while `open`, the alignment follows the slots
    /// (the live latency); shut, it holds what it last took. The engine shuts it while a capture's
    /// alignment is fixed, which took the same live latency. Either way each slot's wet is delayed by
    /// what its own chain's latency lacks of that alignment, so a unit that comes or goes mid-take
    /// moves nothing the take records outside its own crossfade. A chain that grows past the latched
    /// alignment mid-take records late by the excess (E2 holds that edit until the take ends:
    /// `docs/unit-contract.md` §7).
    pub(crate) fn latch_record(&mut self, open: bool) {
        if open {
            self.record_latency = self.live_latency();
        }
        for s in self.slots.iter_mut() {
            let own = if s.len > 0 && !s.source_head() { s.latency() } else { 0 };
            s.dw = (self.record_latency - own).clamp(0, (s.wet_line.len() - 1) as Frame) as usize;
        }
    }

    /// The live latency the record tap is aligned on, as last latched.
    pub(crate) fn record_latency(&self) -> Frame {
        self.record_latency
    }

    /// Slot `slot`'s chain: its role and its latency, `None` while it seats nothing.
    pub(crate) fn installed(&self, slot: usize) -> Option<(SlotKind, Frame)> {
        let s = self.slots.get(slot).filter(|s| s.len > 0)?;
        Some((if s.source_head() { SlotKind::Instrument } else { SlotKind::Effect }, s.latency()))
    }

    /// Slot `slot`'s chain, head first: each seat's id and what it is.
    pub(crate) fn chain(&self, slot: usize) -> impl Iterator<Item = (UnitId, UnitInfo)> + '_ {
        self.slots.get(slot).into_iter().flat_map(|s| s.seats[..s.len].iter().map(|seat| (seat.id, seat.info)))
    }

    fn offset(&self, now: Frame) -> u32 {
        (now - self.cursor).max(0) as u32
    }

    /// The note target moves (`Command::SelectInstrument`): the slot it leaves releases its notes, and
    /// so does the one it lands on when it stays.
    pub(crate) fn select(&mut self, target: Option<usize>, now: Frame) {
        let offset = self.offset(now);
        for i in [self.target, target].into_iter().flatten() {
            if let Some(s) = self.slots.get_mut(i) {
                s.release_all(offset, &mut self.events_dropped);
            }
        }
        self.target = target.filter(|&i| i < SLOT_COUNT);
    }

    pub(crate) fn note_on(&mut self, key: u8, velocity: f32, now: Frame) {
        let offset = self.offset(now);
        let Some(s) = self.target.map(|i| &mut self.slots[i]) else { return };
        if key > 127 || !s.plays() {
            return;
        }
        let velocity = if velocity.is_finite() { velocity.clamp(0.0, 1.0) } else { 0.0 };
        s.queue(SlotEventKind::NoteOn { key, velocity }, offset, &mut self.events_dropped);
        s.held[(key / 64) as usize] |= 1 << (key % 64);
    }

    pub(crate) fn note_off(&mut self, key: u8, now: Frame) {
        let offset = self.offset(now);
        let Some(s) = self.target.map(|i| &mut self.slots[i]) else { return };
        if key > 127 || s.held[(key / 64) as usize] & (1 << (key % 64)) == 0 {
            return;
        }
        s.held[(key / 64) as usize] &= !(1 << (key % 64));
        s.queue(SlotEventKind::NoteOff { key }, offset, &mut self.events_dropped);
    }

    pub(crate) fn all_notes_off(&mut self, now: Frame) {
        let offset = self.offset(now);
        if let Some(s) = self.target.map(|i| &mut self.slots[i]) {
            s.release_all(offset, &mut self.events_dropped);
        }
    }

    /// Whether slot `slot` is live as asked (the gate's target); false for a slot past the rack.
    pub(crate) fn live(&self, slot: usize) -> bool {
        self.slots.get(slot).is_some_and(|s| s.live)
    }

    /// Whether slot `slot`'s live gate passes input from the cursor on: live, or its ramp out not yet
    /// ended.
    pub(crate) fn gate_open(&self, slot: usize) -> bool {
        self.slots.get(slot).is_some_and(|s| s.shut == 0 && (s.live || self.cursor < s.live_end))
    }

    /// GO LIVE on or off at frame `now`: the live gate ramps from where it is to the new target over
    /// `live_frames` from `now` (a reversal mid-ramp from the gain it reached). The same target again
    /// changes nothing. Before the first rendered frame it sets the starting state, settled.
    pub(crate) fn set_live(&mut self, slot: usize, live: bool, now: Frame) {
        let (n, rendered) = (self.live_frames, self.rendered);
        let Some(s) = self.slots.get_mut(slot) else { return };
        if s.live == live {
            return;
        }
        let from = live_gate(s.live, s.live_end, s.live_gain, now, n);
        s.live = live;
        let to = if live { 1.0 } else { 0.0 };
        (s.live_end, s.live_gain) = if !rendered || from == to { (Frame::MIN, to) } else { (now + n, from) };
    }

    /// Shut slot `slot`'s gate at once (no ramp) until the command taken at count `live_off`, a live-off,
    /// has applied ([`Rack::reopen`]): `Engine::close_slot_idle`. The live flag still follows every
    /// command meanwhile.
    pub(crate) fn shut(&mut self, slot: usize, live_off: u64) {
        if let Some(s) = self.slots.get_mut(slot) {
            s.shut = live_off;
        }
    }

    /// After the command taken at count `seq` has applied: a gate shut for it opens to its live flag,
    /// settled (the live-off it waited for leaves it closed, with no ramp from the gain it had before).
    pub(crate) fn reopen(&mut self, seq: u64) {
        for slot in 0..self.slots.len() {
            if self.slots[slot].shut == seq {
                self.slots[slot].shut = 0;
                self.settle_live(slot);
            }
        }
    }

    /// Slot `slot`'s live gate at its target at once, a ramp cut short.
    pub(crate) fn settle_live(&mut self, slot: usize) {
        if let Some(s) = self.slots.get_mut(slot) {
            (s.live_end, s.live_gain) = (Frame::MIN, if s.live { 1.0 } else { 0.0 });
        }
    }

    pub(crate) fn set_gain(&mut self, slot: usize, gain: f32) {
        if let Some(s) = self.slots.get_mut(slot) {
            s.gain_target = if gain.is_finite() { gain.max(0.0) as f64 } else { 0.0 };
        }
    }

    /// The input OTT's setting, on every slot's instance from the next frame rendered.
    pub(crate) fn set_ott(&mut self, setting: Setting) {
        for s in self.slots.iter_mut() {
            s.ott.set(setting);
        }
    }

    /// Whether the input OTT is on, as last set.
    pub(crate) fn ott_on(&self) -> bool {
        self.slots[0].ott.on()
    }

    /// Render both slots over `m` frames from the cursor, slot `s` reading the pair `inputs[s]`: `wet`,
    /// `aligned` and `bus` are overwritten (the input-fed chains' outputs as heard and at the take's
    /// alignment, and the source chains'), `record` gets `aligned` plus the source chains' record path,
    /// `delay` frames late less each one's latency (`delay` = the input side plus the latched record
    /// latency, as the built-in instruments' record path).
    pub(crate) fn render(&mut self, inputs: [[&[f32]; 2]; SLOT_COUNT], wet: [&mut [f32]; 2], aligned: [&mut [f32]; 2], bus: [&mut [f32]; 2], record: &mut [Pair], delay: Frame) {
        let m = record.len();
        let [wet_l, wet_r] = wet;
        let [al_l, al_r] = aligned;
        let [bus_l, bus_r] = bus;
        for x in [&mut *wet_l, &mut *wet_r, &mut *al_l, &mut *al_r, &mut *bus_l, &mut *bus_r] {
            x[..m].fill(0.0);
        }
        record.fill([0.0; 2]);
        let (frame, fade_frames, n) = (self.cursor, self.fade_frames, self.live_frames);
        let cx = self.unit_cx(frame);
        let zeros = &self.zeros[..m];
        self.rendered |= m > 0;
        for (s, input) in self.slots.iter_mut().zip(inputs) {
            let input = [&input[0][..m], &input[1][..m]];
            // The live gate: settled, the input itself or silence; mid-ramp, the input times the gate,
            // frame by frame, into `gated`. It feeds an input-fed chain and an empty slot alike, and it
            // is the dry side of the first seat's crossfade; a unit's output is never gated, so its tail
            // rings.
            let (live, end, from) = if s.shut == 0 { (s.live, s.live_end, s.live_gain) } else { (false, Frame::MIN, 0.0) };
            if frame < end {
                for (c, x) in input.iter().enumerate() {
                    for (k, (g, &x)) in s.gated[c].iter_mut().zip(x.iter()).enumerate() {
                        let f = frame + k as Frame;
                        *g = if f < end {
                            x * live_gate(live, end, from, f, n)
                        } else if live {
                            x
                        } else {
                            0.0
                        };
                    }
                }
            }
            let gated: [&[f32]; 2] = if frame < end {
                [&s.gated[0][..m], &s.gated[1][..m]]
            } else if live {
                input
            } else {
                [zeros, zeros]
            };
            // An event stamped past this call (never, while slot commands bound the ranges) plays late
            // on its last frame rather than breaking the offset contract.
            for e in s.events.iter_mut() {
                e.offset = e.offset.min(m.saturating_sub(1) as u32);
            }
            // A source head routes the chain to the bus for the frames before its removal completes;
            // from there the chain is input-fed (an empty slot passes what an empty slot does).
            let source = s.source_head();
            let mut to_bus = if source { m } else { 0 };
            for k in 0..s.len {
                let (before, rest) = s.seats.split_at_mut(k);
                let seat = &mut rest[0];
                let seat_in: [&[f32]; 2] = if k == 0 { gated } else { [&before[k - 1].out[0][..m], &before[k - 1].out[1][..m]] };
                if let Some(unit) = seat.unit.as_mut() {
                    // A source takes no input.
                    let x = if seat.info.source { [zeros, zeros] } else { seat_in };
                    let [ol, or] = &mut seat.out;
                    unit.process(&cx, x, &s.events, [&mut ol[..m], &mut or[..m]]);
                    // A non-finite sample would stick in every filter, echo, limiter and loop after it:
                    // the whole call's output goes silent, counted.
                    if !ol[..m].iter().chain(&or[..m]).all(|y| y.is_finite()) {
                        ol[..m].fill(0.0);
                        or[..m].fill(0.0);
                        self.protocol_errors += 1;
                    }
                }
                let [ol, or] = &mut seat.out;
                if seat.info.source {
                    // A source and its slot's input go different ways (the bus, the wet), so each fades
                    // against silence there; once gone, the seat passes the gated input.
                    let [dl, dr] = &mut s.dry;
                    for j in 0..m {
                        let gone = seat.removing && seat.fade == 0;
                        if gone {
                            to_bus = to_bus.min(j);
                            (ol[j], or[j]) = (gated[0][j], gated[1][j]);
                        } else {
                            ol[j] = crossfade(0.0, ol[j], seat.fade, fade_frames);
                            or[j] = crossfade(0.0, or[j], seat.fade, fade_frames);
                            dl[j] = crossfade(gated[0][j], 0.0, seat.fade, fade_frames);
                            dr[j] = crossfade(gated[1][j], 0.0, seat.fade, fade_frames);
                        }
                        step(&mut seat.fade, seat.engaged, fade_frames);
                    }
                } else {
                    for j in 0..m {
                        ol[j] = crossfade(seat_in[0][j], ol[j], seat.fade, fade_frames);
                        or[j] = crossfade(seat_in[1][j], or[j], seat.fade, fade_frames);
                        step(&mut seat.fade, seat.engaged, fade_frames);
                    }
                }
            }
            s.events.clear();
            let len = s.line.len();
            let (d, dw) = ((delay - s.line_latency).clamp(0, (len - 1) as Frame) as usize, s.dw);
            for j in 0..m {
                let y = match s.len {
                    0 => [gated[0][j], gated[1][j]],
                    k => [s.seats[k - 1].out[0][j], s.seats[k - 1].out[1][j]],
                };
                let dry = if j < to_bus { [s.dry[0][j], s.dry[1][j]] } else { [0.0; 2] };
                s.gain = crate::glide(s.gain, s.gain_target, self.gain_coef);
                let g = s.gain;
                let (y, dry) = if g == 1.0 { (y, dry) } else { (y.map(|v| (g * v as f64) as f32), dry.map(|v| (g * v as f64) as f32)) };
                // The input OTT, on the output before it splits: heard and recorded the same.
                let y = s.ott.frame(y);
                let (heard, onto_bus) = if j < to_bus { (dry, y) } else { (y, [0.0; 2]) };
                bus_l[j] += onto_bus[0];
                bus_r[j] += onto_bus[1];
                wet_l[j] += heard[0];
                wet_r[j] += heard[1];
                s.line[s.write] = onto_bus;
                s.wet_line[s.write] = heard;
                let r = s.line[(s.write + len - d) % len];
                let w = s.wet_line[(s.write + len - dw) % len];
                record[j][0] += r[0];
                record[j][1] += r[1];
                al_l[j] += w[0];
                al_r[j] += w[1];
                s.write = (s.write + 1) % len;
            }
            let mut k = 0;
            while k < s.len {
                if s.seats[k].removing && s.seats[k].fade == 0 {
                    s.finish_removal(k, &mut self.protocol_errors);
                } else {
                    k += 1;
                }
            }
        }
        for (r, (&l, &rr)) in record.iter_mut().zip(al_l.iter().zip(al_r.iter())) {
            r[0] += l;
            r[1] += rr;
        }
        self.cursor += m as Frame;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source that renders silence.
    struct Sink;

    impl UnitProcessor for Sink {
        fn info(&self) -> UnitInfo {
            UnitInfo { kind: UnitKind::Plugin, source: true, latency: 0 }
        }

        fn process(&mut self, _cx: &UnitCx, _input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
            for o in out {
                o.fill(0.0);
            }
        }

        fn stop(&mut self) {}

        fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
            self
        }
    }

    /// The engine's command table (64 a frame) keeps a slot under the queue's bound; past it, a note is
    /// dropped and counted rather than allocating.
    #[test]
    fn notes_past_the_event_queue_are_dropped_and_counted() {
        let (mut rack, mut ports) = Rack::new(48000, 64, 0);
        assert!(ports[0].install(1, Box::new(Sink)).is_ok());
        rack.service_idle(0);
        rack.select(Some(0), 0);
        for k in 0..MAX_SLOT_EVENTS + 3 {
            rack.note_on((k % 128) as u8, 1.0, 0);
        }
        assert_eq!(rack.events_dropped, 3);
        assert_eq!(rack.slots[0].events.len(), MAX_SLOT_EVENTS);
    }
}
