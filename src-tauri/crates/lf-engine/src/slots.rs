//! OWNS: the slots' chains inside the callback: the units the host installs, moves, withdraws and takes
//! back through each slot's [`SlotPort`] (each under its [`UnitId`], each message answered once by an
//! [`Event::Seat`], each applied chain sent as an [`Event::Chain`]), their crossfades, their bypass, the
//! notes a slot holds, the live flag and gain, and where a slot's output goes. The processors themselves
//! are the host's (`src-tauri/src/host`, which implements [`UnitProcessor`] for CLAP, VST3 and VST2) or a
//! test's.
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
//! ([`Rack::latch_record`]): it holds while a capture runs, so a slot going live or not never moves
//! what records mid-take (a slot off live feeds its line silence, and what was on the way drains in
//! place). A chain headed by a source takes no input and plays the notes while its slot is the note
//! target; its output joins the master bus and goes to the record tap delayed like a built-in
//! instrument's, less the chain's own latency (`instruments`). An install that would sum a chain past
//! [`max_chain_latency`] is refused, and every line holds the device's input side plus that plus a
//! block ([`Rack::new`]), so no delay is ever clipped.
//!
//! **A seat is two crossfades deep** (`docs/unit-contract.md` §3), each counted in the frames of
//! `FADE_SECONDS` so both ends land on an exact frame at any block size: its lifecycle (the undelayed
//! input ↔ what the seat sounds) and inside it its bypass (the input delayed by the unit's latency, on
//! the seat's own line, ↔ the unit's output; a source's bypass is silence). Every seat's bypass line
//! holds [`max_chain_latency`] frames, allocated with the rack, so no line crosses the port and none has
//! to come back. A **bypass** (`Command::SetUnitBypass`, `Action::ToggleUnit`) lands on its command's
//! frame through [`UnitProcessor::set_bypass`]: a unit that bypasses itself gets it and the seat passes
//! its output on; any other is crossfaded to its delayed input, still called (it comes back warm). The
//! chain's latency never changes with it. An id no seat holds is dropped and counted.
//!
//! **Indices are the chain's as the host sees it** ([`Event::Chain`]): a seat whose removal is fading
//! counts nowhere, a withdrawn seat keeps its place. An **install** goes at its index, a source at the
//! head whatever its index; it fades in from its input (a source from silence). A source where a source
//! is seated is refused unless the install names it in `replaces`, and an install that `replaces` a
//! seated unit (a source, or a reload under its own id) takes its seat in one crossfade from the old
//! unit's output to the new one's, which stops and goes back once it ends. No effect goes ahead of a
//! source; a ninth unit is refused. A **removal** releases the notes the slot holds, fades the seat to
//! its undelayed input, stops the unit once after its last call and hands it back; the slot stays live.
//! A **move** dips the chain's output to silence over the fade's frames, reorders the chain on the frame
//! the dip ends and brings it back over as many: every unit is called for every frame once, the call
//! split on that frame, so a tail rings on through its new successors. A **withdrawal** (a restart, an
//! owner's cycle: invariant 4, unit-scoped) fades the seat to its bypass, stops the unit and hands it
//! back with its bypass; the empty seat keeps its place, role, latency and line and passes its input
//! delayed by that latency (a source's, silence) until an install under its id reseats it, faded in from
//! its bypass, or the player removes it. A reseat the seat cannot take (a mid-chain effect come back a
//! source) is refused and leaves it withdrawn: parked.
//!
//! **An edit that moves the alignment waits for the take** (`docs/unit-contract.md` §7): an install, a
//! removal, a replacement or a reseat that changes its chain's latency sum or its role (input-fed ↔
//! source, a zero-latency source included) is judged at a block start, and while a capture holds the
//! alignment it stays in its port, the slot's later messages behind it, answered `Waiting` once; it
//! applies at the first block start after, unanswered again (its [`Event::Chain`] shows it). A move, a
//! bypass, a withdrawal and an edit that changes neither apply at once. So the latched alignment and
//! every chain's own sum hold for a whole capture, and the alignment changes on an edit's own frame: a
//! leaving seat's latency counts nowhere from its removal on, and only its own crossfade records off it.
//!
//! Each slot runs its own instance of the input OTT (`dsp::ott`, stereo, its detectors linked) on its
//! output, after the slot's gain and before that output splits into the wet signal, the bus and the
//! record lines: an effect chain's, a source chain's and an empty live slot's dry input alike, so what
//! is heard and what is recorded are the same processed signal. A source chain's input fading out of
//! the wet over its head's install or removal does not pass it. Every instance always runs (off and
//! settled, bit-transparent) and takes the engine's one OTT setting on the same frame
//! ([`Rack::set_ott`]).
//!
//! Lifecycle never stops the audio, and nothing here drops a unit. A message leaves its ring only once
//! its answer has a place among those the slot owes the event ring and, an install, once the return
//! ring has a place for it beside every unit the slot holds: a full ring keeps the message waiting, in
//! order, and nothing owed is ever dropped. A slot takes no message while a move's reorder, a
//! replacement's crossfade, a withdrawal or a leaving source is under way (at most the fade's frames).
//! With no device running the ports are serviced at once, without fades, and one silent frame of
//! `process` carries a leaving unit's release. An eviction hands back every seat with its bypass and
//! every install its ring has room for (the host reads its returns and evicts again for the rest); a
//! refused install goes straight back with its reason, never started or stopped. A unit's methods run
//! only while the slot holds it (a seat, or the message taken from its ring), so a panic the
//! callback's guard catches unwinds past no unit held as a local. A call whose output holds a NaN or an
//! infinity is silenced, counted, so nothing downstream (the mix, the input sends, the limiter, a
//! capture) ever sees it.
//!
//! The engine renders the slots once per range, not per chunk: from where they stopped up to the next
//! frame a slot command waits for (a note, the target, a slot's live flag or gain, the OTT's switch or
//! a control, a unit's bypass), so a plugin sees one call per device block unless a stamped slot
//! command or a move's reorder splits it. Those commands therefore apply at the start of a render call
//! (every event offset is 0 in practice); a port message applies at the next block start. A live
//! ramp's end splits nothing: the gate is evaluated per frame.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Release}};
use std::sync::Arc;

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::api::{max_chain_latency, ChainSeat, Event, Pair, SeatOp, SeatOutcome, SeatRefusal, SlotEvent, SlotEventKind, SlotKind, UnitCx, UnitId, UnitInfo, UnitKind, UnitProcessor, MAX_UNITS, SLOT_COUNT};
use crate::dsp::ott::{Setting, StereoOtt};
use crate::engine::Feed;
use crate::grid::{Frame, Meter};

/// The crossfades into and out of a seat, its bypass's and a move's dip: 10 ms, the web monitor's
/// declick (`audio_output.rs`).
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
    Install { id: UnitId, at: usize, bypassed: bool, replaces: Option<UnitId>, unit: Unit },
    Remove { id: UnitId },
    Withdraw { id: UnitId },
    Move { id: UnitId, to: usize },
}

impl SlotMsg {
    fn id(&self) -> UnitId {
        match *self {
            SlotMsg::Install { id, .. } | SlotMsg::Remove { id } | SlotMsg::Withdraw { id } | SlotMsg::Move { id, .. } => id,
        }
    }

    fn op(&self) -> SeatOp {
        match self {
            SlotMsg::Install { .. } => SeatOp::Install,
            SlotMsg::Remove { .. } => SeatOp::Remove,
            SlotMsg::Withdraw { .. } => SeatOp::Withdraw,
            SlotMsg::Move { .. } => SeatOp::Move,
        }
    }
}

/// A unit on its way back to the host: removed, withdrawn, replaced or evicted with the bypass its
/// seat applied, or refused at its install (with the bypass it asked for).
struct Back {
    id: UnitId,
    unit: Unit,
    refused: Option<SeatRefusal>,
    bypassed: bool,
}

/// A unit the engine handed back ([`SlotPort::returned_unit`]).
pub struct Returned {
    pub id: UnitId,
    pub unit: Box<dyn UnitProcessor>,
    /// The bypass its seat applied when it left (a refused install: the one it asked for).
    pub bypassed: bool,
    /// Why its install was refused; `None` for a unit that was seated.
    pub refused: Option<SeatRefusal>,
}

/// The host's end of one slot (`EngineHandle::slots`). Not real-time: the host blocks on it, the engine
/// never does.
pub struct SlotPort {
    tx: Producer<SlotMsg>,
    rx: Consumer<Back>,
    /// Messages pushed, and those the engine has answered (stored after it applied each, handed back
    /// what it refused, or held it waiting).
    sent: u64,
    taken: Arc<AtomicU64>,
    holding: Arc<AtomicBool>,
}

/// One message on a slot's port ([`SlotPort::install`]): whether the engine has answered it yet.
#[derive(Clone)]
pub struct Ticket {
    seq: u64,
    taken: Arc<AtomicU64>,
}

impl Ticket {
    /// The engine has answered the message: applied (a seated install is in its chain), refused (the
    /// unit is on the port's return ring, [`SlotPort::refused`]) or held waiting for a take to end
    /// ([`SlotPort::holding`]; it applies after, unanswered again).
    pub fn answered(&self) -> bool {
        self.taken.load(Acquire) >= self.seq
    }

    /// Whether `port` is the one this message went to (a rebuild replaces the ports with the engine).
    pub fn of(&self, port: &SlotPort) -> bool {
        Arc::ptr_eq(&self.taken, &port.taken)
    }
}

impl SlotPort {
    /// Hand an activated unit to the slot under `id`, to seat at its chain's end, not bypassed:
    /// [`SlotPort::install_at`] with `at` past the end.
    pub fn install(&mut self, id: UnitId, unit: Unit) -> Result<Ticket, Unit> {
        self.install_at(id, MAX_UNITS, false, None, unit)
    }

    /// Hand an activated unit to the slot under `id`, to seat at index `at` of its chain (clamped to its
    /// end; a source goes to the head), `bypassed` or not; `replaces`, a seated unit whose seat it
    /// takes in one crossfade (a source replaced, or a reload under its own id). An `id` the slot holds
    /// withdrawn reseats that seat. It applies at the next block start, or waits for a take to end
    /// (`docs/unit-contract.md` §7), and is answered once by an [`Event::Seat`] (a refused one comes
    /// back on [`SlotPort::returned`], its reason on [`SlotPort::refused`]). The ticket tells when the
    /// engine has answered it. Gives the unit back when the message ring is full (the engine is not
    /// rendering and has not been serviced, or the host has not read its returns).
    pub fn install_at(&mut self, id: UnitId, at: usize, bypassed: bool, replaces: Option<UnitId>, unit: Unit) -> Result<Ticket, Unit> {
        self.tx.push(SlotMsg::Install { id, at, bypassed, replaces, unit }).map_err(|PushError::Full(msg)| match msg {
            SlotMsg::Install { unit, .. } => unit,
            _ => unreachable!(),
        })?;
        self.sent += 1;
        Ok(Ticket { seq: self.sent, taken: self.taken.clone() })
    }

    /// Ask for unit `id` back: the slot releases its notes, the seat crossfades to its input, the unit
    /// stops and arrives on [`SlotPort::returned`]. Answered by an [`Event::Seat`]; nothing comes back
    /// for an id the slot does not seat (nor for a withdrawn seat, whose unit is back already). False
    /// when the ring is full.
    pub fn remove(&mut self, id: UnitId) -> bool {
        self.push(SlotMsg::Remove { id })
    }

    /// Withdraw unit `id` (a restart, an owner's cycle): its seat fades to its bypass, the unit stops
    /// and arrives on [`SlotPort::returned`]; the seat keeps its place, role and latency until an
    /// install under `id` reseats it or a removal takes it. False when the ring is full.
    pub fn withdraw(&mut self, id: UnitId) -> bool {
        self.push(SlotMsg::Withdraw { id })
    }

    /// Move unit `id` to index `to` of its chain (clamped to its end): the chain's output dips to
    /// silence and back around the reorder. False when the ring is full.
    pub fn move_to(&mut self, id: UnitId, to: usize) -> bool {
        self.push(SlotMsg::Move { id, to })
    }

    fn push(&mut self, msg: SlotMsg) -> bool {
        let pushed = self.tx.push(msg).is_ok();
        self.sent += pushed as u64;
        pushed
    }

    /// A message waits for a take to end (answered `Waiting`), the slot's later messages behind it.
    pub fn holding(&self) -> bool {
        self.holding.load(Acquire)
    }

    /// A unit the engine handed back, with its id: removed, withdrawn, replaced, evicted
    /// ([`crate::Engine::evict_slots`]) or refused.
    pub fn returned(&mut self) -> Option<(UnitId, Unit)> {
        self.rx.pop().ok().map(|back| (back.id, back.unit))
    }

    /// [`SlotPort::returned`] with its bypass and, a refused install, why.
    pub fn returned_unit(&mut self) -> Option<Returned> {
        self.rx.pop().ok().map(|b| Returned { id: b.id, unit: b.unit, bypassed: b.bypassed, refused: b.refused })
    }

    /// Unit `id` with why the engine refused its install, if that is what the port hands back next.
    pub fn refused(&mut self, id: UnitId) -> Option<(Unit, SeatRefusal)> {
        if !self.rx.peek().is_ok_and(|b| b.id == id && b.refused.is_some()) {
            return None;
        }
        let back = self.rx.pop().ok()?;
        back.refused.map(|why| (back.unit, why))
    }
}

/// The engine's end of one slot.
struct SlotEnd {
    rx: Consumer<SlotMsg>,
    tx: Producer<Back>,
    taken: Arc<AtomicU64>,
    holding: Arc<AtomicBool>,
}

fn slot_channel() -> (SlotPort, SlotEnd) {
    let (msg_tx, msg_rx) = RingBuffer::new(MSG_CAPACITY);
    let (ret_tx, ret_rx) = RingBuffer::new(RETURN_CAPACITY);
    let taken = Arc::new(AtomicU64::new(0));
    let holding = Arc::new(AtomicBool::new(false));
    (
        SlotPort { tx: msg_tx, rx: ret_rx, sent: 0, taken: taken.clone(), holding: holding.clone() },
        SlotEnd { rx: msg_rx, tx: ret_tx, taken, holding },
    )
}

/// One place in a chain. Every seat's buffers are allocated with the rack; a seat is one of its chain's
/// first `len` exactly while it is in use (holding a unit, withdrawn, or leaving).
struct Seat {
    id: UnitId,
    unit: Option<Unit>,
    /// Read once, at install; a withdrawn seat keeps its unit's.
    info: UnitInfo,
    /// The lifecycle crossfade: frames into the linear undelayed input (0) ↔ seat (`Rack::fade_frames`),
    /// counted so both ends land on an exact frame; `engaged` is the end it heads for.
    fade: u32,
    engaged: bool,
    /// The seat leaves once `fade` reaches its input; it counts in no index or sum meanwhile.
    removing: bool,
    /// The bypass crossfade: frames into the delayed input (0) ↔ the unit's output (n); `wet_on` is the
    /// end it heads for (off while the seat bypasses a unit that does not bypass itself, or has none).
    wet: u32,
    wet_on: bool,
    /// The bypass as applied: what [`Event::Chain`] shows and the unit goes back with.
    bypassed: bool,
    /// The unit leaves once `wet` reaches the delayed input; the seat stays.
    withdrawing: bool,
    /// A replaced unit (its id and its bypass), crossfaded out over `swap` (0 old ↔ n new) and handed
    /// back once it ends.
    outgoing: Option<(UnitId, Unit, bool)>,
    swap: u32,
    /// The unit's output over a render call, then the seat's (the crossfades applied in place).
    out: [Vec<f32>; 2],
    /// The outgoing unit's output over a render call.
    old: [Vec<f32>; 2],
    /// The seat's input, its last [`max_chain_latency`] frames (an effect's bypass reads it at its
    /// latency): written at `line_at`, `filled` frames of it valid.
    line: Vec<Pair>,
    line_at: usize,
    filled: usize,
}

impl Seat {
    /// The unit it holds now, an outgoing one included.
    fn units(&self) -> usize {
        self.unit.is_some() as usize + self.outgoing.is_some() as usize
    }

    fn summary(&self) -> ChainSeat {
        ChainSeat { unit: self.id, kind: self.info.kind, bypassed: self.bypassed, latency: self.info.latency.clamp(0, u32::MAX as Frame) as u32 }
    }
}

/// An [`Event::Seat`] not yet taken by the event ring.
#[derive(Clone, Copy)]
struct Answer {
    frame: Frame,
    unit: UnitId,
    op: SeatOp,
    outcome: SeatOutcome,
}

/// A message taken from the ring and not yet applied: judged now, or held for a take.
struct Taken {
    msg: SlotMsg,
    /// An install's unit's [`UnitInfo`], read once with the unit held here; `tried` once that began (a
    /// unit that panicked there is never asked again).
    info: Option<UnitInfo>,
    tried: bool,
    /// Answered already (`Waiting`): it applies with no answer of its own.
    answered: bool,
}

/// What a message would do now.
enum Verdict {
    Refuse(SeatRefusal),
    /// Applies; `edit`: it changes the chain's latency sum or its role (held while a capture runs).
    Apply { edit: bool },
}

struct Slot {
    end: SlotEnd,
    /// `MAX_UNITS` seats; the chain is the first `len`, head first.
    seats: Vec<Seat>,
    len: usize,
    /// A message taken from the ring: the one being judged, or one held for a take.
    taken: Option<Taken>,
    /// The chain's latency as the source record line reads it: its sum at the last edit that left it
    /// seating a unit (an emptied chain keeps it, so a source's delayed tail plays out once).
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
    /// The chain's output over a render call (its last seat's, whichever that was at each frame).
    tail: [Vec<f32>; 2],
    gain_target: f64,
    gain: f64,
    /// The notes this slot's chain holds (bit per key).
    notes: [u64; 2],
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
    /// The chain changed on this frame and the ring has not taken its [`Event::Chain`] yet.
    chain_owed: Option<Frame>,
    /// A move waiting for its dip to reach silence: the unit and its index.
    reorder: Option<(UnitId, usize)>,
    /// The move's dip: down to silence on this frame (the reorder's), back up over the fade's frames
    /// after it. `None`: no dip.
    dip: Option<Frame>,
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

/// One frame of a crossfade: towards its far end while `engaged`, towards its near end otherwise.
fn step(fade: &mut u32, engaged: bool, n: u32) {
    if engaged && *fade < n {
        *fade += 1;
    } else if !engaged && *fade > 0 {
        *fade -= 1;
    }
}

/// A move's dip at frame `f`: 1 outside it, falling linearly to exactly 0 on `at` (the reorder's
/// frame) while `down`, rising from there to 1 over `n` frames after.
fn dip_gain(dip: Option<Frame>, down: bool, f: Frame, n: Frame) -> f32 {
    let Some(at) = dip else { return 1.0 };
    let k = if down { at - f } else { f - at };
    if k >= n {
        1.0
    } else {
        k.max(0) as f32 / n as f32
    }
}

/// Render seats `seats` (the chain, physical order) over frames `a..b` of a render call: each unit
/// called once on its slice, then each seat's crossfades frame by frame, its output left in its `out`,
/// the chain's in `tail`. `to_bus` falls to the frame a leaving source head is gone.
#[allow(clippy::too_many_arguments)]
fn run_chain(seats: &mut [Seat], gated: [&[f32]; 2], zeros: &[f32], dry: &mut [Vec<f32>; 2], tail: &mut [Vec<f32>; 2], events: &[SlotEvent], cx: &UnitCx, (a, b): (usize, usize), n: u32, to_bus: &mut usize, errors: &mut u64) {
    for k in 0..seats.len() {
        let (before, rest) = seats.split_at_mut(k);
        let seat = &mut rest[0];
        let seat_in: [&[f32]; 2] = if k == 0 { [&gated[0][a..b], &gated[1][a..b]] } else { [&before[k - 1].out[0][a..b], &before[k - 1].out[1][a..b]] };
        // A source takes no input.
        let source = seat.info.source;
        let x = if source { [&zeros[a..b], &zeros[a..b]] } else { seat_in };
        {
            let [ol, or] = &mut seat.out;
            match seat.unit.as_mut() {
                Some(unit) => {
                    unit.process(cx, x, events, [&mut ol[a..b], &mut or[a..b]]);
                    // A non-finite sample would stick in every filter, echo, limiter and loop after it:
                    // the whole call's output goes silent, counted.
                    if !ol[a..b].iter().chain(&or[a..b]).all(|y| y.is_finite()) {
                        ol[a..b].fill(0.0);
                        or[a..b].fill(0.0);
                        *errors += 1;
                    }
                }
                None => {
                    ol[a..b].fill(0.0);
                    or[a..b].fill(0.0);
                }
            }
        }
        if let Some((_, old, _)) = seat.outgoing.as_mut() {
            let [ol, or] = &mut seat.old;
            old.process(cx, x, events, [&mut ol[a..b], &mut or[a..b]]);
            if !ol[a..b].iter().chain(&or[a..b]).all(|y| y.is_finite()) {
                ol[a..b].fill(0.0);
                or[a..b].fill(0.0);
                *errors += 1;
            }
        }
        let swapping = seat.outgoing.is_some();
        let latency = seat.info.latency as usize;
        let cap = seat.line.len();
        for j in a..b {
            // What the unit (or, mid-replacement, the old into the new) sounds, then its bypass.
            let mut core = [seat.out[0][j], seat.out[1][j]];
            if swapping {
                core = [crossfade(seat.old[0][j], core[0], seat.swap, n), crossfade(seat.old[1][j], core[1], seat.swap, n)];
            }
            if source {
                // A source and its slot's input go different ways (the bus, the wet), so each fades
                // against silence there; once gone, the seat passes the gated input.
                let inner = [crossfade(0.0, core[0], seat.wet, n), crossfade(0.0, core[1], seat.wet, n)];
                if seat.removing && seat.fade == 0 {
                    *to_bus = (*to_bus).min(j);
                    (seat.out[0][j], seat.out[1][j]) = (gated[0][j], gated[1][j]);
                } else {
                    seat.out[0][j] = crossfade(0.0, inner[0], seat.fade, n);
                    seat.out[1][j] = crossfade(0.0, inner[1], seat.fade, n);
                    dry[0][j] = crossfade(gated[0][j], 0.0, seat.fade, n);
                    dry[1][j] = crossfade(gated[1][j], 0.0, seat.fade, n);
                }
            } else {
                let input = [seat_in[0][j - a], seat_in[1][j - a]];
                // The bypass line runs whether or not the seat is bypassed, so it is warm when it is.
                let delayed = if latency == 0 {
                    seat.filled = 0;
                    input
                } else {
                    seat.line[seat.line_at] = input;
                    let d = if seat.filled >= latency { seat.line[(seat.line_at + cap - latency) % cap] } else { [0.0; 2] };
                    seat.line_at = (seat.line_at + 1) % cap;
                    seat.filled = (seat.filled + 1).min(cap);
                    d
                };
                let inner = [crossfade(delayed[0], core[0], seat.wet, n), crossfade(delayed[1], core[1], seat.wet, n)];
                seat.out[0][j] = crossfade(input[0], inner[0], seat.fade, n);
                seat.out[1][j] = crossfade(input[1], inner[1], seat.fade, n);
            }
            step(&mut seat.fade, seat.engaged, n);
            step(&mut seat.wet, seat.wet_on, n);
            if swapping && seat.swap < n {
                seat.swap += 1;
            }
        }
    }
    let [tl, tr] = tail;
    match seats.last() {
        Some(last) => {
            tl[a..b].copy_from_slice(&last.out[0][a..b]);
            tr[a..b].copy_from_slice(&last.out[1][a..b]);
        }
        None => {
            tl[a..b].copy_from_slice(&gated[0][a..b]);
            tr[a..b].copy_from_slice(&gated[1][a..b]);
        }
    }
}

impl Slot {
    /// The physical indices of the chain as the host sees it: every seat not leaving.
    fn chain(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.len).filter(|&k| !self.seats[k].removing)
    }

    fn chain_len(&self) -> usize {
        self.chain().count()
    }

    /// The physical index of the `t`-th seat of the chain, or `len` past its end.
    fn physical(&self, t: usize) -> usize {
        self.chain().nth(t).unwrap_or(self.len)
    }

    /// The physical index of unit `id`'s seat in the chain (withdrawn included, leaving not).
    fn find(&self, id: UnitId) -> Option<usize> {
        self.chain().find(|&k| self.seats[k].id == id)
    }

    /// The chain's role: its head (as the host sees it) is a source.
    fn source_chain(&self) -> bool {
        self.chain().next().is_some_and(|k| self.seats[k].info.source)
    }

    /// The physical head is a source, leaving or not: the chain routes to the bus until it is gone.
    fn source_head(&self) -> bool {
        self.len > 0 && self.seats[0].info.source
    }

    /// The chain's latency: its seats' sum, a withdrawn seat's included, a leaving seat's not.
    fn latency(&self) -> Frame {
        self.chain().map(|k| self.seats[k].info.latency).sum()
    }

    /// A seat takes notes: one holds a unit and is not leaving.
    fn plays(&self) -> bool {
        self.chain().any(|k| self.seats[k].unit.is_some())
    }

    /// Units the slot holds: every seat's, and an install taken from its ring.
    fn units(&self) -> usize {
        self.seats[..self.len].iter().map(Seat::units).sum::<usize>() + self.taken.as_ref().is_some_and(|t| matches!(t.msg, SlotMsg::Install { .. })) as usize
    }

    /// A transition that indices or the role would shift under: a move's reorder, a replacement's
    /// crossfade, a withdrawal, or a leaving source. The slot's messages wait for its end.
    fn busy(&self) -> bool {
        self.reorder.is_some() || self.seats[..self.len].iter().any(|s| s.outgoing.is_some() || s.withdrawing || (s.removing && s.info.source))
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

    /// Whether the slot can take its next message from the ring now: the event ring's answer has a
    /// place among the owed, and an install a place on the return ring beside every unit the slot holds
    /// (so a refusal, a removal and an eviction always find one). A message that cannot waits in its
    /// ring.
    fn can_take(&self) -> bool {
        let Ok(msg) = self.end.rx.peek() else { return false };
        self.owed.len() < OWED_CAPACITY && (!matches!(msg, SlotMsg::Install { .. }) || self.end.tx.slots() > self.units())
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
            if self.notes[(key / 64) as usize] & (1 << (key % 64)) != 0 {
                self.queue(SlotEventKind::NoteOff { key }, offset, dropped);
            }
        }
        self.notes = [0; 2];
    }

    /// Owe `answer` to the event ring: [`Slot::can_take`] reserved its place.
    fn answer(&mut self, answer: Answer, errors: &mut u64) {
        if self.owed.len() < OWED_CAPACITY {
            self.owed.push(answer);
        } else {
            *errors += 1;
        }
    }

    /// The chain as [`Event::Chain`] carries it.
    fn summary(&self) -> (u8, [ChainSeat; MAX_UNITS]) {
        let mut seats = [ChainSeat::EMPTY; MAX_UNITS];
        let mut len = 0;
        for k in self.chain() {
            seats[len] = self.seats[k].summary();
            len += 1;
        }
        (len as u8, seats)
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
            let (id, bypassed) = (seat.id, seat.bypassed);
            self.give_back(Back { id, unit, refused: None, bypassed }, errors);
        }
        let seat = &mut self.seats[k];
        seat.removing = false;
        seat.engaged = false;
        seat.fade = 0;
        self.seats[k..self.len].rotate_left(1);
        self.len -= 1;
        if self.len == 0 {
            self.notes = [0; 2];
            self.events.clear();
        }
    }

    /// The seat at `k`'s unit leaves (its bypass reached): stopped while the seat holds it, handed back
    /// with its bypass; the seat stays.
    fn finish_withdrawal(&mut self, k: usize, errors: &mut u64) {
        if let Some(unit) = self.seats[k].unit.as_mut() {
            unit.stop();
        }
        let seat = &mut self.seats[k];
        seat.withdrawing = false;
        if let Some(unit) = seat.unit.take() {
            let (id, bypassed) = (seat.id, seat.bypassed);
            self.give_back(Back { id, unit, refused: None, bypassed }, errors);
        }
    }

    /// The seat at `k`'s replaced unit leaves (the crossfade reached the new one): stopped, handed back.
    fn finish_swap(&mut self, k: usize, errors: &mut u64) {
        if let Some((_, old, _)) = self.seats[k].outgoing.as_mut() {
            old.stop();
        }
        if let Some((id, unit, bypassed)) = self.seats[k].outgoing.take() {
            self.give_back(Back { id, unit, refused: None, bypassed }, errors);
        }
    }

    /// After a render call (or at once, idle): every transition that reached its end finishes.
    fn finish(&mut self, n: u32, errors: &mut u64) {
        let mut k = 0;
        while k < self.len {
            let seat = &self.seats[k];
            if seat.removing && seat.fade == 0 {
                self.finish_removal(k, errors);
                continue;
            }
            if seat.withdrawing && seat.wet == 0 {
                self.finish_withdrawal(k, errors);
            }
            if self.seats[k].outgoing.is_some() && self.seats[k].swap == n {
                self.finish_swap(k, errors);
            }
            k += 1;
        }
    }

    /// An idle removal, withdrawal or replacement, or an eviction: no block will carry the notes the
    /// units were just released from, so one silent frame from `frame` (the next the engine would
    /// render) does, to every unit leaving, and a restarted unit holds no note.
    fn flush(&mut self, cx: &UnitCx, zeros: &[f32]) {
        if self.events.is_empty() {
            return;
        }
        for seat in self.seats[..self.len].iter_mut() {
            let [l, r] = &mut seat.out;
            if seat.removing || seat.withdrawing {
                if let Some(unit) = seat.unit.as_mut() {
                    unit.process(cx, [&zeros[..1], &zeros[..1]], &self.events, [&mut l[..1], &mut r[..1]]);
                }
            }
            if let Some((_, old, _)) = seat.outgoing.as_mut() {
                old.process(cx, [&zeros[..1], &zeros[..1]], &self.events, [&mut l[..1], &mut r[..1]]);
            }
        }
        self.events.clear();
    }

    /// No device runs: every transition under way ends at once (a move's reorder, a seat's leaving, a
    /// withdrawal, a replacement), the leaving units flushed, stopped and handed back.
    fn finish_idle(&mut self, cx: &UnitCx, zeros: &[f32], n: u32, errors: &mut u64) {
        if let Some((id, to)) = self.reorder.take() {
            self.apply_reorder(id, to);
            self.chain_owed = Some(cx.frame);
        }
        self.dip = None;
        for seat in self.seats[..self.len].iter_mut() {
            if seat.removing {
                seat.fade = 0;
            }
            if seat.withdrawing {
                seat.wet = 0;
            }
            if seat.outgoing.is_some() {
                seat.swap = n;
            }
        }
        self.flush(cx, zeros);
        self.finish(n, errors);
    }

    /// Unit `id` to index `to` of the chain, at once.
    fn apply_reorder(&mut self, id: UnitId, to: usize) {
        reorder(&mut self.seats[..self.len], id, to);
    }
}

/// Unit `id` (not leaving) to index `to` among the seats not leaving of `seats`, at once.
fn reorder(seats: &mut [Seat], id: UnitId, to: usize) {
    let len = seats.len();
    let Some(p) = seats.iter().position(|s| s.id == id && !s.removing) else { return };
    seats[p..].rotate_left(1);
    // The others in [..len - 1], the moved one parked at `len - 1`.
    let q = (0..len - 1).filter(|&k| !seats[k].removing).nth(to).unwrap_or(len - 1);
    seats[q..].rotate_right(1);
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
    /// A render call has run: from here a live toggle ramps and a bypass crossfades (before it, each
    /// sets the starting state).
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
    /// Bypass commands naming an id no seat holds.
    pub(crate) unit_commands_dropped: u64,
}

impl Rack {
    /// Allocates: build it off the audio thread. Every record line holds `max_input` (the device's
    /// input side at most: `EngineConfig::max_input_frames`) plus [`max_chain_latency`] plus a block;
    /// every seat's bypass line [`max_chain_latency`] and one.
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
                        wet: 0,
                        wet_on: false,
                        bypassed: false,
                        withdrawing: false,
                        outgoing: None,
                        swap: 0,
                        out: [vec![0.0; block], vec![0.0; block]],
                        old: [vec![0.0; block], vec![0.0; block]],
                        line: vec![[0.0; 2]; max_latency as usize + 1],
                        line_at: 0,
                        filled: 0,
                    })
                    .collect(),
                len: 0,
                taken: None,
                line_latency: 0,
                live: false,
                live_end: Frame::MIN,
                live_gain: 0.0,
                shut: 0,
                gated: [vec![0.0; block], vec![0.0; block]],
                dry: [vec![0.0; block], vec![0.0; block]],
                tail: [vec![0.0; block], vec![0.0; block]],
                gain_target: 1.0,
                gain: 1.0,
                notes: [0; 2],
                events: Vec::with_capacity(MAX_SLOT_EVENTS),
                line: vec![[0.0; 2]; line],
                wet_line: vec![[0.0; 2]; line],
                dw: 0,
                write: 0,
                ott: StereoOtt::new(sample_rate),
                owed: Vec::with_capacity(OWED_CAPACITY),
                chain_owed: None,
                reorder: None,
                dip: None,
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
            unit_commands_dropped: 0,
        };
        (rack, ports)
    }

    fn unit_cx(&self, frame: Frame) -> UnitCx {
        UnitCx { frame, dsp_frame: frame - self.clock.skipped, bpm: self.clock.bpm, meter: self.clock.meter }
    }

    /// Whether `id` is spoken for in a slot other than `i`: seated there (leaving included), or the
    /// install it has taken from its ring and not applied.
    fn claimed_elsewhere(&self, i: usize, id: UnitId) -> bool {
        self.slots.iter().enumerate().filter(|&(j, _)| j != i).any(|(_, s)| {
            s.seats[..s.len].iter().any(|seat| seat.id == id) || s.taken.as_ref().is_some_and(|t| matches!(t.msg, SlotMsg::Install { id: x, .. } if x == id))
        })
    }

    /// Apply the ports' messages at `frame`, a block start (or, idle, the frame the next block would
    /// start at), each answered once. `engaged` installs without a fade (nothing has sounded yet, or no
    /// device runs); `fade` off finishes every transition at once; `hold`: a capture holds the
    /// alignment, so an edit of a chain's sum or role waits.
    fn service(&mut self, frame: Frame, engaged: bool, fade: bool, hold: bool) {
        let cx = self.unit_cx(frame);
        let n = self.fade_frames;
        for i in 0..SLOT_COUNT {
            loop {
                if !fade {
                    let (s, zeros, errors) = (&mut self.slots[i], &self.zeros, &mut self.protocol_errors);
                    s.finish_idle(&cx, zeros, n, errors);
                }
                if self.slots[i].busy() {
                    break;
                }
                // Each message's answer and return have their places before it leaves its ring: nothing
                // it owes is ever dropped, and one that has no place yet waits there, in order.
                if self.slots[i].taken.is_none() {
                    if !self.slots[i].can_take() {
                        break;
                    }
                    let Ok(msg) = self.slots[i].end.rx.pop() else { break };
                    self.slots[i].taken = Some(Taken { msg, info: None, tried: false, answered: false });
                }
                let verdict = self.judge(i);
                let s = &mut self.slots[i];
                let Some(t) = s.taken.as_mut() else { break };
                let (unit, op, answered) = (t.msg.id(), t.msg.op(), t.answered);
                let outcome = match verdict {
                    Verdict::Apply { edit: true } if hold => {
                        if !answered {
                            t.answered = true;
                            s.answer(Answer { frame, unit, op, outcome: SeatOutcome::Waiting }, &mut self.protocol_errors);
                            s.end.taken.fetch_add(1, Release);
                        }
                        s.end.holding.store(true, Release);
                        break;
                    }
                    Verdict::Apply { .. } => {
                        self.apply(i, frame, engaged, fade);
                        SeatOutcome::Applied
                    }
                    Verdict::Refuse(why) => {
                        let s = &mut self.slots[i];
                        if let Some(Taken { msg: SlotMsg::Install { id, bypassed, unit, .. }, .. }) = s.taken.take() {
                            s.give_back(Back { id, unit, refused: Some(why), bypassed }, &mut self.protocol_errors);
                        }
                        SeatOutcome::Refused(why)
                    }
                };
                let s = &mut self.slots[i];
                s.taken = None;
                s.end.holding.store(false, Release);
                if !answered {
                    s.answer(Answer { frame, unit, op, outcome }, &mut self.protocol_errors);
                    s.end.taken.fetch_add(1, Release);
                }
            }
            if !fade {
                let (s, zeros, errors) = (&mut self.slots[i], &self.zeros, &mut self.protocol_errors);
                s.finish_idle(&cx, zeros, n, errors);
            }
        }
    }

    /// What slot `i`'s taken message would do now. An install's unit's kind is read here, once, while
    /// the slot holds it (a unit that panicked there is refused as `Unknown` when asked again).
    fn judge(&mut self, i: usize) -> Verdict {
        let max_latency = self.max_latency;
        let Some(t) = self.slots[i].taken.as_mut() else { return Verdict::Refuse(SeatRefusal::Unknown) };
        if let SlotMsg::Install { unit, .. } = &t.msg {
            if !t.tried {
                t.tried = true;
                let info = unit.info();
                t.info = Some(UnitInfo { latency: info.latency.max(0), ..info });
            }
        }
        let info = t.info;
        let s = &self.slots[i];
        let (sum, role) = (s.latency(), s.source_chain());
        match s.taken.as_ref().map(|t| &t.msg) {
            Some(&SlotMsg::Install { id, at, replaces, .. }) => {
                let Some(info) = info else { return Verdict::Refuse(SeatRefusal::Unknown) };
                if id == 0 || self.claimed_elsewhere(i, id) {
                    return Verdict::Refuse(SeatRefusal::Id);
                }
                // The seat it takes: a withdrawn seat under its id, the one it replaces, or a new one.
                let own = s.find(id);
                let into = match (replaces, own) {
                    (Some(r), _) => match s.find(r) {
                        Some(k) if own.is_none_or(|o| o == k) => Some(k),
                        Some(_) => return Verdict::Refuse(SeatRefusal::Id),
                        None => return Verdict::Refuse(SeatRefusal::Unknown),
                    },
                    (None, Some(k)) if s.seats[k].unit.is_none() => Some(k),
                    (None, Some(_)) => return Verdict::Refuse(SeatRefusal::Id),
                    (None, None) => None,
                };
                // A leaving seat with this id still holds its unit: a new one waits for no seat of its.
                if into.is_none() && s.seats[..s.len].iter().any(|seat| seat.id == id) {
                    return Verdict::Refuse(SeatRefusal::Id);
                }
                let (t, before) = match into {
                    Some(k) => (s.chain().position(|p| p == k).unwrap_or(0), s.seats[k].info.latency),
                    None if s.len == MAX_UNITS => return Verdict::Refuse(SeatRefusal::Full),
                    None if info.source => (0, 0),
                    None => (at.min(s.chain_len()), 0),
                };
                // A source only at the head (one there is replaced only when named); no effect ahead of
                // a source.
                let head_source = role && into.is_none_or(|k| s.chain().next() != Some(k));
                if (info.source && t > 0) || (info.source && into.is_none() && role) || (!info.source && t == 0 && head_source) {
                    return Verdict::Refuse(SeatRefusal::Placement);
                }
                let after = sum - before + info.latency;
                if info.latency > max_latency - (sum - before) {
                    return Verdict::Refuse(SeatRefusal::Latency);
                }
                let new_role = if t == 0 { info.source } else { role };
                Verdict::Apply { edit: after != sum || new_role != role }
            }
            Some(&SlotMsg::Remove { id }) => match s.find(id) {
                Some(k) => {
                    let head = s.chain().next() == Some(k);
                    Verdict::Apply { edit: s.seats[k].info.latency != 0 || (head && s.seats[k].info.source) }
                }
                None => Verdict::Refuse(SeatRefusal::Unknown),
            },
            Some(&SlotMsg::Withdraw { id }) => match s.find(id) {
                Some(k) if s.seats[k].unit.is_some() => Verdict::Apply { edit: false },
                _ => Verdict::Refuse(SeatRefusal::Unknown),
            },
            Some(&SlotMsg::Move { id, to }) => match s.find(id) {
                Some(k) => {
                    let to = to.min(s.chain_len() - 1);
                    let from = s.chain().position(|p| p == k).unwrap_or(0);
                    if from != to && (s.seats[k].info.source || (to == 0 && role)) {
                        Verdict::Refuse(SeatRefusal::Placement)
                    } else {
                        Verdict::Apply { edit: false }
                    }
                }
                None => Verdict::Refuse(SeatRefusal::Unknown),
            },
            None => Verdict::Refuse(SeatRefusal::Unknown),
        }
    }

    /// Apply slot `i`'s taken message, which [`Rack::judge`] passed, at `frame`.
    fn apply(&mut self, i: usize, frame: Frame, engaged: bool, fade: bool) {
        let n = self.fade_frames;
        let rendered = self.rendered;
        let cx = self.unit_cx(frame);
        let s = &mut self.slots[i];
        let Some(t) = s.taken.take() else { return };
        match t.msg {
            SlotMsg::Install { id, at, bypassed, replaces, unit } => {
                let info = t.info.unwrap_or(UnitInfo { kind: UnitKind::Plugin, source: false, latency: 0 });
                let into = replaces.and_then(|r| s.find(r)).or_else(|| s.find(id));
                let k = match into {
                    Some(k) => {
                        // A replacement releases what the slot holds, as a removal does; the old unit
                        // crossfades out of the seat (at once, idle) and goes back.
                        let seat = &mut s.seats[k];
                        if let Some(old) = seat.unit.take() {
                            seat.outgoing = Some((seat.id, old, seat.bypassed));
                            seat.swap = if fade { 0 } else { n };
                        }
                        if seat.outgoing.is_some() {
                            s.release_all(0, &mut self.events_dropped);
                        }
                        let seat = &mut s.seats[k];
                        if seat.info.latency == 0 || info.latency == 0 {
                            seat.filled = 0;
                        }
                        (seat.id, seat.info, seat.unit) = (id, info, Some(unit));
                        k
                    }
                    None => {
                        let k = s.len;
                        let p = if info.source { s.physical(0) } else { s.physical(at.min(s.chain_len())) };
                        if s.len == 0 {
                            // A chain that held nothing holds no note.
                            s.notes = [0; 2];
                            s.events.clear();
                        }
                        let seat = &mut s.seats[k];
                        (seat.id, seat.info, seat.unit) = (id, info, Some(unit));
                        (seat.removing, seat.engaged, seat.fade) = (false, true, if engaged { n } else { 0 });
                        (seat.withdrawing, seat.outgoing, seat.swap) = (false, None, n);
                        (seat.line_at, seat.filled) = (0, 0);
                        s.len += 1;
                        s.seats[p..=k].rotate_right(1);
                        p
                    }
                };
                let seat = &mut s.seats[k];
                seat.bypassed = bypassed;
                let handled = bypassed && seat.unit.as_mut().is_some_and(|u| u.set_bypass(&cx, true));
                seat.wet_on = !bypassed || handled;
                // A new seat starts at its bypass (its lifecycle fade carries it in); a reseat fades in
                // from its bypass, a replacement keeps where its crossfade is.
                if into.is_none() || !fade || !rendered {
                    seat.wet = if seat.wet_on { n } else { 0 };
                }
            }
            SlotMsg::Remove { id } => {
                let Some(k) = s.find(id) else { return };
                s.release_all(0, &mut self.events_dropped);
                let seat = &mut s.seats[k];
                seat.removing = true;
                seat.engaged = false;
                if !fade {
                    seat.fade = 0;
                }
            }
            SlotMsg::Withdraw { id } => {
                let Some(k) = s.find(id) else { return };
                // A source's notes are released; the seat fades to its bypass.
                if s.seats[k].info.source {
                    s.release_all(0, &mut self.events_dropped);
                }
                let seat = &mut s.seats[k];
                seat.withdrawing = true;
                seat.wet_on = false;
                if !fade {
                    seat.wet = 0;
                }
            }
            SlotMsg::Move { id, to } => {
                let Some(k) = s.find(id) else { return };
                let to = to.min(s.chain_len() - 1);
                if s.chain().position(|p| p == k) == Some(to) {
                    return;
                }
                if !fade || !rendered {
                    s.apply_reorder(id, to);
                } else {
                    // Down from where the dip is now (rising after an earlier move, or level) to silence.
                    let up = s.dip.map_or(n as Frame, |at| (frame - at).clamp(0, n as Frame));
                    s.dip = Some(frame + up);
                    s.reorder = Some((id, to));
                    if up == 0 {
                        // Silent already: the reorder lands on this frame.
                        s.apply_reorder(id, to);
                        s.reorder = None;
                    }
                }
            }
        }
        if s.chain().next().is_some() {
            s.line_latency = s.latency();
        }
        s.chain_owed = Some(frame);
    }

    /// The bypass of unit `id` at `now`: `Some` outright, `None` switched from the engine's value. On a
    /// seat that holds its unit, through [`UnitProcessor::set_bypass`]; a withdrawn seat keeps the value
    /// (the reseat brings its own). Answered by [`Event::Chain`]; an id no seat holds is dropped and
    /// counted.
    pub(crate) fn set_unit_bypass(&mut self, id: UnitId, bypassed: Option<bool>, now: Frame) {
        let (n, rendered, cx) = (self.fade_frames, self.rendered, self.unit_cx(now));
        let found = (0..SLOT_COUNT).find_map(|i| self.slots[i].find(id).map(|k| (i, k)));
        let Some((i, k)) = found else {
            self.unit_commands_dropped += 1;
            return;
        };
        let s = &mut self.slots[i];
        let seat = &mut s.seats[k];
        let b = bypassed.unwrap_or(!seat.bypassed);
        seat.bypassed = b;
        if !seat.withdrawing {
            if let Some(unit) = seat.unit.as_mut() {
                let handled = unit.set_bypass(&cx, b);
                seat.wet_on = !b || handled;
                if !rendered {
                    seat.wet = if seat.wet_on { n } else { 0 };
                }
            }
        }
        s.chain_owed = Some(now);
    }

    /// The answers the event ring has not taken, offered in order until it refuses one; each slot's
    /// owed [`Event::Chain`] after its seat answers.
    pub(crate) fn offer(&mut self, feed: &mut Feed) {
        for (i, s) in self.slots.iter_mut().enumerate() {
            let taken = s.owed.iter().take_while(|a| feed.push(Event::Seat { frame: a.frame, slot: i as u8, unit: a.unit, op: a.op, outcome: a.outcome })).count();
            s.owed.drain(..taken);
            if let (true, Some(frame)) = (s.owed.is_empty(), s.chain_owed) {
                let (len, seats) = s.summary();
                if feed.push(Event::Chain { frame, slot: i as u8, len, seats }) {
                    s.chain_owed = None;
                }
            }
        }
    }

    /// At a block start: service the ports and start the render cursor. `hold`: a capture holds the
    /// alignment.
    pub(crate) fn begin_block(&mut self, start: Frame, first: bool, clock: UnitClock, hold: bool) {
        self.clock = clock;
        self.service(start, first, true, hold);
        self.cursor = start;
    }

    /// With no device running (the host holds the engine): apply the ports' messages at once, at `frame`
    /// (the next the engine would render), no fades. A leaving unit stops here and goes straight back.
    pub(crate) fn service_idle(&mut self, frame: Frame, hold: bool) {
        self.cursor = frame;
        self.service(frame, true, false, hold);
    }

    /// Stop every unit and hand it back with its bypass (a sample-rate change: the host re-activates
    /// and reinstalls), with every install still waiting on a port that the return ring has room for:
    /// the host reads its returns and evicts again until none comes back, so nothing is left for the
    /// engine's drop.
    pub(crate) fn evict(&mut self, frame: Frame) {
        self.service_idle(frame, false);
        let (cx, n) = (self.unit_cx(frame), self.fade_frames);
        for s in self.slots.iter_mut() {
            if s.len > 0 {
                s.release_all(0, &mut self.events_dropped);
                for seat in s.seats[..s.len].iter_mut() {
                    seat.removing = true;
                    seat.engaged = false;
                    seat.fade = 0;
                }
                s.finish_idle(&cx, &self.zeros, n, &mut self.protocol_errors);
                s.chain_owed = Some(frame);
            }
        }
    }

    /// The largest latency of a live input-fed chain now: what [`Rack::latch_record`] takes while open.
    pub(crate) fn live_latency(&self) -> Frame {
        self.slots.iter().filter(|s| s.live && s.shut == 0 && !s.source_chain()).map(Slot::latency).max().unwrap_or(0)
    }

    /// At a block start, after the ports were serviced: while `open`, the alignment follows the slots
    /// (the live latency); shut, it holds what it last took. The engine shuts it while a capture's
    /// alignment is fixed, which took the same live latency, and no chain's sum or role changes then
    /// (an edit that would waits: [`Rack::service`]). Either way each slot's wet is delayed by what its
    /// own chain's latency lacks of that alignment.
    pub(crate) fn latch_record(&mut self, open: bool) {
        if open {
            self.record_latency = self.live_latency();
        }
        for s in self.slots.iter_mut() {
            let own = if !s.source_chain() { s.latency() } else { 0 };
            s.dw = (self.record_latency - own).clamp(0, (s.wet_line.len() - 1) as Frame) as usize;
        }
    }

    /// The live latency the record tap is aligned on, as last latched.
    pub(crate) fn record_latency(&self) -> Frame {
        self.record_latency
    }

    /// Slot `slot`'s chain: its role and its latency, `None` while it seats nothing (a leaving seat
    /// counts until it has left).
    pub(crate) fn installed(&self, slot: usize) -> Option<(SlotKind, Frame)> {
        let s = self.slots.get(slot).filter(|s| s.len > 0)?;
        Some((if s.source_head() { SlotKind::Instrument } else { SlotKind::Effect }, s.latency()))
    }

    /// Slot `slot`'s chain as the host sees it, head first.
    pub(crate) fn chain(&self, slot: usize) -> impl Iterator<Item = ChainSeat> + '_ {
        self.slots.get(slot).into_iter().flat_map(|s| s.chain().map(|k| s.seats[k].summary()))
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
        s.notes[(key / 64) as usize] |= 1 << (key % 64);
    }

    pub(crate) fn note_off(&mut self, key: u8, now: Frame) {
        let offset = self.offset(now);
        let Some(s) = self.target.map(|i| &mut self.slots[i]) else { return };
        if key > 127 || s.notes[(key / 64) as usize] & (1 << (key % 64)) == 0 {
            return;
        }
        s.notes[(key / 64) as usize] &= !(1 << (key % 64));
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
            let mut to_bus = if s.source_head() { m } else { 0 };
            // A move's reorder inside this call splits it there: every unit is called once on each side.
            let split = match (s.reorder, s.dip) {
                (Some(_), Some(at)) if at >= frame && at < frame + m as Frame => (at - frame) as usize,
                _ => m,
            };
            let len = s.len;
            let first = s.events.iter().take_while(|e| (e.offset as usize) < split).count();
            run_chain(&mut s.seats[..len], gated, zeros, &mut s.dry, &mut s.tail, &s.events[..first], &self_cx(&self.clock, frame), (0, split), fade_frames, &mut to_bus, &mut self.protocol_errors);
            if split < m {
                if let Some((id, to)) = s.reorder.take() {
                    reorder(&mut s.seats[..len], id, to);
                    s.chain_owed = Some(frame + split as Frame);
                }
                for e in s.events[first..].iter_mut() {
                    e.offset -= split as u32;
                }
                run_chain(&mut s.seats[..len], gated, zeros, &mut s.dry, &mut s.tail, &s.events[first..], &self_cx(&self.clock, frame + split as Frame), (split, m), fade_frames, &mut to_bus, &mut self.protocol_errors);
            }
            s.events.clear();
            let dipping = s.dip.is_some();
            let len = s.line.len();
            let (d, dw) = ((delay - s.line_latency).clamp(0, (len - 1) as Frame) as usize, s.dw);
            for j in 0..m {
                let mut y = [s.tail[0][j], s.tail[1][j]];
                if dipping {
                    let g = dip_gain(s.dip, s.reorder.is_some(), frame + j as Frame, fade_frames as Frame);
                    if g != 1.0 {
                        y = y.map(|v| v * g);
                    }
                }
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
            // The dip has risen back: settled.
            if let (None, Some(at)) = (s.reorder, s.dip) {
                if frame + m as Frame >= at + fade_frames as Frame {
                    s.dip = None;
                }
            }
            s.finish(fade_frames, &mut self.protocol_errors);
        }
        for (r, (&l, &rr)) in record.iter_mut().zip(al_l.iter().zip(al_r.iter())) {
            r[0] += l;
            r[1] += rr;
        }
        self.cursor += m as Frame;
    }
}

/// The units' clock at `frame` (a free function: the render loop holds the slots mutably).
fn self_cx(clock: &UnitClock, frame: Frame) -> UnitCx {
    UnitCx { frame, dsp_frame: frame - clock.skipped, bpm: clock.bpm, meter: clock.meter }
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
        rack.service_idle(0, false);
        rack.select(Some(0), 0);
        for k in 0..MAX_SLOT_EVENTS + 3 {
            rack.note_on((k % 128) as u8, 1.0, 0);
        }
        assert_eq!(rack.events_dropped, 3);
        assert_eq!(rack.slots[0].events.len(), MAX_SLOT_EVENTS);
    }
}
