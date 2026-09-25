//! OWNS: the plugin slots inside the callback: the units the host installs and takes back through each
//! slot's [`SlotPort`], their bypass crossfades, the notes a slot holds, the live flag and gain, and
//! where a slot's output goes. The processors themselves are the host's (`src-tauri/src/host`, which
//! implements [`SlotProcessor`] for CLAP and VST3).
//!
//! A slot holding an effect (or nothing) passes the device input on while it is live and gets silence
//! otherwise; its output is the wet signal, which the engine hears after the limiter and records at the
//! take's alignment. A slot holding an instrument plays the notes while it is the note target; its
//! output joins the master bus and goes to the record tap delayed like a built-in instrument's, less the
//! plugin's own latency (`instruments`). Outputs are mono, as today's bridge is.
//!
//! Lifecycle never stops the audio. The host builds and activates a unit off the audio thread and
//! installs it: the unit crossfades in from bypass (an effect's bypass is its dry input, an
//! instrument's is silence) over the frames of `FADE_SECONDS`, counted, so both ends land on an exact
//! frame and are bit-exact. A removal first releases the notes the slot holds and crossfades to bypass;
//! the slot is empty from the frame the fade ends. The release reaches the unit in the fade's first
//! `process`, [`SlotProcessor::stop`] follows its last, and the unit goes back on the port's return
//! ring (a full ring parks it until there is room). With no device running the ports are serviced at
//! once, without fades, and one silent frame of `process` carries a removed unit's release. Nothing
//! here drops a unit: an eviction hands back an install still waiting on a port too, and a unit's
//! methods run only while its slot holds it, so a panic the callback's guard catches unwinds past no
//! unit held as a local. Installing into an occupied slot is a protocol error: the new unit goes
//! straight back, counted, never started or stopped.
//!
//! The engine renders the slots once per range, not per chunk: from where they stopped up to the next
//! frame a slot command waits for (a note, the target, a slot's live flag or gain), so a plugin sees
//! one call per device block unless a stamped slot command splits it. Those commands therefore apply
//! at the start of a render call (every event offset is 0 in practice); an install or removal applies
//! at the next block start.

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::api::{SlotEvent, SlotEventKind, SlotKind, SlotProcessor, SLOT_COUNT};
use crate::grid::Frame;

/// The crossfade into and out of bypass: 10 ms, the web monitor's declick (`audio_output.rs`).
const FADE_SECONDS: f64 = 0.010;
/// Slot gain smoothing, as the master volume's.
const GAIN_TAU_SECONDS: f64 = 0.012;
/// Note events one slot queues between two render calls; more are dropped and counted.
pub const MAX_SLOT_EVENTS: usize = 256;
/// The longest record-path delay of an instrument slot: a second, as the built-in instruments'.
const MAX_RECORD_DELAY_SECONDS: usize = 1;
const MSG_CAPACITY: usize = 4;
const RETURN_CAPACITY: usize = 4;

enum SlotMsg {
    Install(Box<dyn SlotProcessor>),
    Remove,
}

/// The host's end of one slot (`EngineHandle::slots`). Not real-time: the host blocks on it, the engine
/// never does.
pub struct SlotPort {
    tx: Producer<SlotMsg>,
    rx: Consumer<Box<dyn SlotProcessor>>,
}

impl SlotPort {
    /// Hand an activated unit to the slot; it crossfades in from the next block. Gives the unit back
    /// when the message ring is full (the engine is not rendering and has not been serviced).
    pub fn install(&mut self, unit: Box<dyn SlotProcessor>) -> Result<(), Box<dyn SlotProcessor>> {
        self.tx.push(SlotMsg::Install(unit)).map_err(|PushError::Full(msg)| match msg {
            SlotMsg::Install(unit) => unit,
            SlotMsg::Remove => unreachable!(),
        })
    }

    /// Ask for the slot's unit back: it releases its notes, crossfades to bypass, stops and arrives on
    /// [`SlotPort::returned`]. Nothing comes back from an empty slot. False when the ring is full.
    pub fn remove(&mut self) -> bool {
        self.tx.push(SlotMsg::Remove).is_ok()
    }

    /// A unit the engine handed back: removed, evicted ([`crate::Engine::evict_slots`]) or refused.
    pub fn returned(&mut self) -> Option<Box<dyn SlotProcessor>> {
        self.rx.pop().ok()
    }
}

/// The engine's end of one slot.
struct SlotEnd {
    rx: Consumer<SlotMsg>,
    tx: Producer<Box<dyn SlotProcessor>>,
}

fn slot_channel() -> (SlotPort, SlotEnd) {
    let (msg_tx, msg_rx) = RingBuffer::new(MSG_CAPACITY);
    let (ret_tx, ret_rx) = RingBuffer::new(RETURN_CAPACITY);
    (SlotPort { tx: msg_tx, rx: ret_rx }, SlotEnd { rx: msg_rx, tx: ret_tx })
}

struct Slot {
    end: SlotEnd,
    unit: Option<Box<dyn SlotProcessor>>,
    /// The unit's kind and latency, read once at install. Both outlive a removal (nothing reads them
    /// without a unit but the record line, whose offset then holds, so an instrument's delayed tail
    /// plays out once).
    kind: SlotKind,
    latency: Frame,
    /// Frames into the linear bypass (0) ↔ engaged (`Rack::fade_frames`) crossfade, counted so both
    /// ends land on an exact frame; `engaged` is the end it heads for.
    fade: u32,
    engaged: bool,
    /// The unit leaves once the fade reaches bypass.
    removing: bool,
    /// Units stopped and waiting for room on the return ring, oldest first.
    parked: [Option<Box<dyn SlotProcessor>>; 2],
    live: bool,
    gain_target: f64,
    gain: f64,
    /// The notes this slot's unit holds (bit per key).
    held: [u64; 2],
    events: Vec<SlotEvent>,
    out: Vec<f32>,
    /// An instrument slot's record path: its output, `delay - latency` frames late.
    line: Vec<f32>,
    write: usize,
}

impl Slot {
    fn takes_input(&self) -> bool {
        self.live && !(self.unit.is_some() && self.kind == SlotKind::Instrument)
    }

    fn instrument(&self) -> bool {
        self.unit.is_some() && self.kind == SlotKind::Instrument
    }

    fn park(&mut self, unit: Box<dyn SlotProcessor>, errors: &mut u64) {
        let Err(PushError::Full(unit)) = self.end.tx.push(unit) else { return };
        match self.parked.iter_mut().find(|p| p.is_none()) {
            Some(p) => *p = Some(unit),
            None => {
                // Unreachable under the host protocol (one unit per slot in flight, and the host reads
                // its port). Leak rather than drop: a drop frees memory and calls into the plugin's DLL.
                *errors += 1;
                std::mem::forget(unit);
            }
        }
    }

    fn unpark(&mut self) {
        for p in self.parked.iter_mut() {
            if let Some(unit) = p.take() {
                if let Err(PushError::Full(unit)) = self.end.tx.push(unit) {
                    *p = Some(unit);
                    return;
                }
            }
        }
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
}

/// The two slots, as the engine holds them.
pub(crate) struct Rack {
    slots: [Slot; SLOT_COUNT],
    zeros: Vec<f32>,
    fade_frames: u32,
    gain_coef: f64,
    /// The slot that takes the notes, if a slot is the note target.
    target: Option<usize>,
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
            let (port, end) = slot_channel();
            ports.push(port);
            Slot {
                end,
                unit: None,
                kind: SlotKind::Effect,
                latency: 0,
                fade: 0,
                engaged: false,
                removing: false,
                parked: [None, None],
                live: false,
                gain_target: 1.0,
                gain: 1.0,
                held: [0; 2],
                events: Vec::with_capacity(MAX_SLOT_EVENTS),
                out: vec![0.0; max_block.max(1)],
                line: vec![0.0; line],
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
            fade_frames: (FADE_SECONDS * sample_rate as f64).round().max(1.0) as u32,
            gain_coef: (-1.0 / (GAIN_TAU_SECONDS * sample_rate as f64)).exp(),
            target: None,
            cursor: 0,
            events_dropped: 0,
            protocol_errors: 0,
        };
        (rack, ports)
    }

    /// Apply the ports' messages at a block start. `engaged` installs without a fade (nothing has
    /// sounded yet, or no device runs).
    fn service(&mut self, engaged: bool, fade: bool) {
        let dropped = &mut self.events_dropped;
        for s in self.slots.iter_mut() {
            s.unpark();
            while let Ok(msg) = s.end.rx.pop() {
                match msg {
                    SlotMsg::Install(unit) => {
                        if s.unit.is_some() {
                            self.protocol_errors += 1;
                            s.park(unit, &mut self.protocol_errors);
                            continue;
                        }
                        s.held = [0; 2];
                        s.events.clear();
                        s.removing = false;
                        s.engaged = true;
                        s.fade = if engaged { self.fade_frames } else { 0 };
                        // The slot holds the unit before any of its methods runs: one that panics
                        // unwinds with the unit here, never dropped as a local.
                        let unit = s.unit.insert(unit);
                        s.kind = unit.kind();
                        s.latency = unit.latency().max(0);
                    }
                    SlotMsg::Remove => {
                        if s.unit.is_none() {
                            continue;
                        }
                        s.release_all(0, dropped);
                        s.removing = true;
                        s.engaged = false;
                        if !fade {
                            s.fade = 0;
                        }
                    }
                }
            }
            if !fade && s.removing {
                Self::flush(s, self.cursor, &self.zeros);
                Self::finish_removal(s, &mut self.protocol_errors);
            }
        }
    }

    /// At a block start: service the ports and start the render cursor.
    pub(crate) fn begin_block(&mut self, start: Frame, first: bool) {
        self.service(first, true);
        self.cursor = start;
    }

    /// With no device running (the host holds the engine): apply the ports' messages at once, no
    /// fades. A removed unit stops here and goes straight back.
    pub(crate) fn service_idle(&mut self) {
        self.service(true, false);
    }

    /// Stop every unit and hand it back (a sample-rate change: the host re-activates and reinstalls),
    /// an install still waiting on a port included: nothing is left for the engine's drop.
    pub(crate) fn evict(&mut self) {
        self.service_idle();
        for s in self.slots.iter_mut() {
            if s.unit.is_some() {
                s.release_all(0, &mut self.events_dropped);
                s.fade = 0;
                s.engaged = false;
                Self::flush(s, self.cursor, &self.zeros);
                Self::finish_removal(s, &mut self.protocol_errors);
            }
        }
    }

    /// An idle removal or an eviction: no block will carry the notes the unit was just released from,
    /// so one silent frame from `frame` (the next the engine would render) does, and a restarted unit
    /// holds no note.
    fn flush(s: &mut Slot, frame: Frame, zeros: &[f32]) {
        if let Some(unit) = s.unit.as_mut().filter(|_| !s.events.is_empty()) {
            unit.process(frame, &zeros[..1], &s.events, &mut s.out[..1]);
            s.events.clear();
        }
    }

    /// The unit leaves: stopped while the slot still holds it, then handed back. A `stop` that panics
    /// unwinds with the unit left in the slot, for the owner's eviction (or the engine's drop, off the
    /// audio thread).
    fn finish_removal(s: &mut Slot, errors: &mut u64) {
        if let Some(unit) = s.unit.as_mut() {
            unit.stop();
        }
        if let Some(unit) = s.unit.take() {
            s.park(unit, errors);
        }
        s.removing = false;
        s.held = [0; 2];
        s.events.clear();
    }

    /// Frames the wet signal lags the input: the largest latency of a live slot's effect.
    pub(crate) fn live_latency(&self) -> Frame {
        self.slots.iter().filter(|s| s.live && s.unit.is_some() && s.kind == SlotKind::Effect).map(|s| s.latency).max().unwrap_or(0)
    }

    pub(crate) fn installed(&self, slot: usize) -> Option<(SlotKind, Frame)> {
        self.slots.get(slot).and_then(|s| s.unit.as_ref().map(|_| (s.kind, s.latency)))
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
        if key > 127 || s.unit.is_none() || s.removing {
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

    pub(crate) fn set_live(&mut self, slot: usize, live: bool) {
        if let Some(s) = self.slots.get_mut(slot) {
            s.live = live;
        }
    }

    pub(crate) fn set_gain(&mut self, slot: usize, gain: f32) {
        if let Some(s) = self.slots.get_mut(slot) {
            s.gain_target = if gain.is_finite() { gain.max(0.0) as f64 } else { 0.0 };
        }
    }

    /// Render both slots over `input.len()` frames from the cursor: `wet` and `bus` are overwritten
    /// (the effects' and the instruments' outputs), `record` gets `wet` plus the instruments' record
    /// path, `delay` frames late less each one's latency (`delay` = the input side plus the live
    /// effect's latency, as the built-in instruments' record path).
    pub(crate) fn render(&mut self, input: &[f32], wet: &mut [f32], bus: &mut [f32], record: &mut [f32], delay: Frame) {
        let m = input.len();
        wet.fill(0.0);
        bus.fill(0.0);
        record.fill(0.0);
        let (frame, fade_frames) = (self.cursor, self.fade_frames);
        for s in self.slots.iter_mut() {
            let x = if s.takes_input() { input } else { &self.zeros[..m] };
            // A removal completes on the frame its fade reaches bypass, whatever the call's bounds: from
            // there the slot is empty and passes what an empty slot does.
            let empty = if s.live { input } else { &self.zeros[..m] };
            let has = s.unit.is_some();
            // An event stamped past this call (never, while slot commands bound the ranges) plays late
            // on its last frame rather than breaking the offset contract.
            for e in s.events.iter_mut() {
                e.offset = e.offset.min(m.saturating_sub(1) as u32);
            }
            if let Some(unit) = s.unit.as_mut() {
                unit.process(frame, x, &s.events, &mut s.out[..m]);
            }
            s.events.clear();
            let instrument = s.instrument();
            let len = s.line.len();
            let d = (delay - s.latency).clamp(0, (len - 1) as Frame) as usize;
            for k in 0..m {
                let gone = s.removing && s.fade == 0;
                let b = if gone { empty[k] } else { x[k] };
                // Read only mid-fade, which a gone slot never is.
                let o = if has { s.out[k] } else { b };
                let y = if s.fade == fade_frames {
                    o
                } else if s.fade == 0 {
                    b
                } else {
                    b + (s.fade as f32 / fade_frames as f32) * (o - b)
                };
                if s.engaged && s.fade < fade_frames {
                    s.fade += 1;
                } else if !s.engaged && s.fade > 0 {
                    s.fade -= 1;
                }
                s.gain = s.gain_target + (s.gain - s.gain_target) * self.gain_coef;
                let y = if s.gain == 1.0 { y } else { (s.gain * y as f64) as f32 };
                if instrument && !gone {
                    bus[k] += y;
                    s.line[s.write] = y;
                } else {
                    wet[k] += y;
                    s.line[s.write] = 0.0;
                }
                record[k] += s.line[(s.write + len - d) % len];
                s.write = (s.write + 1) % len;
            }
            if s.removing && s.fade == 0 {
                Self::finish_removal(s, &mut self.protocol_errors);
            }
        }
        for (r, &w) in record.iter_mut().zip(wet.iter()) {
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
        assert!(ports[0].install(Box::new(Sink)).is_ok());
        rack.service_idle();
        rack.select(Some(0), 0);
        for k in 0..MAX_SLOT_EVENTS + 3 {
            rack.note_on((k % 128) as u8, 1.0, 0);
        }
        assert_eq!(rack.events_dropped, 3);
        assert_eq!(rack.slots[0].events.len(), MAX_SLOT_EVENTS);
    }
}
