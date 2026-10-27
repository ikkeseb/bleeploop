//! A slot's port: the host's tagged messages in, the engine's replies out ([`SlotReply`]), in the order
//! the engine sent them. A reply the return ring has no room for parks behind those already parked and
//! moves on before any later one, so nothing overtakes; one with nowhere to go is leaked (never
//! dropped: a drop frees memory and calls into the plugin's DLL) and counted.

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::api::{Handback, HandbackReason, SlotProcessor, SlotReply};

/// Messages a slot's ring holds: also what a block takes from it at most.
pub(super) const MSG_CAPACITY: usize = 4;
const RETURN_CAPACITY: usize = 4;
/// Replies waiting for room on the return ring.
const PARKED: usize = 2;

pub(super) enum SlotMsg {
    Install { tag: u64, unit: Box<dyn SlotProcessor> },
    Replace {
        tag: u64,
        unit: Box<dyn SlotProcessor>,
        gain: f32,
    },
    Remove { tag: u64 },
    Withdraw { tag: u64 },
}

/// The host's end of one slot (`EngineHandle::slots`). Not real-time: the host blocks on it, the engine
/// never does. A tag is the host's own; the engine only compares tags, and 0 is one.
pub struct SlotPort {
    tx: Producer<SlotMsg>,
    rx: Consumer<SlotReply>,
}

impl SlotPort {
    /// Hand an activated unit to an empty slot; it crossfades in from the next block. Gives the unit
    /// back when the message ring is full (the engine is not rendering and has not been serviced).
    pub fn install(&mut self, tag: u64, unit: Box<dyn SlotProcessor>) -> Result<(), Box<dyn SlotProcessor>> {
        self.send_unit(SlotMsg::Install { tag, unit })
    }

    /// Stage `unit` beside the unit the slot plays, then crossfade to it at `gain` (`crate::slots`'
    /// rules). Gives the unit back when the message ring is full.
    pub fn replace(&mut self, tag: u64, unit: Box<dyn SlotProcessor>, gain: f32) -> Result<(), Box<dyn SlotProcessor>> {
        self.send_unit(SlotMsg::Replace { tag, unit, gain })
    }

    /// Take out the unit the slot plays, if `tag` is its: it comes back `Removed` (`crate::slots`'
    /// rules). False when the ring is full.
    pub fn remove(&mut self, tag: u64) -> bool {
        self.tx.push(SlotMsg::Remove { tag }).is_ok()
    }

    /// Call off the replace tagged `tag` while its unit stages (`crate::slots`' rules). False when the
    /// ring is full.
    pub fn withdraw(&mut self, tag: u64) -> bool {
        self.tx.push(SlotMsg::Withdraw { tag }).is_ok()
    }

    /// The next reply, in the order the engine sent them.
    pub fn reply(&mut self) -> Option<SlotReply> {
        self.rx.pop().ok()
    }

    fn send_unit(&mut self, msg: SlotMsg) -> Result<(), Box<dyn SlotProcessor>> {
        self.tx.push(msg).map_err(|PushError::Full(msg)| match msg {
            SlotMsg::Install { unit, .. } | SlotMsg::Replace { unit, .. } => unit,
            SlotMsg::Remove { .. } | SlotMsg::Withdraw { .. } => unreachable!(),
        })
    }
}

/// The engine's end of one slot.
pub(super) struct SlotEnd {
    rx: Consumer<SlotMsg>,
    tx: Producer<SlotReply>,
    /// Replies waiting for room on the return ring, oldest at `head`.
    parked: [Option<SlotReply>; PARKED],
    head: usize,
    len: usize,
}

pub(super) fn channel() -> (SlotPort, SlotEnd) {
    let (msg_tx, msg_rx) = RingBuffer::new(MSG_CAPACITY);
    let (ret_tx, ret_rx) = RingBuffer::new(RETURN_CAPACITY);
    (SlotPort { tx: msg_tx, rx: ret_rx }, SlotEnd { rx: msg_rx, tx: ret_tx, parked: [None, None], head: 0, len: 0 })
}

impl SlotEnd {
    pub(super) fn next(&mut self) -> Option<SlotMsg> {
        self.rx.pop().ok()
    }

    /// Move parked replies onto the return ring, oldest first, while it has room.
    pub(super) fn unpark(&mut self) {
        while self.len > 0 {
            let Some(reply) = self.parked[self.head].take() else { unreachable!() };
            if let Err(PushError::Full(reply)) = self.tx.push(reply) {
                self.parked[self.head] = Some(reply);
                return;
            }
            self.head = (self.head + 1) % PARKED;
            self.len -= 1;
        }
    }

    /// Reply behind everything sent before: onto the ring once nothing is parked, else parked; with
    /// nowhere to go, leaked and counted in `errors`. Unreachable under the host protocol (at most two
    /// units per slot in flight, and the host reads its port).
    pub(super) fn send(&mut self, reply: SlotReply, errors: &mut u64) {
        self.unpark();
        let reply = if self.len == 0 {
            match self.tx.push(reply) {
                Ok(()) => return,
                Err(PushError::Full(reply)) => reply,
            }
        } else {
            reply
        };
        if self.len < PARKED {
            self.parked[(self.head + self.len) % PARKED] = Some(reply);
            self.len += 1;
        } else {
            *errors += 1;
            std::mem::forget(reply);
        }
    }

    pub(super) fn hand_back(&mut self, tag: u64, reason: HandbackReason, unit: Box<dyn SlotProcessor>, errors: &mut u64) {
        self.send(SlotReply::Handback(Handback { tag, reason, unit }), errors);
    }
}
