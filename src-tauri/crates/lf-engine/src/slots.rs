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
//! target (notes are kept per unit: a unit gets a NoteOff only for a key it holds, a leaving unit none
//! but the releases of what it holds, once, as it starts leaving, and a new one holds none); its output joins the master bus and goes to the record tap delayed like a built-in
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
//! source; a ninth unit is refused. A **removal** fades the seat to its undelayed input, calls the unit
//! for that fade and no frame past it, stops it once and hands it back; the slot stays live.
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
//! resolves at the first block start after with one second [`Event::Seat`] (`Applied` on its frame,
//! ahead of its [`Event::Chain`], or `Refused`). A move, a
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
//! its answer and its chain have their places among those the slot owes the event ring and, an install,
//! once the return ring has a place for it beside every unit the slot holds: a full ring keeps the
//! message waiting, in order (a bypass command too, in the engine's table), and nothing owed is ever
//! dropped or folded; the slots take turns at the event ring. A slot takes no message while a move's reorder, a
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

use crate::api::{max_chain_latency, unit_meter_frames, BandMeter, ChainSeat, Event, InstallToken, Pair, SeatOp, SeatOutcome, SeatRefusal, SlotEvent, SlotEventKind, SlotKind, UnitCx, UnitId, UnitInfo, UnitKind, UnitMeter, UnitParam, UnitProcessor, MAX_UNITS, SLOT_COUNT};
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
/// What one slot owes the event ring: an answer ([`Event::Seat`]) and a chain ([`Event::Chain`]) per
/// message, and one place kept for an eviction's chain. A bypass's chain takes a free place, or its
/// command waits ([`Rack::bypass_room`]).
const OWED_CAPACITY: usize = 2 * MSG_CAPACITY + 1;

type Unit = Box<dyn UnitProcessor>;

enum SlotMsg {
    Install { id: UnitId, token: InstallToken, at: usize, bypassed: bool, replaces: Option<UnitId>, unit: Unit },
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

/// Why a unit left the engine ([`Returned::left`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Left {
    /// A removal took it.
    Removed,
    /// An install that `replaces` it, or a same-id reload, took its seat.
    Replaced,
    /// A withdrawal took it.
    Withdrawn,
    /// [`crate::Engine::evict_slots`] took a seated unit.
    Evicted,
    /// [`crate::Engine::evict_slots`] took an install still in the ring or the taken place, never applied
    /// because it had no place for its answer (an install with reply room applies in `evict`'s idle
    /// service first and leaves as [`Left::Evicted`], with its predecessor as [`Left::Replaced`]).
    Unsent,
    /// Its install was refused (the unit never started or stopped).
    Refused(SeatRefusal),
}

/// A unit on its way back to the host: removed, withdrawn, replaced or evicted with the bypass its
/// seat applied, or refused at its install (with the bypass it asked for).
struct Back {
    id: UnitId,
    token: InstallToken,
    unit: Unit,
    bypassed: bool,
    left: Left,
}

/// A unit the engine handed back ([`SlotPort::returned_unit`]).
pub struct Returned {
    pub id: UnitId,
    pub token: InstallToken,
    pub unit: Box<dyn UnitProcessor>,
    /// The bypass its seat applied when it left (a refused install or unsent install: the one it asked for).
    pub bypassed: bool,
    pub left: Left,
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
    /// ([`SlotPort::holding`]; it resolves after with a second [`Event::Seat`]).
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
        self.install_at(id, 0, MAX_UNITS, false, None, unit)
    }

    /// Hand an activated unit to the slot under `id` and `token`, to seat at index `at` of its chain (clamped to its
    /// end; a source goes to the head), `bypassed` or not; `replaces`, a seated unit whose seat it
    /// takes in one crossfade (a source replaced, or a reload under its own id; one of the other role is
    /// refused as `Placement`). An `id` the slot holds withdrawn reseats that seat; a reseat or reload
    /// under its own id keeps the seat's latest bypass, not `bypassed`. It applies at the next block
    /// start, or waits for a take to end
    /// (`docs/unit-contract.md` §7), and is answered by an [`Event::Seat`] (a held message gets a second
    /// [`Event::Seat`] when it resolves; a refused one comes
    /// back on [`SlotPort::returned`], its reason on [`SlotPort::refused`]). The ticket tells when the
    /// engine has answered it. Gives the unit back when the message ring is full (the engine is not
    /// rendering and has not been serviced, or the host has not read its returns).
    pub fn install_at(&mut self, id: UnitId, token: InstallToken, at: usize, bypassed: bool, replaces: Option<UnitId>, unit: Unit) -> Result<Ticket, Unit> {
        self.tx.push(SlotMsg::Install { id, token, at, bypassed, replaces, unit }).map_err(|PushError::Full(msg)| match msg {
            SlotMsg::Install { unit, .. } => unit,
            _ => unreachable!(),
        })?;
        self.sent += 1;
        Ok(Ticket { seq: self.sent, taken: self.taken.clone() })
    }

    /// Ask for unit `id` back: the unit releases the notes it holds, the seat crossfades to its input,
    /// the unit stops and arrives on [`SlotPort::returned`]. Answered by an [`Event::Seat`]; nothing comes back
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

    /// [`SlotPort::returned`] with its token, bypass and why it left.
    pub fn returned_unit(&mut self) -> Option<Returned> {
        self.rx.pop().ok().map(|b| Returned { id: b.id, token: b.token, unit: b.unit, bypassed: b.bypassed, left: b.left })
    }

    /// Unit `id` with why the engine refused its install, if that is what the port hands back next.
    pub fn refused(&mut self, id: UnitId) -> Option<(Unit, SeatRefusal)> {
        if !self.rx.peek().is_ok_and(|b| b.id == id && matches!(b.left, Left::Refused(_))) {
            return None;
        }
        let back = self.rx.pop().ok()?;
        match back.left {
            Left::Refused(why) => Some((back.unit, why)),
            _ => None,
        }
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
    /// The install token this seat's unit arrived with ([`SlotPort::install_at`]).
    token: InstallToken,
    unit: Option<Unit>,
    /// Read once, at install; a withdrawn seat keeps its unit's.
    info: UnitInfo,
    /// The lifecycle crossfade: frames into the linear undelayed input (0) ↔ seat (`Rack::fade_frames`),
    /// counted so both ends land on an exact frame; `engaged` is the end it heads for.
    fade: u32,
    engaged: bool,
    /// The seat leaves once `fade` reaches its input; it counts in no index or sum meanwhile.
    removing: bool,
    /// Marked leaving by [`Rack::evict`]: keeps [`Left::Evicted`] even if a `stop` unwinds and a later
    /// `service_idle` finishes it.
    evicting: bool,
    /// The bypass crossfade: frames into the delayed input (0) ↔ the unit's output (n); `wet_on` is the
    /// end it heads for (off while the seat bypasses a unit that does not bypass itself, or has none).
    wet: u32,
    wet_on: bool,
    /// The bypass as applied: what [`Event::Chain`] shows and the unit goes back with.
    bypassed: bool,
    /// The unit leaves once `wet` reaches the delayed input; the seat stays.
    withdrawing: bool,
    /// A replaced unit (its id, its install token, the unit and its bypass), crossfaded out over `swap`
    /// (0 old ↔ n new) and handed back once it ends.
    outgoing: Option<(UnitId, InstallToken, Unit, bool)>,
    swap: u32,
    /// The replaced unit's kind, latency and where its bypass crossfade stood when it was replaced:
    /// what it sounds while it crossfades out.
    old_kind: UnitKind,
    old_latency: usize,
    old_wet: u32,
    /// The keys the unit holds (bit per key), and those the outgoing unit held: a NoteOff reaches a unit
    /// only for a key it holds, and a leaving unit gets the releases of what it holds, once.
    held: [u64; 2],
    old_held: [u64; 2],
    /// One unit's events over a call (allocated with the rack: a whole queue, or every key's release).
    ev: Vec<SlotEvent>,
    /// The unit's output over a render call, then the seat's (the crossfades applied in place).
    out: [Vec<f32>; 2],
    /// The outgoing unit's output over a render call.
    old: [Vec<f32>; 2],
    /// The seat's input, its last [`max_chain_latency`] frames (an effect's bypass reads it at its
    /// latency): written at `line_at`, `filled` frames of it valid.
    line: Vec<Pair>,
    line_at: usize,
    filled: usize,
    /// Owed [`Event::UnitParam`] answers for this seat (one per [`UnitParam`]): the latest applied value
    /// and its frame when the event ring has not taken it yet.
    params_owed: [Option<(Frame, f64)>; UnitParam::COUNT],
    /// Folded output peak per channel (`[left, right]`) since the last [`UnitMeter`] publish under this
    /// seat's id: kept across a same-id replacement and a withdrawal, reset by a removal or another id.
    meter_out: [f32; 2],
    /// Rendered frames folded into `meter_out` since the last [`UnitMeter`] publish for this seat.
    meter_frames: usize,
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

/// Owe `o` to the event ring, after what is owed already: its place was kept ([`Slot::room`]). Should
/// it ever have none, it is counted, never written over another.
fn owe(owed: &mut Vec<Owed>, o: Owed, errors: &mut u64) {
    if owed.len() < OWED_CAPACITY {
        owed.push(o);
    } else {
        *errors += 1;
    }
}

/// What a slot owes the event ring, in the order it applied.
#[derive(Clone, Copy)]
enum Owed {
    Seat(Answer),
    /// The chain as one applied change left it, on that change's frame.
    Chain { frame: Frame, len: u8, seats: [ChainSeat; MAX_UNITS] },
}

/// A message taken from the ring and not yet applied: judged now, or held for a take.
struct Taken {
    msg: SlotMsg,
    /// An install's unit's [`UnitInfo`], read once with the unit held here; `tried` once that began (a
    /// unit that panicked there is never asked again).
    info: Option<UnitInfo>,
    tried: bool,
    /// Answered `Waiting` already: its ticket was answered, and it still owes its terminal [`Event::Seat`]
    /// (`Applied` or `Refused`) when the hold ends.
    answered: bool,
}

/// What a message would do now.
enum Verdict {
    Refuse(SeatRefusal),
    /// Applies; `edit`: it changes the chain's latency sum or its role (held while a capture runs).
    Apply { edit: bool },
    /// Cannot apply yet, unanswered: a source (new, or a reseat that turns the head into one) while a
    /// seat ahead of where it lands is still leaving (it goes at physical index 0, the head's record
    /// path, once that seat has gone).
    Wait,
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
    /// The most frames any audio can take from this slot's input to the record tap, as
    /// [`Rack::latch_record`] last took it ([`Slot::reach`]).
    reach: Frame,
    /// The frames the crossfades of an edit taken since the last latch run (a move's dip, down and back
    /// up, twice the fade); 0: none ([`Rack::latch_record`]).
    edit: Frame,
    write: usize,
    /// The input OTT on this slot's output, after its gain.
    ott: StereoOtt,
    /// Answers and chains the event ring has not taken yet, oldest first: none is ever dropped or
    /// folded into another ([`Slot::room`]).
    owed: Vec<Owed>,
    /// A move waiting for its dip to reach silence: the unit and its index.
    reorder: Option<(UnitId, usize)>,
    /// A bypass command on one of this slot's units waits for a place among the owed: the next place
    /// freed is kept for it, ahead of a port message ([`Slot::room`]).
    reserved: bool,
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
        let (t, d) = (fade as f32 / n as f32, to - from);
        if from.is_finite() && to.is_finite() && !d.is_finite() {
            // Extreme finite ends: the span overflows, so weigh the ends instead, inside them.
            (from * (1.0 - t) + to * t).max(from.min(to)).min(from.max(to))
        } else {
            from + t * d
        }
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
/// frame), rising from there to 1 over `n` frames after. Judged by the frame alone, not by whether the
/// reorder has applied: a render call that holds the reorder splits there, and its frames before it
/// are still on the way down when the whole call is mixed.
fn dip_gain(dip: Option<Frame>, f: Frame, n: Frame) -> f32 {
    let Some(at) = dip else { return 1.0 };
    let k = (at - f).abs();
    if k >= n {
        1.0
    } else {
        k as f32 / n as f32
    }
}

/// The events one unit gets over a call into `ev`: a playing unit every NoteOn and wheel of the slot's
/// `events` (it holds a NoteOn's key from there) and a NoteOff only for a key it holds; a leaving one
/// (removed, withdrawn, replaced) none of them, only the releases of what it holds, at the call's first
/// frame, once. So a source change releases the old source's notes on the frame it applies (a sustained
/// one too: the router withholds its NoteOff), the new source starts holding none, and a NoteOff for a
/// key the new one never got is dropped (`docs/unit-contract.md` §4).
fn unit_events(ev: &mut Vec<SlotEvent>, held: &mut [u64; 2], events: &[SlotEvent], playing: bool) {
    ev.clear();
    let bit = |key: u8| ((key / 64) as usize, 1u64 << (key % 64));
    if playing {
        for e in events {
            match e.kind {
                SlotEventKind::NoteOn { key, .. } => {
                    let (w, b) = bit(key);
                    held[w] |= b;
                }
                SlotEventKind::NoteOff { key } => {
                    let (w, b) = bit(key);
                    if held[w] & b == 0 {
                        continue;
                    }
                    held[w] &= !b;
                }
                SlotEventKind::PitchBend { .. } | SlotEventKind::Modulation { .. } => {}
            }
            ev.push(*e);
        }
    } else {
        for key in 0..128u8 {
            let (w, b) = bit(key);
            if held[w] & b != 0 {
                ev.push(SlotEvent { offset: 0, kind: SlotEventKind::NoteOff { key } });
            }
        }
        *held = [0; 2];
    }
}

/// Call `unit` on the first `input[0].len()` frames of `out` (the events inside them), the rest of `out`
/// silent. A non-finite sample would stick in every filter, echo, limiter and loop after it: the whole
/// call's output goes silent, counted.
fn process_for(unit: &mut dyn UnitProcessor, cx: &UnitCx, input: [&[f32]; 2], events: &[SlotEvent], out: [&mut [f32]; 2], errors: &mut u64) {
    let k = input[0].len();
    let [ol, or] = out;
    if k > 0 {
        let inside = events.iter().take_while(|e| (e.offset as usize) < k).count();
        unit.process(cx, input, &events[..inside], [&mut ol[..k], &mut or[..k]]);
        if !ol[..k].iter().chain(&or[..k]).all(|y| y.is_finite()) {
            ol[..k].fill(0.0);
            or[..k].fill(0.0);
            *errors += 1;
        }
    }
    ol[k..].fill(0.0);
    or[k..].fill(0.0);
}

/// Render seats `seats` (the chain, physical order) over frames `a..b` of a render call: each unit
/// called once on its slice, then each seat's crossfades frame by frame, its output left in its `out`,
/// the chain's in `tail`. `to_bus` falls to the frame a leaving source head is gone.
#[allow(clippy::too_many_arguments)]
fn run_chain(seats: &mut [Seat], gated: [&[f32]; 2], zeros: &[f32], dry: &mut [Vec<f32>; 2], tail: &mut [Vec<f32>; 2], events: &[SlotEvent], cx: &UnitCx, (a, b): (usize, usize), n: u32, to_bus: &mut usize, errors: &mut u64) {
    // A move's reorder on a call's first frame leaves nothing before it: no unit is called for no frames.
    if a == b {
        return;
    }
    let seats_len = seats.len();
    for k in 0..seats_len {
        let (before, rest) = seats.split_at_mut(k);
        let seat = &mut rest[0];
        let seat_in: [&[f32]; 2] = if k == 0 { [&gated[0][a..b], &gated[1][a..b]] } else { [&before[k - 1].out[0][a..b], &before[k - 1].out[1][a..b]] };
        // A source takes no input.
        let source = seat.info.source;
        let x = if source { [&zeros[a..b], &zeros[a..b]] } else { seat_in };
        // A leaving unit is called for what is left of its fade and no frame past it (one, to carry its
        // releases, should its fade have none left): the frames after weigh it 0, and it is stopped once
        // the call ends.
        let left = |frames: u32, releases: bool| (frames as usize).max(releases as usize).min(b - a);
        let playing = !seat.removing && !seat.withdrawing;
        {
            let [ol, or] = &mut seat.out;
            match seat.unit.as_mut() {
                Some(unit) => {
                    unit_events(&mut seat.ev, &mut seat.held, events, playing);
                    let unit_len = match (seat.removing, seat.withdrawing) {
                        (false, false) => b - a,
                        (removing, withdrawing) => left(if removing { seat.fade } else { u32::MAX }.min(if withdrawing { seat.wet } else { u32::MAX }), !seat.ev.is_empty()),
                    };
                    process_for(unit.as_mut(), cx, [&x[0][..unit_len], &x[1][..unit_len]], &seat.ev, [&mut ol[a..b], &mut or[a..b]], errors);
                }
                None => {
                    ol[a..b].fill(0.0);
                    or[a..b].fill(0.0);
                }
            }
        }
        if let Some((_, _, old, _)) = seat.outgoing.as_mut() {
            let [ol, or] = &mut seat.old;
            unit_events(&mut seat.ev, &mut seat.old_held, events, false);
            let old_len = left(n - seat.swap, !seat.ev.is_empty());
            process_for(old.as_mut(), cx, [&x[0][..old_len], &x[1][..old_len]], &seat.ev, [&mut ol[a..b], &mut or[a..b]], errors);
        }
        let swapping = seat.outgoing.is_some();
        // What the seat outputs while it holds its unit, a withdrawal's fade included (the seat stays under
        // its id); a removal's fade is not (the id leaves with it).
        let meter_active = !seat.removing && seat.unit.is_some();
        let latency = seat.info.latency as usize;
        let old_latency = if swapping { seat.old_latency } else { latency };
        let cap = seat.line.len();
        for j in a..b {
            // What the seat sounds: the unit against its bypass, and mid-replacement the old unit against
            // its own bypass (at its latency, where its bypass crossfade stood) into that.
            let (core, old) = ([seat.out[0][j], seat.out[1][j]], [seat.old[0][j], seat.old[1][j]]);
            if source {
                // A source and its slot's input go different ways (the bus, the wet), so each fades
                // against silence there; once gone, the seat passes the gated input.
                let mut inner = [crossfade(0.0, core[0], seat.wet, n), crossfade(0.0, core[1], seat.wet, n)];
                if swapping {
                    inner = [0, 1].map(|c| crossfade(crossfade(0.0, old[c], seat.old_wet, n), inner[c], seat.swap, n));
                }
                if seat.removing && seat.fade == 0 {
                    *to_bus = (*to_bus).min(j);
                    (seat.out[0][j], seat.out[1][j]) = (gated[0][j], gated[1][j]);
                } else if k + 1 < seats_len {
                    // Seats after it: the input it fades against goes through them, never around them.
                    seat.out[0][j] = crossfade(gated[0][j], inner[0], seat.fade, n);
                    seat.out[1][j] = crossfade(gated[1][j], inner[1], seat.fade, n);
                    (dry[0][j], dry[1][j]) = (0.0, 0.0);
                } else {
                    seat.out[0][j] = crossfade(0.0, inner[0], seat.fade, n);
                    seat.out[1][j] = crossfade(0.0, inner[1], seat.fade, n);
                    dry[0][j] = crossfade(gated[0][j], 0.0, seat.fade, n);
                    dry[1][j] = crossfade(gated[1][j], 0.0, seat.fade, n);
                }
            } else {
                let input = [seat_in[0][j - a], seat_in[1][j - a]];
                // The bypass line runs every frame, bypassed or not and at any latency (0 included), so
                // a reload or replacement that raises the seat's latency reads real delayed input from
                // its first frame (A10), never the silence of an empty line.
                seat.line[seat.line_at] = input;
                let at = |l: usize| if l == 0 { input } else if seat.filled >= l { seat.line[(seat.line_at + cap - l) % cap] } else { [0.0; 2] };
                let delayed = [at(latency), at(old_latency)];
                seat.line_at = (seat.line_at + 1) % cap;
                seat.filled = (seat.filled + 1).min(cap);
                let mut inner = [crossfade(delayed[0][0], core[0], seat.wet, n), crossfade(delayed[0][1], core[1], seat.wet, n)];
                if swapping {
                    inner = [0, 1].map(|c| crossfade(crossfade(delayed[1][c], old[c], seat.old_wet, n), inner[c], seat.swap, n));
                }
                seat.out[0][j] = crossfade(input[0], inner[0], seat.fade, n);
                seat.out[1][j] = crossfade(input[1], inner[1], seat.fade, n);
            }
            if meter_active {
                seat.meter_out[0] = seat.meter_out[0].max(seat.out[0][j].abs());
                seat.meter_out[1] = seat.meter_out[1].max(seat.out[1][j].abs());
            }
            step(&mut seat.fade, seat.engaged, n);
            step(&mut seat.wet, seat.wet_on, n);
            if swapping && seat.swap < n {
                seat.swap += 1;
            }
        }
        if meter_active {
            seat.meter_frames = seat.meter_frames.saturating_add(b - a);
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

    /// The most frames any audio can take from the slot's input to the record tap now, given the record
    /// alignment `rec`, this slot's delay on the wet to the tap `dw` and the input side `side`: the
    /// whole chain's latency (a leaving or withdrawn seat's, and a replaced unit's if it is more) and
    /// the wet's delay; a chain that routes to the bus (a source head, leaving or not) also takes the
    /// record line, `side + rec` ([`Rack::render`]).
    fn reach(&self, rec: Frame, dw: usize, side: Frame) -> Frame {
        let span: Frame = self.seats[..self.len].iter().map(|s| s.info.latency.max(if s.outgoing.is_some() { s.old_latency as Frame } else { 0 })).sum();
        let wet = span + dw as Frame;
        if self.source_head() {
            wet.max(span).max(side + rec)
        } else {
            wet
        }
    }

    /// A seat takes notes: one holds a unit and is not leaving.
    fn plays(&self) -> bool {
        self.chain().any(|k| self.seats[k].unit.is_some())
    }

    /// A seat takes the wheels: one holds a SYNTH and is not leaving. Only such a chain is handed them
    /// (and only then do they split the slots' render), so a chain without one renders as before.
    fn takes_wheels(&self) -> bool {
        self.chain().any(|k| self.seats[k].unit.is_some() && self.seats[k].info.kind == UnitKind::Synth)
    }

    /// Units the slot holds: every seat's, and an install taken from its ring.
    fn units(&self) -> usize {
        self.seats[..self.len].iter().map(Seat::units).sum::<usize>() + self.taken.as_ref().is_some_and(|t| matches!(t.msg, SlotMsg::Install { .. })) as usize
    }

    /// A transition that indices or the role would shift under: a move's reorder, a replacement's
    /// crossfade, a withdrawal, a leaving source, or a leaving seat of a full chain (its place is
    /// free once it has gone, not before). The slot's messages wait for its end.
    fn busy(&self) -> bool {
        self.reorder.is_some()
            || self.seats[..self.len].iter().any(|s| s.outgoing.is_some() || s.withdrawing || (s.removing && (s.info.source || self.len == MAX_UNITS)))
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
        self.room(3) && (!matches!(msg, SlotMsg::Install { .. }) || self.end.tx.slots() > self.units())
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

    /// The places kept among the owed: a taken message's answers and chain (`Waiting` + terminal answer
    /// + chain = 3 before `Waiting`; terminal answer + chain = 2 once it answered `Waiting`), a move's
    /// answer and chain for its reorder, and an eviction's chain.
    fn kept(&self) -> usize {
        let taken = self.taken.as_ref().map_or(0, |t| if t.answered { 2 } else { 3 });
        taken + 2 * self.reorder.is_some() as usize + 1
    }

    /// Room among the owed for a port message's `n` beside the places kept and, while a bypass command
    /// on this slot waits for a place ([`Rack::reserve_bypasses`]), the first place freed, which is its.
    fn room(&self, n: usize) -> bool {
        self.owed.len() + self.kept() + self.reserved as usize + n <= OWED_CAPACITY
    }

    fn answer(&mut self, answer: Answer, errors: &mut u64) {
        owe(&mut self.owed, Owed::Seat(answer), errors);
    }

    /// Owe the chain as it now stands, on `frame`.
    fn owe_chain(&mut self, frame: Frame, errors: &mut u64) {
        let (len, seats) = self.summary();
        owe(&mut self.owed, Owed::Chain { frame, len, seats }, errors);
    }

    /// A move's reorder applied on `frame`: its answer, its ticket and its chain, owed there before any
    /// unit is called in the new order (a panic there unwinds past none of them). The fields alone, so
    /// a render call can owe them while it reads the slot's input.
    fn reordered(owed: &mut Vec<Owed>, end: &SlotEnd, (len, seats): (u8, [ChainSeat; MAX_UNITS]), id: UnitId, frame: Frame, errors: &mut u64) {
        owe(owed, Owed::Seat(Answer { frame, unit: id, op: SeatOp::Move, outcome: SeatOutcome::Applied }), errors);
        end.taken.fetch_add(1, Release);
        owe(owed, Owed::Chain { frame, len, seats }, errors);
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

    /// The seat at `k` leaves: its unit stopped while the seat still holds it, then handed back with
    /// `left` (`Left::Removed` or `Left::Evicted`), and the seats after it close up. A `stop` that panics
    /// unwinds with the unit left in its seat, for the owner's eviction (or the engine's drop, off the audio thread).
    fn finish_removal(&mut self, k: usize, left: Left, errors: &mut u64) {
        if let Some(unit) = self.seats[k].unit.as_mut() {
            unit.stop();
        }
        let seat = &mut self.seats[k];
        let left = if seat.evicting { Left::Evicted } else { left };
        if let Some(unit) = seat.unit.take() {
            let (id, token, bypassed) = (seat.id, seat.token, seat.bypassed);
            self.give_back(Back { id, token, unit, bypassed, left }, errors);
        }
        let seat = &mut self.seats[k];
        seat.removing = false;
        seat.evicting = false;
        seat.engaged = false;
        seat.fade = 0;
        seat.params_owed = [None; UnitParam::COUNT];
        seat.meter_out = [0.0; 2];
        seat.meter_frames = 0;
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
        // The seat stays under its id: what it output since its last publish waits for a same-id reseat
        // (a seat without its unit publishes nothing) and goes out with that unit's first range.
        if let Some(unit) = seat.unit.take() {
            let (id, token, bypassed) = (seat.id, seat.token, seat.bypassed);
            self.give_back(Back { id, token, unit, bypassed, left: Left::Withdrawn }, errors);
        }
    }

    /// The seat at `k`'s replaced unit leaves (the crossfade reached the new one): stopped, handed back.
    fn finish_swap(&mut self, k: usize, errors: &mut u64) {
        if let Some((_, _, old, _)) = self.seats[k].outgoing.as_mut() {
            old.stop();
        }
        if let Some((id, token, unit, bypassed)) = self.seats[k].outgoing.take() {
            self.give_back(Back { id, token, unit, bypassed, left: Left::Replaced }, errors);
        }
    }

    /// After a render call (or at once, idle): every transition that reached its end finishes.
    fn finish(&mut self, n: u32, removal_left: Left, errors: &mut u64) {
        let mut k = 0;
        while k < self.len {
            let seat = &self.seats[k];
            if seat.removing && seat.fade == 0 {
                self.finish_removal(k, removal_left, errors);
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

    /// An idle removal, withdrawal or replacement, or an eviction: no block will carry the releases of
    /// the units leaving, so one silent frame from `frame` (the next the engine would render) does, to
    /// each that holds a key, and a restarted unit holds no note. The slot's own events stay queued for
    /// the units that stay.
    fn flush(&mut self, cx: &UnitCx, zeros: &[f32]) {
        for seat in self.seats[..self.len].iter_mut() {
            let [l, r] = &mut seat.out;
            if seat.removing || seat.withdrawing {
                if let Some(unit) = seat.unit.as_mut() {
                    unit_events(&mut seat.ev, &mut seat.held, &[], false);
                    if !seat.ev.is_empty() {
                        unit.process(cx, [&zeros[..1], &zeros[..1]], &seat.ev, [&mut l[..1], &mut r[..1]]);
                    }
                }
            }
            if let Some((_, _, old, _)) = seat.outgoing.as_mut() {
                unit_events(&mut seat.ev, &mut seat.old_held, &[], false);
                if !seat.ev.is_empty() {
                    old.process(cx, [&zeros[..1], &zeros[..1]], &seat.ev, [&mut l[..1], &mut r[..1]]);
                }
            }
        }
    }

    /// No device runs: every transition under way ends at once (a move's reorder, a seat's leaving, a
    /// withdrawal, a replacement), the leaving units flushed, stopped and handed back.
    fn finish_idle(&mut self, cx: &UnitCx, zeros: &[f32], n: u32, removal_left: Left, errors: &mut u64) {
        if let Some((id, to)) = self.reorder.take() {
            self.apply_reorder(id, to);
            let summary = self.summary();
            Slot::reordered(&mut self.owed, &self.end, summary, id, cx.frame, errors);
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
        self.finish(n, removal_left, errors);
    }

    /// A move's reorder due by `frame` (its dip's silence on or before it) applies on `frame`, its answer
    /// and its chain owed there.
    fn reorder_due(&mut self, frame: Frame, errors: &mut u64) {
        if let (Some((id, to)), Some(at)) = (self.reorder, self.dip) {
            if at <= frame {
                self.reorder = None;
                self.apply_reorder(id, to);
                let summary = self.summary();
                Slot::reordered(&mut self.owed, &self.end, summary, id, frame, errors);
            }
        }
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

/// The engine's clock the units read ([`UnitCx`]): `skipped` is set once per block
/// ([`Rack::begin_block`]); `bpm` and `meter` follow [`Rack::set_clock`] as `SetBpm` and `SetMeter`
/// apply (splitting a block's slot render only while [`Rack::reads_tempo`] holds).
#[derive(Clone, Copy)]
pub(crate) struct UnitClock {
    /// Device frames the DSP clock is behind.
    pub(crate) skipped: Frame,
    pub(crate) bpm: f64,
    pub(crate) meter: Meter,
}

/// What a bypass command targets for owed-room checks and reservations (`engine.rs`).
#[derive(Clone, Copy)]
pub(crate) enum BypassTarget {
    Unit(UnitId),
    Kind(UnitKind),
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
    /// The slot [`Rack::offer`] offers first.
    offer_first: usize,
    /// Per-seat folded meters pushed at most every [`crate::UNIT_METER_MS`] when the ring has room
    /// (`EngineHandle::unit_meters`, invariant 2).
    meter_tx: Producer<UnitMeter>,
    /// Minimum rendered frames between consecutive publishes for one seat ([`unit_meter_frames`]).
    meter_interval: usize,
    /// Round-robin start index into `SLOT_COUNT * MAX_UNITS` seats for [`Rack::flush_meters`], so a
    /// ring with partial room never starves a later seat.
    meter_cursor: usize,
    pub(crate) events_dropped: u64,
    pub(crate) protocol_errors: u64,
    /// Unit commands naming an id no seat holds.
    pub(crate) unit_commands_dropped: u64,
    /// Param commands whose unit answered `None`.
    pub(crate) unit_params_dropped: u64,
}

impl Rack {
    /// Allocates: build it off the audio thread. Every record line holds `max_input` (the device's
    /// input side at most: `EngineConfig::max_input_frames`) plus [`max_chain_latency`] plus a block;
    /// every seat's bypass line [`max_chain_latency`] and one. The unit meter ring is allocated and
    /// every slot touched here so pushes in the callback never fault an untouched page.
    pub(crate) fn new(sample_rate: u32, max_block: usize, max_input: Frame, unit_meter_capacity: usize) -> (Rack, [SlotPort; SLOT_COUNT], Consumer<UnitMeter>) {
        let max_latency = max_chain_latency(sample_rate);
        let line = (max_input.max(0) + max_latency) as usize + max_block;
        let block = max_block.max(1);
        let (mut meter_tx, mut meter_rx) = RingBuffer::new(unit_meter_capacity);
        let empty_meter = UnitMeter { unit: 0, out: [0.0; 2], bands: None };
        while meter_tx.push(empty_meter).is_ok() {}
        while meter_rx.pop().is_ok() {}
        let mut ports = Vec::with_capacity(SLOT_COUNT);
        let slots = std::array::from_fn(|_| {
            let (port, end) = slot_channel();
            ports.push(port);
            Slot {
                end,
                seats: (0..MAX_UNITS)
                    .map(|_| Seat {
                        id: 0,
                        token: 0,
                        unit: None,
                        info: UnitInfo { kind: UnitKind::Plugin, source: false, latency: 0 },
                        fade: 0,
                        engaged: false,
                        removing: false,
                        evicting: false,
                        wet: 0,
                        wet_on: false,
                        bypassed: false,
                        withdrawing: false,
                        outgoing: None,
                        swap: 0,
                        old_kind: UnitKind::Plugin,
                        old_latency: 0,
                        old_wet: 0,
                        held: [0; 2],
                        old_held: [0; 2],
                        ev: Vec::with_capacity(MAX_SLOT_EVENTS + 128),
                        out: [vec![0.0; block], vec![0.0; block]],
                        old: [vec![0.0; block], vec![0.0; block]],
                        line: vec![[0.0; 2]; max_latency as usize + 1],
                        line_at: 0,
                        filled: 0,
                        params_owed: [None; UnitParam::COUNT],
                        meter_out: [0.0; 2],
                        meter_frames: 0,
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
                reach: 0,
                edit: 0,
                write: 0,
                ott: StereoOtt::new(sample_rate),
                owed: Vec::with_capacity(OWED_CAPACITY),
                reorder: None,
                reserved: false,
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
            offer_first: 0,
            meter_tx,
            meter_interval: unit_meter_frames(sample_rate),
            meter_cursor: 0,
            events_dropped: 0,
            protocol_errors: 0,
            unit_commands_dropped: 0,
            unit_params_dropped: 0,
        };
        (rack, ports, meter_rx)
    }

    fn unit_cx(&self, frame: Frame) -> UnitCx {
        UnitCx { frame, dsp_frame: frame - self.clock.skipped, bpm: self.clock.bpm, meter: self.clock.meter }
    }

    /// Whether `id` is spoken for in a slot other than `i`: seated there (leaving included), held there
    /// as an outgoing unit, or the install it has taken from its ring and not applied.
    fn claimed_elsewhere(&self, i: usize, id: UnitId) -> bool {
        self.slots.iter().enumerate().filter(|&(j, _)| j != i).any(|(_, s)| {
            s.seats[..s.len].iter().any(|seat| seat.id == id || seat.outgoing.as_ref().is_some_and(|&(x, ..)| x == id)) || s.taken.as_ref().is_some_and(|t| matches!(t.msg, SlotMsg::Install { id: x, .. } if x == id))
        })
    }

    /// Apply the ports' messages at `frame`, a block start (or, idle, the frame the next block would
    /// start at), each answered by an [`Event::Seat`] (a held message gets `Waiting`, then a second
    /// `Event::Seat` when the hold ends). `engaged` installs without a fade (nothing has sounded yet, or no
    /// device runs); `fade` off finishes every transition at once; `hold`: a capture holds the
    /// alignment, so an edit of a chain's sum or role waits.
    fn service(&mut self, frame: Frame, engaged: bool, fade: bool, hold: bool) {
        let cx = self.unit_cx(frame);
        let n = self.fade_frames;
        for i in 0..SLOT_COUNT {
            // A move's reorder due by this frame (a device jump past it) applies before the port is
            // served, so a message waiting behind it is taken in this block.
            if fade {
                self.slots[i].reorder_due(frame, &mut self.protocol_errors);
            }
            loop {
                if !fade {
                    let (s, zeros, errors) = (&mut self.slots[i], &self.zeros, &mut self.protocol_errors);
                    s.finish_idle(&cx, zeros, n, Left::Removed, errors);
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
                        // `holding` first: a host that sees the answer must see the hold with it.
                        s.end.holding.store(true, Release);
                        if !answered {
                            t.answered = true;
                            s.answer(Answer { frame, unit, op, outcome: SeatOutcome::Waiting }, &mut self.protocol_errors);
                            s.end.taken.fetch_add(1, Release);
                        }
                        break;
                    }
                    Verdict::Wait => break,
                    Verdict::Apply { .. } => match self.apply(i, frame, engaged, fade) {
                        Some(changed) => (SeatOutcome::Applied, changed),
                        // A move's dip: answered on its reorder ([`Slot::reordered`]).
                        None => (SeatOutcome::Waiting, false),
                    },
                    Verdict::Refuse(why) => {
                        let s = &mut self.slots[i];
                        if let Some(Taken { msg: SlotMsg::Install { id, token, bypassed, unit, .. }, .. }) = s.taken.take() {
                            s.give_back(Back { id, token, unit, bypassed, left: Left::Refused(why) }, &mut self.protocol_errors);
                        }
                        (SeatOutcome::Refused(why), false)
                    }
                };
                let (outcome, changed) = outcome;
                let s = &mut self.slots[i];
                s.taken = None;
                s.end.holding.store(false, Release);
                // Every non-Waiting outcome gets an `Event::Seat` — including a held (`Waiting`)
                // message's terminal `Applied` or `Refused` when the hold ends (its ticket was already
                // answered with `Waiting`). A move that dips is answered on its reorder.
                if !matches!(outcome, SeatOutcome::Waiting) {
                    s.answer(Answer { frame, unit, op, outcome }, &mut self.protocol_errors);
                    if !answered {
                        s.end.taken.fetch_add(1, Release);
                    }
                }
                if changed {
                    s.owe_chain(frame, &mut self.protocol_errors);
                }
            }
            if !fade {
                let (s, zeros, errors) = (&mut self.slots[i], &self.zeros, &mut self.protocol_errors);
                s.finish_idle(&cx, zeros, n, Left::Removed, errors);
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
                // A leaving seat with this id still holds its unit: a new one waits for no seat of its,
                // whichever seat it goes to (a slot with an outgoing unit takes no message).
                if s.seats[..s.len].iter().enumerate().any(|(k, seat)| into != Some(k) && seat.id == id) {
                    return Verdict::Refuse(SeatRefusal::Id);
                }
                let (t, before) = match into {
                    Some(k) => (s.chain().position(|p| p == k).unwrap_or(0), s.seats[k].info.latency),
                    None if s.len == MAX_UNITS => return Verdict::Refuse(SeatRefusal::Full),
                    None if info.source => (0, 0),
                    None => (at.min(s.chain_len()), 0),
                };
                // A replacement keeps its seat's role: the crossfade and the seat's line assume it.
                if replaces.is_some() && into.is_some_and(|k| s.seats[k].info.source != info.source) {
                    return Verdict::Refuse(SeatRefusal::Placement);
                }
                // A source only at the head (one there is replaced only when named); no effect ahead of
                // a source.
                let head_source = role && into.is_none_or(|k| s.chain().next() != Some(k));
                if (info.source && t > 0) || (info.source && into.is_none() && role) || (!info.source && t == 0 && head_source) {
                    return Verdict::Refuse(SeatRefusal::Placement);
                }
                // Against the budget first: the sum itself could overflow on a port's latency.
                if info.latency > max_latency - (sum - before) {
                    return Verdict::Refuse(SeatRefusal::Latency);
                }
                let after = sum - before + info.latency;
                let new_role = if t == 0 { info.source } else { role };
                // A source lands at physical index 0, the head's record path: one whose seat would sit
                // behind a leaving seat (a new source, or a role-changing reseat of the logical head)
                // waits for it to have gone.
                if info.source && into.unwrap_or_else(|| s.physical(0)) > 0 {
                    return Verdict::Wait;
                }
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

    /// Apply slot `i`'s taken message, which [`Rack::judge`] passed, at `frame`: whether the chain
    /// changed, or `None` for a move that dips first (its answer and its chain come on its reorder).
    fn apply(&mut self, i: usize, frame: Frame, engaged: bool, fade: bool) -> Option<bool> {
        let n = self.fade_frames;
        let rendered = self.rendered;
        let cx = self.unit_cx(frame);
        let s = &mut self.slots[i];
        // An install's bypass is set while the slot still holds the message: a unit that panics there
        // leaves the chain as it was, and the message (its info unread) is refused `Unknown` and goes
        // back, answered once. A same-id reseat or reload keeps its seat's latest bypass.
        let mut install_bypass = None;
        if let Some(&SlotMsg::Install { id, bypassed, replaces, .. }) = s.taken.as_ref().map(|t| &t.msg) {
            let kept = replaces.and_then(|r| s.find(r)).or_else(|| s.find(id)).filter(|&k| s.seats[k].id == id).map(|k| s.seats[k].bypassed);
            let target = kept.unwrap_or(bypassed);
            let mut handled = false;
            if let Some(t) = s.taken.as_mut() {
                let info = t.info.take();
                if let (SlotMsg::Install { unit, .. }, true) = (&mut t.msg, target || target != bypassed) {
                    handled = unit.set_bypass(&cx, target);
                }
                t.info = info;
            }
            install_bypass = Some((target, handled));
        }
        let Some(t) = s.taken.take() else { return Some(false) };
        match t.msg {
            SlotMsg::Install { id, token, at, replaces, unit, .. } => {
                let (bypassed, handled) = install_bypass.unwrap_or_default();
                let info = t.info.unwrap_or(UnitInfo { kind: UnitKind::Plugin, source: false, latency: 0 });
                let into = replaces.and_then(|r| s.find(r)).or_else(|| s.find(id));
                let k = match into {
                    Some(k) => {
                        // The old unit crossfades out of the seat (at once, idle) and goes back, taking
                        // the keys it holds with it (released as it leaves); the new one holds none.
                        let seat = &mut s.seats[k];
                        if let Some(old) = seat.unit.take() {
                            seat.outgoing = Some((seat.id, seat.token, old, seat.bypassed));
                            seat.swap = if fade { 0 } else { n };
                            (seat.old_kind, seat.old_latency, seat.old_wet) = (seat.info.kind, seat.info.latency as usize, seat.wet);
                            seat.old_held = seat.held;
                        }
                        seat.held = [0; 2];
                        // The line holds this seat's input while it was an effect, at any latency: a
                        // role change starts it empty.
                        let seat = &mut s.seats[k];
                        if seat.info.source != info.source {
                            seat.filled = 0;
                        }
                        let same_id = seat.id == id;
                        (seat.id, seat.token, seat.info, seat.unit) = (id, token, info, Some(unit));
                        // A same-id replacement (a reload, a reseat after a withdrawal) keeps what the seat
                        // output since its last meter publish: that range reaches the ring under the id
                        // with the new unit's first; only the old unit's own `take_meter` fold leaves with
                        // it. Another id starts its range here.
                        if !same_id {
                            seat.params_owed = [None; UnitParam::COUNT];
                            seat.meter_out = [0.0; 2];
                            seat.meter_frames = 0;
                        }
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
                        (seat.id, seat.token, seat.info, seat.unit) = (id, token, info, Some(unit));
                        (seat.removing, seat.evicting, seat.engaged, seat.fade) = (false, false, true, if engaged { n } else { 0 });
                        (seat.withdrawing, seat.outgoing, seat.swap) = (false, None, n);
                        (seat.line_at, seat.filled, seat.held) = (0, 0, [0; 2]);
                        seat.params_owed = [None; UnitParam::COUNT];
                        seat.meter_out = [0.0; 2];
                        seat.meter_frames = 0;
                        s.len += 1;
                        s.seats[p..=k].rotate_right(1);
                        p
                    }
                };
                let seat = &mut s.seats[k];
                seat.bypassed = bypassed;
                seat.wet_on = !bypassed || handled;
                // A new seat starts at its bypass (its lifecycle fade carries it in), and so does a
                // replacement (the swap carries it in from the old unit's); a reseat fades in from its
                // bypass.
                if into.is_none() || !fade || !rendered || seat.outgoing.is_some() {
                    seat.wet = if seat.wet_on { n } else { 0 };
                }
            }
            SlotMsg::Remove { id } => {
                let Some(k) = s.find(id) else { return Some(false) };
                // Its unit gets the releases of what it holds as it leaves; the others keep theirs.
                let seat = &mut s.seats[k];
                seat.removing = true;
                seat.engaged = false;
                if !fade {
                    seat.fade = 0;
                }
            }
            SlotMsg::Withdraw { id } => {
                let Some(k) = s.find(id) else { return Some(false) };
                // Its unit gets the releases of what it holds as it leaves; the seat fades to its bypass.
                let seat = &mut s.seats[k];
                seat.withdrawing = true;
                seat.wet_on = false;
                if !fade {
                    seat.wet = 0;
                }
            }
            SlotMsg::Move { id, to } => {
                let Some(k) = s.find(id) else { return Some(false) };
                let to = to.min(s.chain_len() - 1);
                if s.chain().position(|p| p == k) == Some(to) {
                    return Some(false);
                }
                if !fade || !rendered {
                    s.apply_reorder(id, to);
                } else {
                    // Down from where the dip is now (rising after an earlier move, or level) to silence.
                    let up = s.dip.map_or(n as Frame, |at| (frame - at).clamp(0, n as Frame));
                    s.dip = Some(frame + up);
                    if up > 0 {
                        s.reorder = Some((id, to));
                        // Down to silence, then back up: two fades of the chain's output.
                        s.edit = s.edit.max(2 * n as Frame);
                        return None;
                    }
                    // Silent already: the reorder lands on this frame.
                    s.apply_reorder(id, to);
                }
            }
        }
        if s.chain().next().is_some() {
            s.line_latency = s.latency();
        }
        s.edit = s.edit.max(n as Frame);
        Some(true)
    }

    /// Apply bypass `b` to `seat` on frame `cx.frame`, calling `set_bypass` on its unit when live.
    fn apply_seat_bypass(seat: &mut Seat, b: bool, cx: &UnitCx, n: u32, rendered: bool) {
        seat.bypassed = b;
        if !seat.withdrawing {
            if let Some(unit) = seat.unit.as_mut() {
                let handled = unit.set_bypass(cx, b);
                seat.wet_on = !b || handled;
                if !rendered {
                    seat.wet = if seat.wet_on { n } else { 0 };
                }
            }
        }
    }

    /// The bypass of unit `id` at `now`: `Some` outright, `None` switched from the engine's value. On a
    /// seat that holds its unit, through [`UnitProcessor::set_bypass`]; a withdrawn seat keeps the value
    /// (a same-id reseat keeps it). Answered by [`Event::Chain`]; an id no seat holds is dropped and
    /// counted. The engine applies it only while its chain has a place ([`Rack::bypass_room`]).
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
        Self::apply_seat_bypass(seat, b, &cx, n, rendered);
        s.owe_chain(now, &mut self.protocol_errors);
    }

    /// Whether any seated unit (non-removing) of `kind` exists in either slot.
    pub(crate) fn has_unit_kind(&self, kind: UnitKind) -> bool {
        self.slots.iter().any(|s| s.chain().any(|k| s.seats[k].info.kind == kind))
    }

    /// Whether any seated unit (non-removing) of `kind` in either slot is currently on (not bypassed).
    pub(crate) fn any_unit_on(&self, kind: UnitKind) -> bool {
        self.slots.iter().any(|s| {
            s.chain().any(|k| {
                let seat = &s.seats[k];
                seat.info.kind == kind && !seat.bypassed
            })
        })
    }

    /// [`crate::Toggle::Units`]' any-on rule (`docs/unit-contract.md` §6): across both slots, every
    /// seated unit of built-in effect `kind` switches — if any is on, all go bypassed (`Some(false)`),
    /// else all come on (`Some(true)`), owing one [`Event::Chain`] per slot whose chain changed.
    /// Returns `None` when `kind` is not a built-in effect or no unit of `kind` is seated (does nothing).
    pub(crate) fn toggle_units(&mut self, kind: UnitKind, now: Frame) -> Option<bool> {
        if !kind.is_builtin_effect() {
            return None;
        }
        let mut any_seated = false;
        let mut any_on = false;
        for s in &self.slots {
            for k in s.chain() {
                let seat = &s.seats[k];
                if seat.info.kind == kind {
                    any_seated = true;
                    if !seat.bypassed {
                        any_on = true;
                    }
                }
            }
        }
        if !any_seated {
            return None;
        }
        let target_bypassed = any_on;
        let (n, rendered, cx) = (self.fade_frames, self.rendered, self.unit_cx(now));
        for s in &mut self.slots {
            let mut changed = false;
            for k in 0..s.len {
                let seat = &mut s.seats[k];
                if seat.removing || seat.info.kind != kind || seat.bypassed == target_bypassed {
                    continue;
                }
                Self::apply_seat_bypass(seat, target_bypassed, &cx, n, rendered);
                changed = true;
            }
            if changed {
                s.owe_chain(now, &mut self.protocol_errors);
            }
        }
        Some(!target_bypassed)
    }

    /// Apply `param` = `value` to unit `id` at `now` through [`UnitProcessor::set_param`]. Answered by
    /// [`Event::UnitParam`] with the applied value, kept per seat and param until the event ring takes
    /// it (a later value for the same param overwrites it). An unseated id (or a withdrawing/withdrawn
    /// seat) is dropped and counted on `unit_commands_dropped`; a param the unit answers `None` to is
    /// dropped and counted on `unit_params_dropped`.
    pub(crate) fn set_unit_param(&mut self, id: UnitId, param: UnitParam, value: f64, now: Frame) {
        let cx = self.unit_cx(now);
        let found = (0..SLOT_COUNT).find_map(|i| self.slots[i].find(id).map(|k| (i, k)));
        let Some((i, k)) = found else {
            self.unit_commands_dropped += 1;
            return;
        };
        let seat = &mut self.slots[i].seats[k];
        if seat.withdrawing {
            self.unit_commands_dropped += 1;
            return;
        }
        let Some(unit) = seat.unit.as_mut() else {
            self.unit_commands_dropped += 1;
            return;
        };
        match unit.set_param(&cx, param, value) {
            Some(applied) => seat.params_owed[param.index()] = Some((now, applied)),
            None => self.unit_params_dropped += 1,
        }
    }

    /// The owed [`Event::UnitParam`] answers: the last value each active seat's unit applied that the
    /// event ring has not taken yet. Not for the audio thread.
    pub(crate) fn unsent_unit_params(&self) -> impl Iterator<Item = (UnitId, UnitParam, f64)> + '_ {
        self.slots.iter().flat_map(|s| {
            s.chain().flat_map(move |k| {
                let seat = &s.seats[k];
                let unit = seat.id;
                UnitParam::ALL
                    .into_iter()
                    .zip(seat.params_owed.iter())
                    .filter_map(move |(param, cell)| cell.map(|(_, value)| (unit, param, value)))
            })
        })
    }

    /// Whether a bypass command (`SetUnitBypass`, `ToggleUnit`, `Toggle::Units`) can apply now: every
    /// slot whose chain would change has a place among what it owes (an unseated id or kind needs none).
    /// One that cannot waits, with every bypass command sent after it (the engine's table), so none is
    /// folded or lost.
    pub(crate) fn bypass_room(&self, target: BypassTarget) -> bool {
        match target {
            BypassTarget::Unit(id) => {
                self.slots.iter().find(|s| s.find(id).is_some()).is_none_or(|s| s.owed.len() + s.kept() < OWED_CAPACITY)
            }
            BypassTarget::Kind(kind) => {
                if !kind.is_builtin_effect() {
                    return true;
                }
                let target_bypassed = self.any_unit_on(kind);
                self.slots.iter().all(|s| {
                    let would_change = s.chain().any(|k| {
                        let seat = &s.seats[k];
                        seat.info.kind == kind && seat.bypassed != target_bypassed
                    });
                    !would_change || s.owed.len() + s.kept() < OWED_CAPACITY
                })
            }
        }
    }

    /// The bypass commands that wait for a place among what their slot owes (the engine's table, oldest
    /// first among them): each such slot keeps the next place freed for the oldest, ahead of its port's
    /// next message, so a port edit every block never takes the places a waiting bypass needs. Set
    /// before the ports are served.
    pub(crate) fn reserve_bypasses(&mut self, waiting: impl Iterator<Item = BypassTarget>) {
        for s in self.slots.iter_mut() {
            s.reserved = false;
        }
        for target in waiting {
            match target {
                BypassTarget::Unit(id) => {
                    if let Some(s) = self.slots.iter_mut().find(|s| s.find(id).is_some()) {
                        s.reserved = true;
                    }
                }
                BypassTarget::Kind(kind) => {
                    for s in self.slots.iter_mut() {
                        if s.chain().any(|k| s.seats[k].info.kind == kind) {
                            s.reserved = true;
                        }
                    }
                }
            }
        }
    }

    /// What each slot owes the event ring, each slot's in order until the ring refuses one. The slots
    /// take turns at the first offer (the one after the last that got a place goes first), so a slot
    /// that owes something every block starves no other. The order is per slot only: a turn can put a
    /// newer reply of one slot ahead of an older one of the other ([`Event::Seat`]).
    pub(crate) fn offer(&mut self, feed: &mut Feed) {
        let first = self.offer_first;
        for i in (0..SLOT_COUNT).map(|k| (first + k) % SLOT_COUNT) {
            let s = &mut self.slots[i];
            let slot = i as u8;
            let taken = s
                .owed
                .iter()
                .take_while(|o| {
                    feed.push(match **o {
                        Owed::Seat(a) => Event::Seat { frame: a.frame, slot, unit: a.unit, op: a.op, outcome: a.outcome },
                        Owed::Chain { frame, len, seats } => Event::Chain { frame, slot, len, seats },
                    })
                })
                .count();
            s.owed.drain(..taken);
            let mut moved = taken > 0;
            if s.owed.is_empty() {
                'params: for seat in &mut s.seats[..s.len] {
                    if seat.removing {
                        continue;
                    }
                    let unit = seat.id;
                    for (param, cell) in UnitParam::ALL.into_iter().zip(seat.params_owed.iter_mut()) {
                        let Some((frame, value)) = *cell else { continue };
                        if feed.push(Event::UnitParam { frame, unit, param, value }) {
                            *cell = None;
                            moved = true;
                        } else {
                            break 'params;
                        }
                    }
                }
            }
            if moved {
                self.offer_first = (i + 1) % SLOT_COUNT;
            }
        }
    }

    /// Inside a block, before the commands due on `now`: a move's reorder due on it applies first, as at
    /// a block start ([`Rack::service`]), so a slot command on a reorder's exact frame answers after the
    /// move at any block size. Only where the slots' render stopped (a slot command's frame): a render
    /// that passed a reorder's frame applied it there, and one that has not reached it yet will.
    pub(crate) fn reorders_due(&mut self, now: Frame) {
        if now != self.cursor {
            return;
        }
        for s in self.slots.iter_mut() {
            s.reorder_due(now, &mut self.protocol_errors);
        }
    }

    pub(crate) fn set_clock(&mut self, bpm: f64, meter: Meter) {
        self.clock.bpm = bpm;
        self.clock.meter = meter;
    }

    /// Whether any seated unit (leaving or outgoing included) reads the engine's tempo (`UnitCx::bpm`
    /// / `UnitCx::meter`): a built-in ECHO today. Only then do `SetBpm` and `SetMeter` split the
    /// slots' render range mid-block.
    pub(crate) fn reads_tempo(&self) -> bool {
        self.slots.iter().any(|s| {
            s.seats[..s.len].iter().any(|seat| {
                (seat.unit.is_some() && seat.info.kind == UnitKind::Echo)
                    || (seat.outgoing.is_some() && seat.old_kind == UnitKind::Echo)
            })
        })
    }

    /// At a block start: service the ports (unless `serve` is off: a capture's end may be due on this
    /// frame, and [`Rack::release_hold`] serves them once it is resolved) and start the render cursor.
    /// `hold`: a capture holds the alignment.
    pub(crate) fn begin_block(&mut self, start: Frame, first: bool, clock: UnitClock, hold: bool, serve: bool) {
        self.clock = clock;
        if serve {
            self.service(start, first, true, hold);
        }
        self.cursor = start;
    }

    /// At block start `start`, after that frame's looper events resolved the capture that held the
    /// alignment there: the ports' messages apply now, `hold` if a capture still holds it (a RETAKE pass
    /// or a handoff took it on).
    pub(crate) fn release_hold(&mut self, start: Frame, first: bool, hold: bool) {
        self.service(start, first, true, hold);
    }

    /// With no device running (the host holds the engine): apply the ports' messages at once, at `frame`
    /// (the next the engine would render), no fades. A leaving unit stops here and goes straight back.
    pub(crate) fn service_idle(&mut self, frame: Frame, hold: bool) {
        self.cursor = frame;
        self.service(frame, true, false, hold);
    }

    /// Stop every unit and hand it back with its bypass (a sample-rate change: the host re-activates
    /// and reinstalls), with every install still waiting on a port that the return ring has room for,
    /// however full what its slot owes the event ring is. It services the ports idle first, so any
    /// queued or held install with reply room applies before the eviction: a same-id reload queued
    /// behind its seat comes back as two boxes, the old [`Left::Replaced`] and the new
    /// [`Left::Evicted`]; only an install that found no place for its answer comes back [`Left::Unsent`],
    /// unstarted and unanswered (its return is its answer; its ticket is answered), and the messages
    /// ahead of it leave with no effect (the slot seats nothing now). The device side settles by these
    /// labels (`Evicted`: reinstall; `Replaced`: tear down; `Unsent`: submit again), so both paths end
    /// with the reload seated in the new engine. The host reads its returns and evicts again until none
    /// comes back, so nothing is left for the engine's drop.
    pub(crate) fn evict(&mut self, frame: Frame) {
        self.service_idle(frame, false);
        let (cx, n) = (self.unit_cx(frame), self.fade_frames);
        for s in self.slots.iter_mut() {
            if s.len > 0 {
                s.release_all(0, &mut self.events_dropped);
                s.edit = s.edit.max(n as Frame);
                for seat in s.seats[..s.len].iter_mut() {
                    seat.removing = true;
                    seat.evicting = true;
                    seat.engaged = false;
                    seat.fade = 0;
                }
                s.finish_idle(&cx, &self.zeros, n, Left::Evicted, &mut self.protocol_errors);
                s.owe_chain(frame, &mut self.protocol_errors);
            }
            // Each unit's return is published before its ticket is answered, so a host that sees the
            // answer finds the unit on the port.
            if let Some(t) = s.taken.take() {
                if let SlotMsg::Install { id, token, bypassed, unit, .. } = t.msg {
                    s.give_back(Back { id, token, unit, bypassed, left: Left::Unsent }, &mut self.protocol_errors);
                }
                if !t.answered {
                    s.end.taken.fetch_add(1, Release);
                }
                s.end.holding.store(false, Release);
            }
            while let Ok(msg) = s.end.rx.peek() {
                if matches!(msg, SlotMsg::Install { .. }) && s.end.tx.slots() == 0 {
                    break;
                }
                let Ok(msg) = s.end.rx.pop() else { break };
                if let SlotMsg::Install { id, token, bypassed, unit, .. } = msg {
                    s.give_back(Back { id, token, unit, bypassed, left: Left::Unsent }, &mut self.protocol_errors);
                }
                s.end.taken.fetch_add(1, Release);
            }
        }
    }

    /// The largest latency of a live input-fed chain now: what [`Rack::latch_record`] takes while open.
    pub(crate) fn live_latency(&self) -> Frame {
        self.slots.iter().filter(|s| s.live && s.shut == 0 && !s.source_chain()).map(Slot::latency).max().unwrap_or(0)
    }

    /// At a block start `now`, after the ports were serviced: while `open`, the alignment follows the
    /// slots (the live latency); shut, it holds what it last took. The engine shuts it while a capture's
    /// alignment is fixed, which took the same live latency, and no chain's sum or role changes then
    /// (an edit that would waits: [`Rack::service`]). Either way each slot's wet is delayed by what its
    /// own chain's latency lacks of that alignment. The audio itself is left as it was, so nothing on
    /// the play path (the input sends take the aligned wet) drops out.
    ///
    /// The record tap's timing changed when a chain was edited since the last latch (a seat installed,
    /// removed, replaced, reseated, withdrawn or moved; a bypass does not: it keeps the seat's
    /// latency) or the alignment changed. Audio rendered at the old timing then still arrives for a
    /// while, so this is one conservative rule and no per-path arithmetic: it returns the frame from
    /// which the tap is all at the new timing, the edit's crossfades (the fade's frames, a move's two)
    /// after `now` plus the most any path to the tap can take at the old or the new timing
    /// ([`Slot::reach`], the old one as last latched). Equal sums, latency downstream of an edit and
    /// crossfades that overlap lie inside that bound; a zero-latency edit blinds it for its fade, which
    /// is accepted.
    pub(crate) fn latch_record(&mut self, open: bool, side: Frame, now: Frame) -> Option<Frame> {
        let before = self.record_latency;
        if open {
            self.record_latency = self.live_latency();
        }
        let rec = self.record_latency;
        let mut run = if rec != before { self.fade_frames as Frame } else { 0 };
        let mut reach = 0;
        for s in self.slots.iter_mut() {
            let own = if !s.source_chain() { s.latency() } else { 0 };
            s.dw = (rec - own).clamp(0, (s.wet_line.len() - 1) as Frame) as usize;
            let now_reach = s.reach(rec, s.dw, side);
            reach = reach.max(s.reach).max(now_reach);
            s.reach = now_reach;
            run = run.max(std::mem::take(&mut s.edit));
        }
        (run > 0).then_some(now + run + reach)
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

    /// Whether the note target's chain takes the wheels now (it seats a SYNTH): only then is a wheel
    /// command a slot command that splits the slots' render (`engine.rs` `first_slot_pending`).
    pub(crate) fn takes_wheels(&self) -> bool {
        self.target.is_some_and(|i| self.slots[i].takes_wheels())
    }

    /// A wheel ([`SlotEventKind::PitchBend`] or [`SlotEventKind::Modulation`]) for the note target's
    /// chain on `now`, as a note goes, while it seats a SYNTH; nowhere otherwise.
    pub(crate) fn wheel(&mut self, kind: SlotEventKind, now: Frame) {
        let offset = self.offset(now);
        let Some(s) = self.target.map(|i| &mut self.slots[i]) else { return };
        if s.takes_wheels() {
            s.queue(kind, offset, &mut self.events_dropped);
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
            // One the device jumped past (it never delivered the reorder's frame) applies on this call's
            // first frame, the dip rising on from where the device clock puts it.
            let split = match (s.reorder, s.dip) {
                (Some(_), Some(at)) if at < frame + m as Frame => (at - frame).max(0) as usize,
                _ => m,
            };
            let len = s.len;
            let first = s.events.iter().take_while(|e| (e.offset as usize) < split).count();
            run_chain(&mut s.seats[..len], gated, zeros, &mut s.dry, &mut s.tail, &s.events[..first], &self_cx(&self.clock, frame), (0, split), fade_frames, &mut to_bus, &mut self.protocol_errors);
            if split < m {
                if let Some((id, to)) = s.reorder.take() {
                    reorder(&mut s.seats[..len], id, to);
                    let summary = s.summary();
                    Slot::reordered(&mut s.owed, &s.end, summary, id, frame + split as Frame, &mut self.protocol_errors);
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
                    let g = dip_gain(s.dip, frame + j as Frame, fade_frames as Frame);
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
            s.finish(fade_frames, Left::Removed, &mut self.protocol_errors);
        }
        for (r, (&l, &rr)) in record.iter_mut().zip(al_l.iter().zip(al_r.iter())) {
            r[0] += l;
            r[1] += rr;
        }
        self.flush_meters();
        self.cursor += m as Frame;
    }

    /// Move each seated unit's folded meter onto `meter_tx` once at least `meter_interval` frames
    /// (~4 ms) have accumulated, calling `take_meter` only while the ring has room so a full ring
    /// delays a range and never drops or loses samples (`docs/unit-contract.md` §2). Never allocates,
    /// locks or blocks (invariant 5).
    fn flush_meters(&mut self) {
        let total = SLOT_COUNT * MAX_UNITS;
        let start = self.meter_cursor;
        let mut next_cursor = start;
        for offset in 0..total {
            if self.meter_tx.slots() == 0 {
                break;
            }
            let idx = (start + offset) % total;
            let slot_idx = idx / MAX_UNITS;
            let seat_idx = idx % MAX_UNITS;
            let s = &mut self.slots[slot_idx];
            if seat_idx >= s.len {
                continue;
            }
            let seat = &mut s.seats[seat_idx];
            if seat.removing || seat.withdrawing || seat.meter_frames < self.meter_interval {
                continue;
            }
            let Some(unit) = seat.unit.as_mut() else {
                continue;
            };
            let mut bands = [BandMeter::default(); 3];
            let has_bands = unit.take_meter(&mut bands);
            let meter = UnitMeter {
                unit: seat.id,
                out: seat.meter_out,
                bands: has_bands.then_some(bands),
            };
            if self.meter_tx.push(meter).is_ok() {
                seat.meter_out = [0.0; 2];
                seat.meter_frames = 0;
                next_cursor = (idx + 1) % total;
            }
        }
        self.meter_cursor = next_cursor;
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
        let (mut rack, mut ports, _meters) = Rack::new(48000, 64, 0, crate::api::UNIT_METER_CAPACITY);
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
