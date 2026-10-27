//! OWNS: the plugin slots inside the callback: the units the host installs and takes back through each
//! slot's [`SlotPort`], their bypass crossfades, the notes a slot holds, the live flag and gain, and
//! where a slot's output goes. The processors themselves are the host's (`src-tauri/src/host`, which
//! implements [`SlotProcessor`] for CLAP, VST3 and VST2).
//!
//! Each slot has its own input (the device side picks its capture channel). A slot holding an effect
//! (or nothing) passes its input on while it is live and gets silence otherwise, through its live gate:
//! a linear ramp over the frames of `LIVE_SECONDS` from the command's frame (STATUS D23), on the input
//! alone, so an effect's own tail rings out after it goes off. Its output is the wet
//! signal, which the engine hears after the limiter at once and records at the take's alignment: the
//! largest live effect's latency, so a live slot with less (an empty one, a quicker effect) reaches the
//! record tap that much later and two live inputs land together. That record compensation is latched
//! ([`Rack::latch_record`]): it holds while a capture runs, so a slot going live or not, or a unit
//! coming or going, never moves what records mid-take (a slot off live feeds its line silence, and what
//! was on the way drains in place). A slot holding an instrument plays the notes while it is the note
//! target; its
//! output joins the master bus and goes to the record tap delayed like a built-in instrument's, less the
//! plugin's own latency (`instruments`). Outputs are mono, as today's bridge is.
//!
//! Lifecycle never stops the audio. The host builds and activates a unit off the audio thread and
//! installs it: the unit crossfades in from bypass (its slot's gated input, what an empty slot passes)
//! over the frames of `FADE_SECONDS`, counted, so both ends land on an exact frame and are bit-exact. An
//! instrument's output joins the bus, not the wet, so there it fades in from silence while the gated
//! input fades out of the wet over the same frames (and back at its removal): a live slot's dry signal
//! never steps. A removal names the unit by the tag it was sent with (a unit already leaving, being
//! removed or fading out after a swap, gets no answer; any other tag is answered `NotHeld` and changes
//! nothing), first releases the notes the slot holds and crossfades to bypass;
//! the slot is empty from the frame the fade ends. The release reaches the unit in the fade's first
//! `process`, [`SlotProcessor::stop`] follows its last, and the unit goes back on the port, tagged and
//! with why (`port`: a full return ring parks it behind the replies before it). With no device running
//! the ports are serviced at once, without fades, and one silent frame of `process` carries a removed
//! unit's release. Nothing here drops a unit: an eviction hands back whatever either seat holds and an
//! install still waiting on a port, and a unit's methods run only while a seat of its slot holds it, so
//! a panic the callback's guard catches unwinds past no unit held as a local. Installing into an
//! occupied slot is a protocol error: the new unit goes straight back `Occupied`, counted, never started
//! or stopped. So is a call whose output holds a NaN or an infinity: that call's output is silenced, so
//! nothing downstream (the mix, the input sends, the limiter, a capture) ever sees it.
//!
//! A replace ([`SlotPort::replace`], `swap`) changes the unit a slot plays without a gap. The candidate
//! takes the slot's `side` seat and stages there: it processes the slot's input (an instrument silence),
//! unheard, for its latency plus `STAGE_SECONDS` of frames, while a gain change applies to the playing
//! unit. At the first block start after that it commits (`Committed`): the playing unit's notes are
//! released (a later release of one reaches neither unit), the two seats trade places, and over the frames
//! of `SWAP_SECONDS`, counted from the commit frame, the unit it replaced crossfades out at the slot's gain
//! as it stood, under the new one at the replace's gain, the slot's from then on. Each unit's term stays on
//! its own destination (an effect's on the wet, an instrument's on the bus), and the slot's bypass
//! crossfade applies over them as ever. A commit that changes the kind (effect ↔ instrument) takes a live
//! slot off live at its frame, as a GO LIVE off would, answered by `Event::CommitLiveOff`. The old unit stops after
//! its last call and goes back `ReplacedOut`, behind the `Committed`.
//! While a take captures, a candidate whose latency differs from the playing unit's (an empty slot's
//! is 0) stays staged until the capture ends, so nothing moves what records mid-take. A candidate that
//! refuses to start goes back `StartRefused` after the call it refused in, and a withdraw of its tag
//! (or a remove of the playing unit) sends it back `Cancelled`; the playing unit plays on. A withdraw
//! names only a staging candidate: any other tag, a committed unit's included, gets no answer and
//! changes nothing, so one that loses the race to the commit never touches the new unit. A replace into an empty slot
//! needs no staging: an install gliding to its gain, `Committed` as it arrives unless a capture holds it
//! as above. One into a slot already swapping, or whose unit is being removed, goes straight back
//! `Busy`, counted, never started or stopped, so a slot holds at most two units. With no block to render
//! the swap (idle servicing, an eviction), a staging candidate commits at once and the old unit goes back
//! at once (an eviction then hands back the new one `Evicted`).
//!
//! The engine renders the slots once per range, not per chunk: from where they stopped up to the next
//! frame a slot command waits for (a note, the target, a slot's live flag or gain), so a plugin sees
//! one call per device block unless a stamped slot command splits it. Those commands therefore apply
//! at the start of a render call (every event offset is 0 in practice); an install, a replace or a
//! removal applies at the next block start, which takes at most a message ring's worth per slot (the
//! rest waits for the one after). A live ramp's end splits nothing: the gate is evaluated per frame.

mod port;
mod swap;

pub use port::SlotPort;

use crate::api::{HandbackReason, Rejection, SlotEvent, SlotEventKind, SlotKind, SlotProcessor, SlotReply, SLOT_COUNT};
use crate::grid::Frame;
use port::{SlotEnd, SlotMsg};
use swap::{Swap, STAGE_SECONDS, SWAP_SECONDS};

/// The crossfade into and out of bypass: 10 ms, the web monitor's declick (`audio_output.rs`).
const FADE_SECONDS: f64 = 0.010;
/// The live gate's ramp: 5 ms (STATUS D23).
const LIVE_SECONDS: f64 = 0.005;
/// Slot gain smoothing, as the master volume's.
const GAIN_TAU_SECONDS: f64 = 0.012;
/// Note events one slot queues between two render calls; more are dropped and counted.
pub const MAX_SLOT_EVENTS: usize = 256;
/// The longest record-path delay of an instrument slot: a second, as the built-in instruments'.
const MAX_RECORD_DELAY_SECONDS: usize = 1;

/// One unit's place in a slot: the unit, the tag it was sent with, what it reported as it arrived, the
/// notes it holds, its event queue and its output. A slot plays the unit in its `seat`; its `side` seat
/// holds a swap's other unit.
struct Seat {
    unit: Option<Box<dyn SlotProcessor>>,
    tag: u64,
    /// The unit's kind and latency, read once as it takes the seat. Both outlive its leaving (nothing
    /// reads them without a unit but the record line, whose offset then holds, so an instrument's
    /// delayed tail plays out once).
    kind: SlotKind,
    latency: Frame,
    /// The notes the unit holds (bit per key).
    held: [u64; 2],
    events: Vec<SlotEvent>,
    out: Vec<f32>,
}

struct Slot {
    end: SlotEnd,
    /// The unit the slot plays.
    seat: Seat,
    /// A swap's other unit: the candidate while it stages, the unit it replaced while that fades out.
    side: Seat,
    swap: Swap,
    /// Frames into the linear bypass (0) ↔ engaged (`Rack::fade_frames`) crossfade, counted so both
    /// ends land on an exact frame; `engaged` is the end it heads for.
    fade: u32,
    engaged: bool,
    /// The unit leaves once the fade reaches bypass.
    removing: bool,
    /// GO LIVE as asked: the live gate's target, and what the record latency follows.
    live: bool,
    /// A swap's commit took the slot off live and the engine has not answered it yet
    /// ([`Rack::live_off`]).
    live_off: bool,
    /// The live gate ramps linearly from `live_gain` at frame `live_end - Rack::live_frames` to the
    /// target (1 live, 0 off), which it holds exactly from `live_end` on (`Frame::MIN`: settled).
    live_end: Frame,
    live_gain: f32,
    /// While not 0: the gate is shut, whatever `live` says, until the command taken at this count (an
    /// idle unload's live-off: [`Rack::shut`]) has applied.
    shut: u64,
    gain_target: f64,
    gain: f64,
    /// An instrument slot's record path: its output, `delay - latency` frames late.
    line: Vec<f32>,
    /// A slot's wet on its way to the record tap, `dw` frames late: the latched record latency less
    /// the slot's own, as [`Rack::latch_record`] last took them.
    wet_line: Vec<f32>,
    dw: usize,
    write: usize,
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

/// The bypass crossfade `fade` frames of `n` from `from` (the bypass) towards `to` (the unit): exactly
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

/// A slot gain as the engine applies it: finite and at least 0, anything else silence.
fn slot_gain(gain: f32) -> f64 {
    if gain.is_finite() {
        gain.max(0.0) as f64
    } else {
        0.0
    }
}

impl Seat {
    /// Allocates: built in `Rack::new`.
    fn new(max_block: usize) -> Seat {
        Seat {
            unit: None,
            tag: 0,
            kind: SlotKind::Effect,
            latency: 0,
            held: [0; 2],
            events: Vec::with_capacity(MAX_SLOT_EVENTS),
            out: vec![0.0; max_block.max(1)],
        }
    }

    /// Seat `unit`, sent tagged `tag`. The seat holds it before any of its methods runs: one that
    /// panics unwinds with the unit here, never dropped as a local.
    fn take(&mut self, tag: u64, unit: Box<dyn SlotProcessor>) {
        self.held = [0; 2];
        self.events.clear();
        self.tag = tag;
        let unit = self.unit.insert(unit);
        self.kind = unit.kind();
        self.latency = unit.latency().max(0);
    }

    fn instrument(&self) -> bool {
        self.unit.is_some() && self.kind == SlotKind::Instrument
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

    /// The unit renders `gated.len()` frames from `frame` into `out` with the queued events, which then
    /// clear (an empty seat only clears them): an effect from `gated`, the slot's gated input, an
    /// instrument from `zeros`. An event stamped past the call (never, while slot commands bound the
    /// ranges) plays late on its last frame rather than breaking the offset contract. A non-finite
    /// sample would stick in every filter, echo, limiter and loop after it: the whole call's output goes
    /// silent, counted in `errors`.
    fn process(&mut self, frame: Frame, gated: &[f32], zeros: &[f32], errors: &mut u64) {
        let m = gated.len();
        let x = if self.instrument() { &zeros[..m] } else { gated };
        for e in self.events.iter_mut() {
            e.offset = e.offset.min(m.saturating_sub(1) as u32);
        }
        if let Some(unit) = self.unit.as_mut() {
            let out = &mut self.out[..m];
            unit.process(frame, x, &self.events, out);
            if !out.iter().all(|y| y.is_finite()) {
                out.fill(0.0);
                *errors += 1;
            }
        }
        self.events.clear();
    }

    /// An idle removal or an eviction: no block will carry the notes the unit was just released from,
    /// so one silent frame from `frame` (the next the engine would render) does, and a restarted unit
    /// holds no note. That frame's output is not checked.
    fn flush(&mut self, frame: Frame, zeros: &[f32]) {
        if let Some(unit) = self.unit.as_mut().filter(|_| !self.events.is_empty()) {
            unit.process(frame, &zeros[..1], &self.events, &mut self.out[..1]);
            self.events.clear();
        }
    }

    /// The unit leaves: stopped while the seat still holds it, then handed back on `end` with its tag
    /// and `reason`. A `stop` that panics unwinds with the unit left in the seat, for the owner's
    /// eviction (or the engine's drop, off the audio thread).
    fn hand_back(&mut self, end: &mut SlotEnd, reason: HandbackReason, errors: &mut u64) {
        if let Some(unit) = self.unit.as_mut() {
            unit.stop();
        }
        if let Some(unit) = self.unit.take() {
            end.hand_back(self.tag, reason, unit, errors);
        }
        self.held = [0; 2];
        self.events.clear();
    }
}

impl Slot {
    /// Into an empty slot, the unit takes the seat and crossfades in from bypass, `fade` frames already
    /// done (all of them: engaged at once). Into an occupied one it goes straight back `Occupied`,
    /// counted, never started or stopped.
    fn install(&mut self, tag: u64, unit: Box<dyn SlotProcessor>, fade: u32, errors: &mut u64) {
        if self.seat.unit.is_some() {
            *errors += 1;
            self.end.hand_back(tag, HandbackReason::Rejected(Rejection::Occupied), unit, errors);
            return;
        }
        self.engage(fade);
        self.seat.take(tag, unit);
    }

    /// The seat's unit is new: it heads in from bypass, `fade` frames of the way already done.
    fn engage(&mut self, fade: u32) {
        self.removing = false;
        self.engaged = true;
        self.fade = fade;
    }

    /// A remove for `tag`: the unit the slot plays, if `tag` is its, releases its notes and heads for
    /// bypass (there at once without `fade`), a candidate staging beside it sent back `Cancelled` first.
    /// A unit already leaving (being removed, or fading out after a swap) gets no answer; any other tag,
    /// an empty slot's and a staging candidate's included, is answered `NotHeld`.
    fn remove(&mut self, tag: u64, fade: bool, dropped: &mut u64, errors: &mut u64) {
        if self.seat.unit.is_none() || self.seat.tag != tag {
            if !self.fading_out(tag) {
                self.end.send(SlotReply::NotHeld { tag }, errors);
            }
        } else if !self.removing {
            self.cancel(errors);
            self.seat.release_all(0, dropped);
            self.removing = true;
            self.engaged = false;
            if !fade {
                self.fade = 0;
            }
        }
    }

    /// GO LIVE on or off: the live gate ramps from where it is to the new target over `ramp`'s frames
    /// from its frame (`Rack::set_live`); with no `ramp` (nothing rendered yet, or no device running) it
    /// is there at once, settled. The same target again changes nothing.
    fn set_live(&mut self, live: bool, ramp: Option<(Frame, Frame)>) {
        if self.live == live {
            return;
        }
        let to = if live { 1.0 } else { 0.0 };
        let from = ramp.map(|(now, n)| (live_gate(self.live, self.live_end, self.live_gain, now, n), now + n));
        self.live = live;
        (self.live_end, self.live_gain) = match from {
            Some((from, end)) if from != to => (end, from),
            _ => (Frame::MIN, to),
        };
    }

    /// The removal ends: a unit still fading out after a swap goes back `ReplacedOut`, then the slot's
    /// `Removed`, and the slot is empty.
    fn finish_removal(&mut self, errors: &mut u64) {
        self.retire_side(HandbackReason::ReplacedOut, errors);
        self.seat.hand_back(&mut self.end, HandbackReason::Removed, errors);
        self.removing = false;
    }

    /// Frame `k` of a render call, `b` its bypass (the gated input): what the slot adds to the wet and to
    /// the bus (`None`: nothing, so an effect's slot leaves the bus's bits alone). The bypass fade and
    /// the swap fade step on, and the gain glides.
    fn mix(&mut self, k: usize, b: f32, fade_frames: u32, swap_frames: u32, gain_coef: f64) -> (f32, Option<f32>) {
        // A removal completes on the frame its fade reaches bypass, whatever the call's bounds: from
        // there the slot is empty and passes what an empty slot does.
        let gone = self.removing && self.fade == 0;
        let fade = self.fade;
        if self.engaged && self.fade < fade_frames {
            self.fade += 1;
        } else if !self.engaged && self.fade > 0 {
            self.fade -= 1;
        }
        let swap = self.swap.step(swap_frames);
        self.gain = crate::glide(self.gain, self.gain_target, gain_coef);
        let Some(at) = swap else { return self.plain(k, b, fade, fade_frames, gone) };
        // Mid-swap the bypass crossfade runs over both units' terms, the bypass at the slot's gain.
        let (wet, bus) = self.crossing(k, at, swap_frames);
        let bypass = (self.gain * b as f64) as f32;
        (crossfade(bypass, wet, fade, fade_frames), Some(crossfade(0.0, bus, fade, fade_frames)))
    }

    /// Frame `k` outside a swap fade: the bypass crossfade `fade` frames (of `n`) from the bypass `b`
    /// towards the unit the slot plays (an empty seat passes `b`), then the gain. An instrument's output
    /// and its bypass go different ways (the bus, the wet), so each fades against silence there.
    fn plain(&self, k: usize, b: f32, fade: u32, n: u32, gone: bool) -> (f32, Option<f32>) {
        let o = if self.seat.unit.is_some() { self.seat.out[k] } else { b };
        let g = self.gain;
        let gained = |y: f32| if g == 1.0 { y } else { (g * y as f64) as f32 };
        if self.seat.instrument() && !gone {
            (gained(crossfade(b, 0.0, fade, n)), Some(gained(crossfade(0.0, o, fade, n))))
        } else {
            (gained(crossfade(b, o, fade, n)), None)
        }
    }
}

/// The two slots, as the engine holds them.
pub(crate) struct Rack {
    slots: [Slot; SLOT_COUNT],
    zeros: Vec<f32>,
    /// The gated input over a render call that a ramp touches, one slot's at a time.
    gated: Vec<f32>,
    fade_frames: u32,
    /// A candidate's staging past its latency, and the swap fade, in frames.
    stage_frames: Frame,
    swap_frames: u32,
    /// The live gate's ramp in frames: `LIVE_SECONDS` at the rate, at least 1.
    live_frames: Frame,
    /// A render call has run: from here a live toggle ramps (before it, it sets the starting state).
    rendered: bool,
    gain_coef: f64,
    /// The slot that takes the notes, if a slot is the note target.
    target: Option<usize>,
    /// The live latency the record tap is aligned on ([`Rack::latch_record`]).
    record_latency: Frame,
    /// The device frame the next render call starts at.
    cursor: Frame,
    pub(crate) events_dropped: u64,
    pub(crate) protocol_errors: u64,
}

impl Rack {
    /// Allocates: build it off the audio thread.
    pub(crate) fn new(sample_rate: u32, max_block: usize) -> (Rack, [SlotPort; SLOT_COUNT]) {
        let line = sample_rate as usize * MAX_RECORD_DELAY_SECONDS + max_block;
        let mut ports = Vec::with_capacity(SLOT_COUNT);
        let slots = std::array::from_fn(|_| {
            let (port, end) = port::channel();
            ports.push(port);
            Slot {
                end,
                seat: Seat::new(max_block),
                side: Seat::new(max_block),
                swap: Swap::Idle,
                fade: 0,
                engaged: false,
                removing: false,
                live: false,
                live_off: false,
                live_end: Frame::MIN,
                live_gain: 0.0,
                shut: 0,
                gain_target: 1.0,
                gain: 1.0,
                line: vec![0.0; line],
                wet_line: vec![0.0; line],
                dw: 0,
                write: 0,
            }
        });
        let ports: [SlotPort; SLOT_COUNT] = match ports.try_into() {
            Ok(ports) => ports,
            Err(_) => unreachable!(),
        };
        let rack = Rack {
            slots,
            zeros: vec![0.0; max_block.max(1)],
            gated: vec![0.0; max_block.max(1)],
            fade_frames: (FADE_SECONDS * sample_rate as f64).round().max(1.0) as u32,
            stage_frames: (STAGE_SECONDS * sample_rate as f64).round().max(1.0) as Frame,
            swap_frames: (SWAP_SECONDS * sample_rate as f64).round().max(1.0) as u32,
            live_frames: (LIVE_SECONDS * sample_rate as f64).round().max(1.0) as Frame,
            rendered: false,
            gain_coef: (-1.0 / (GAIN_TAU_SECONDS * sample_rate as f64)).exp(),
            target: None,
            record_latency: 0,
            cursor: 0,
            events_dropped: 0,
            protocol_errors: 0,
        };
        (rack, ports)
    }

    /// Apply the ports' messages at a block start, at most a ring's worth per slot (the rest waits for
    /// the next), then let a staged swap commit (unless `capturing` holds it), a live-off it makes
    /// ramping as `live` says (`Slot::set_live`). `engaged` installs without a fade (nothing has sounded
    /// yet, or no device runs); without `fade` a swap and a removal complete here.
    fn service(&mut self, engaged: bool, fade: bool, capturing: bool, live: Option<(Frame, Frame)>) {
        let (dropped, errors, stage) = (&mut self.events_dropped, &mut self.protocol_errors, self.stage_frames);
        let fade_in = if engaged { self.fade_frames } else { 0 };
        for s in self.slots.iter_mut() {
            s.end.unpark();
            for _ in 0..port::MSG_CAPACITY {
                let Some(msg) = s.end.next() else { break };
                match msg {
                    SlotMsg::Install { tag, unit } => s.install(tag, unit, fade_in, errors),
                    SlotMsg::Replace { tag, unit, gain } => s.replace(tag, unit, gain, fade_in, stage, capturing, dropped, errors),
                    SlotMsg::Remove { tag } => s.remove(tag, fade, dropped, errors),
                    SlotMsg::Withdraw { tag } => s.withdraw(tag, errors),
                }
            }
            if fade {
                s.commit_when_ready(stage, capturing, fade_in, live, dropped, errors);
            } else {
                s.settle_swap(self.cursor, &self.zeros, fade_in, dropped, errors);
            }
            if !fade && s.removing {
                s.seat.flush(self.cursor, &self.zeros);
                s.finish_removal(errors);
            }
        }
    }

    /// At a block start: service the ports and start the render cursor. `capturing`: a take's
    /// alignment is fixed over this block, so a swap that changes the latency waits.
    pub(crate) fn begin_block(&mut self, start: Frame, first: bool, capturing: bool) {
        let live = self.rendered.then_some((start, self.live_frames));
        self.service(first, true, capturing, live);
        self.cursor = start;
    }

    /// With no device running (the host holds the engine): apply the ports' messages at once, no
    /// fades. A removed unit stops here and goes straight back.
    pub(crate) fn service_idle(&mut self) {
        self.service(true, false, false, None);
    }

    /// Stop every unit and hand it back `Evicted` (a sample-rate change: the host re-activates and
    /// reinstalls), an install still waiting on a port included: nothing is left for the engine's drop.
    pub(crate) fn evict(&mut self) {
        self.service_idle();
        for s in self.slots.iter_mut() {
            if s.seat.unit.is_some() {
                s.seat.release_all(0, &mut self.events_dropped);
                s.fade = 0;
                s.engaged = false;
                s.seat.flush(self.cursor, &self.zeros);
                s.seat.hand_back(&mut s.end, HandbackReason::Evicted, &mut self.protocol_errors);
            }
        }
    }

    /// The largest latency of a live slot's effect now: what [`Rack::latch_record`] takes while open.
    pub(crate) fn live_latency(&self) -> Frame {
        self.slots.iter().filter(|s| s.live && s.shut == 0 && s.seat.unit.is_some() && s.seat.kind == SlotKind::Effect).map(|s| s.seat.latency).max().unwrap_or(0)
    }

    /// At a block start: while `open`, the record compensation follows the slots (the live latency, and
    /// each slot's wet delayed by what its own latency lacks of it); shut, it holds what it last took.
    /// The engine shuts it while a capture's alignment is fixed, which took the same live latency.
    pub(crate) fn latch_record(&mut self, open: bool) {
        if !open {
            return;
        }
        self.record_latency = self.live_latency();
        for s in self.slots.iter_mut() {
            let own = if s.seat.unit.is_some() && s.seat.kind == SlotKind::Effect { s.seat.latency } else { 0 };
            s.dw = (self.record_latency - own).clamp(0, (s.wet_line.len() - 1) as Frame) as usize;
        }
    }

    /// The live latency the record tap is aligned on, as last latched.
    pub(crate) fn record_latency(&self) -> Frame {
        self.record_latency
    }

    pub(crate) fn installed(&self, slot: usize) -> Option<(SlotKind, Frame)> {
        self.slots.get(slot).and_then(|s| s.seat.unit.as_ref().map(|_| (s.seat.kind, s.seat.latency)))
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
                s.seat.release_all(offset, &mut self.events_dropped);
            }
        }
        self.target = target.filter(|&i| i < SLOT_COUNT);
    }

    pub(crate) fn note_on(&mut self, key: u8, velocity: f32, now: Frame) {
        let offset = self.offset(now);
        let Some(s) = self.target.map(|i| &mut self.slots[i]) else { return };
        if key > 127 || s.seat.unit.is_none() || s.removing {
            return;
        }
        let velocity = if velocity.is_finite() { velocity.clamp(0.0, 1.0) } else { 0.0 };
        s.seat.queue(SlotEventKind::NoteOn { key, velocity }, offset, &mut self.events_dropped);
        s.seat.held[(key / 64) as usize] |= 1 << (key % 64);
    }

    pub(crate) fn note_off(&mut self, key: u8, now: Frame) {
        let offset = self.offset(now);
        let Some(s) = self.target.map(|i| &mut self.slots[i]) else { return };
        if key > 127 || s.seat.held[(key / 64) as usize] & (1 << (key % 64)) == 0 {
            return;
        }
        s.seat.held[(key / 64) as usize] &= !(1 << (key % 64));
        s.seat.queue(SlotEventKind::NoteOff { key }, offset, &mut self.events_dropped);
    }

    pub(crate) fn all_notes_off(&mut self, now: Frame) {
        let offset = self.offset(now);
        if let Some(s) = self.target.map(|i| &mut self.slots[i]) {
            s.seat.release_all(offset, &mut self.events_dropped);
        }
    }

    /// Whether a swap's commit took slot `slot` off live and the engine has not answered it yet
    /// (`Engine::answer_live_offs`).
    pub(crate) fn live_off(&self, slot: usize) -> bool {
        self.slots.get(slot).is_some_and(|s| s.live_off)
    }

    /// The engine answered slot `slot`'s live-off, or a GO LIVE since stands in for it.
    pub(crate) fn clear_live_off(&mut self, slot: usize) {
        if let Some(s) = self.slots.get_mut(slot) {
            s.live_off = false;
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
        let ramp = self.rendered.then_some((now, self.live_frames));
        if let Some(s) = self.slots.get_mut(slot) {
            s.set_live(live, ramp);
        }
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
            s.gain_target = slot_gain(gain);
        }
    }

    /// Render both slots over `wet.len()` frames from the cursor, slot `s` reading `inputs[s]`: `wet`,
    /// `aligned` and `bus` are overwritten (the effects' outputs as heard and at the take's alignment,
    /// and the instruments'), `record` gets `aligned` plus the instruments' record path, `delay` frames
    /// late less each one's latency (`delay` = the input side plus the latched record latency, as the
    /// built-in instruments' record path).
    pub(crate) fn render(&mut self, inputs: [&[f32]; SLOT_COUNT], wet: &mut [f32], aligned: &mut [f32], bus: &mut [f32], record: &mut [f32], delay: Frame) {
        let m = wet.len();
        wet.fill(0.0);
        aligned.fill(0.0);
        bus.fill(0.0);
        record.fill(0.0);
        let (frame, fade_frames, swap_frames, n) = (self.cursor, self.fade_frames, self.swap_frames, self.live_frames);
        self.rendered |= m > 0;
        for (s, input) in self.slots.iter_mut().zip(inputs) {
            let input = &input[..m];
            // The live gate: settled, the input itself or silence; mid-ramp, the input times the gate,
            // frame by frame, into `gated`. It feeds an effect and an empty slot alike, and it is the
            // dry side of the bypass crossfade; an effect's output is never gated, so its tail rings.
            let (live, end, from) = if s.shut == 0 { (s.live, s.live_end, s.live_gain) } else { (false, Frame::MIN, 0.0) };
            if frame < end {
                for (k, (g, &x)) in self.gated.iter_mut().zip(input).enumerate() {
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
            let gated = if frame < end {
                &self.gated[..m]
            } else if live {
                input
            } else {
                &self.zeros[..m]
            };
            // A swap's other unit renders beside the one the slot plays (an empty seat renders nothing).
            s.seat.process(frame, gated, &self.zeros, &mut self.protocol_errors);
            s.side.process(frame, gated, &self.zeros, &mut self.protocol_errors);
            let len = s.line.len();
            let (d, dw) = ((delay - s.seat.latency).clamp(0, (len - 1) as Frame) as usize, s.dw);
            for (k, &b) in gated.iter().enumerate() {
                let (w, v) = s.mix(k, b, fade_frames, swap_frames, self.gain_coef);
                wet[k] += w;
                if let Some(v) = v {
                    bus[k] += v;
                }
                s.line[s.write] = v.unwrap_or(0.0);
                s.wet_line[s.write] = w;
                record[k] += s.line[(s.write + len - d) % len];
                aligned[k] += s.wet_line[(s.write + len - dw) % len];
                s.write = (s.write + 1) % len;
            }
            s.swap_after_call(m, swap_frames, &mut self.protocol_errors);
            if s.removing && s.fade == 0 {
                s.finish_removal(&mut self.protocol_errors);
            }
        }
        for (r, &w) in record.iter_mut().zip(aligned.iter()) {
            *r += w;
        }
        self.cursor += m as Frame;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An instrument that renders silence.
    struct Sink;

    impl SlotProcessor for Sink {
        fn kind(&self) -> SlotKind {
            SlotKind::Instrument
        }

        fn latency(&self) -> Frame {
            0
        }

        fn process(&mut self, _frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
            out.fill(0.0);
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
        let (mut rack, mut ports) = Rack::new(48000, 64);
        assert!(ports[0].install(0, Box::new(Sink)).is_ok());
        rack.service_idle();
        rack.select(Some(0), 0);
        for k in 0..MAX_SLOT_EVENTS + 3 {
            rack.note_on((k % 128) as u8, 1.0, 0);
        }
        assert_eq!(rack.events_dropped, 3);
        assert_eq!(rack.slots[0].seat.events.len(), MAX_SLOT_EVENTS);
    }
}
