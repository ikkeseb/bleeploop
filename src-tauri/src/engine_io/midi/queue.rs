//! OWNS: the one ordered path from every input to the engine's command ring: a
//! bounded FIFO that the router's batches (notes, wheels, the note target), the MIDI-learn actions and
//! the UI's input commands (`input_send`'s looper presses, `Press`, the toggles) all join, in the
//! order they happened, so a pedal and a click keep their order. Its owner drains it into
//! `EngineHost::send`, which keeps the settings memory; the router and the queue sit under one lock.
//!
//! # Rules
//!
//! - **One FIFO, head of line.** A command the ring refuses stays at the head, and nothing behind it
//!   overtakes it: the next drain (after the next input, or the owner's short timer) retries it.
//!   Nothing is stamped (`TimedCommand::frame` `None`), so the ring's admission order is also the
//!   order the engine applies them in.
//! - **Drained only while a device runs.** A stopped engine's ring is drained by nothing, so what
//!   waits for the device waits here, where wheel updates still coalesce and every release has its
//!   slot; the engine (retained, or rebuilt) gets it all, in order, once a device runs.
//! - **Capacity is reserved, not hoped for.** An admitted attack (`NoteOn`) reserves the slot its
//!   `NoteOff` will take, a HOLD press the slot of its release, and one slot each is kept for the next
//!   target change and for a panic (`AllNotesOff`). So releases, a target switch and a panic always go
//!   in, and the router, which forgets a note when it releases it, never forgets one the queue then
//!   refuses. When the unreserved room is gone, a batch that needs some is refused (counted): fresh
//!   attacks, HOLD presses, actions, and a release nothing reserved a slot for.
//! - **A batch goes in whole or not at all** (one input's commands: a re-strike's `NoteOff` and
//!   `NoteOn`, a switch's releases and the switch); a refused batch leaves nothing queued, and the
//!   router undoes the attack that batch carried (`Router::attack_refused`). The drain sends command by
//!   command, so a batch may reach the engine in two blocks: harmless, as a switch releases what sounds
//!   on its own frame (`Rack::select`) and a synth pick releases its SYNTH's held notes itself.
//! - **Routing stays chronological** (one ordered path into the engine). A target change replaces the
//!   last one still queued only while nothing fresh (an attack, an action, a HOLD press) was admitted
//!   after it, so no note is moved onto a target it was not played on; otherwise it goes in at the
//!   tail. The first fresh entry after the last queued change takes the slot the next change will need.
//! - **DEV only (chain mode, `cfg(debug_assertions)`, as the `chain_*` commands): a synth pick**
//!   (`SetUnitParam(_, Instrument, _)`, the router's alone: [`Out::pick`]) has one place kept per slot,
//!   free while no pick of that slot is queued after the last fresh entry (the first fresh entry after
//!   one keeps the place again), and goes in with no device too (it starts nothing). It replaces the
//!   queue's tail entry only when that is a pick of its unit, else it goes in at the tail: never ahead
//!   of a target change or another slot's pick queued after its unit's last one. One whose slot's place
//!   is taken needs free room; the host asks first ([`Queue::takes_pick`], after the router's dedupe),
//!   so the router never forgets a note the queue then refuses. A release build has none of this.
//! - **A panic goes in at the tail, and one still queued leaves its place:** silencing later ends
//!   everything the earlier one would have (a target switch on the way releases what it leaves).
//! - **Wheel updates coalesce:** a new value replaces the queued value of the same wheel when only wheel
//!   updates follow it, never across a note, an action or a target change. One that finds no room is
//!   not queued at its place: its value waits outside the FIFO (the latest wins, counted) and goes in
//!   at the tail as soon as there is room, so the wheel still ends where the player left it.
//! - **Fresh one-shots need a running device** (the no-device rule): while no device runs, or while
//!   paused for a rebuild, attacks, actions and HOLD presses are refused (the router records no owner);
//!   releases, the target and the wheels still go in (every release passes, controller state is kept).
//! - **A rebuild empties the queue** (a rebuild needs no WebView: `pause`, `rebuild`, then `resume` once
//!   the settings replay has run): the old engine's attacks, non-unit actions, HOLD presses and their
//!   releases are discarded (the new engine has no voice or capture for them to end; an old release
//!   would end the new engine's note). Queued unit commands (`Command::unit().is_some()`, a DEV synth
//!   pick among them) and kind toggles (`Command::kind_toggle().is_some()`) stay in queue order for the host's unit-command gate, and the latest target and wheels, queued or
//!   waiting, come back as a [`Fold`] for the settings memory, so they reach the new engine through
//!   its replay alone. Batches are built and admitted under the lock the rebuild takes, so none spans
//!   two engines.

use std::collections::VecDeque;

use lf_engine::{Action, Command, NoteTarget, TimedCommand};
#[cfg(debug_assertions)]
use lf_engine::{UnitId, UnitParam, SLOT_COUNT};

/// The FIFO's bound. It fills only while the engine takes nothing (no device runs, or its ring of 1024
/// commands, `EngineConfig::new`, refuses), so it is sized for what it must honour then: a reserved
/// release for every note (128) and for each HOLD control the engine tells apart (16,
/// `lf_engine::HOLD_CONTROLS`), the next target change and a panic (2), leaving 110 entries of backlog
/// for fresh attacks, actions and wheel updates (DEV: 108, a pick place per slot kept too); past that,
/// input that cannot reach the engine is stale and refused. Preallocated.
pub const CAPACITY: usize = 256;

/// What a command is to the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// `NoteOn`: reserves the slot its release takes.
    Attack,
    /// `NoteOff`: takes its attack's slot.
    Release,
    /// `AllNotesOff`: takes the slot kept for it; one still queued leaves its place.
    Panic,
    /// `PitchBend`, `Modulation`: coalesces.
    Wheel,
    /// `SelectInstrument`: takes the slot kept for it, or replaces the last one queued.
    Target,
    /// DEV: `SetUnitParam(_, Instrument, _)` on the SYNTH heading this slot's chain, a synth pick (the
    /// router's alone, [`Out::pick`]): takes the place kept for its slot, or replaces the tail entry when
    /// that is its unit's pick.
    #[cfg(debug_assertions)]
    Pick(u8),
    /// Every other command: a looper press, `Press`, a toggle.
    Action,
    /// HOLD's press (`Action::Hold`): reserves the slot of its release.
    HoldPress,
    /// HOLD's release (`Action::Release`).
    HoldRelease,
}

impl Class {
    /// A command's class; a synth pick is the router's to class ([`Out::pick`]: its slot).
    pub fn of(command: &Command) -> Class {
        match command {
            Command::NoteOn(..) => Class::Attack,
            Command::NoteOff(_) => Class::Release,
            Command::AllNotesOff => Class::Panic,
            Command::PitchBend(_) | Command::Modulation(_) => Class::Wheel,
            Command::SelectInstrument(_) => Class::Target,
            Command::Action(Action::Hold(_)) | Command::ActionOn(_, Action::Hold(_)) => Class::HoldPress,
            Command::Action(Action::Release(_)) | Command::ActionOn(_, Action::Release(_)) => Class::HoldRelease,
            _ => Class::Action,
        }
    }

    /// A one-shot that starts something: it needs a running device.
    fn fresh(self) -> bool {
        matches!(self, Class::Attack | Class::Action | Class::HoldPress)
    }

    /// A synth pick's slot.
    #[cfg(debug_assertions)]
    fn pick_slot(self) -> Option<u8> {
        match self {
            Class::Pick(slot) => Some(slot),
            _ => None,
        }
    }
}

/// A command for the engine and its class: what the router and MIDI learn hand the queue.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Out {
    pub command: Command,
    pub class: Class,
}

impl Out {
    pub fn new(command: Command) -> Out {
        Out { command, class: Class::of(&command) }
    }

    /// DEV: the router's synth pick, `instrument` (the UI's `SYNTHS` index) on `unit`, the SYNTH heading
    /// `slot`'s chain.
    #[cfg(debug_assertions)]
    pub fn pick(slot: u8, unit: UnitId, instrument: u8) -> Out {
        Out { command: Command::SetUnitParam(unit, UnitParam::Instrument, f64::from(instrument)), class: Class::Pick(slot) }
    }
}

/// Why a batch was refused (nothing of it was queued).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// No device runs: a fresh note-on, action or HOLD press would fire at the next open.
    NoDevice,
    /// A rebuild is under way.
    Paused,
    /// The unreserved room is gone.
    Full,
}

/// What a drain did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drained {
    pub sent: usize,
    /// Still queued.
    pub left: usize,
    /// The ring refused the head (it stays there).
    pub blocked: bool,
}

/// The latest desired target and wheels a rebuild took out of the queue: the settings memory keeps
/// them, so the new engine gets them through its replay.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fold {
    pub target: Option<NoteTarget>,
    pub pitch_bend: Option<f64>,
    pub modulation: Option<f64>,
}

impl Fold {
    /// As the commands that set them.
    pub fn commands(&self) -> impl Iterator<Item = Command> {
        let target = self.target.map(Command::SelectInstrument);
        [target, self.pitch_bend.map(Command::PitchBend), self.modulation.map(Command::Modulation)].into_iter().flatten()
    }
}

/// For the DEV probe and the release log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueCounters {
    pub attacks_refused: u64,
    pub holds_refused: u64,
    pub actions_refused: u64,
    /// Releases no attack or HOLD press reserved a slot for, refused for room.
    pub releases_refused: u64,
    /// Wheel updates that found no room: their value waited outside the FIFO.
    pub wheels_deferred: u64,
    /// Wheel updates merged into a value still queued or waiting.
    pub coalesced: u64,
    /// Target changes that replaced one still queued.
    pub targets_replaced: u64,
    /// DEV: synth picks that replaced their unit's pick at the tail.
    #[cfg(debug_assertions)]
    pub picks_replaced: u64,
    /// Panics that moved one still queued to the tail.
    pub panics_moved: u64,
    /// Queued commands of a replaced engine that never reached it (a rebuild's one-shots and releases).
    pub discarded: u64,
}

struct Entry {
    command: Command,
    class: Class,
}

impl Entry {
    fn new(command: Command, class: Class) -> Entry {
        Entry { command, class }
    }
}

/// A set of 0..=255 (notes, HOLD controls): the ones holding a reserved slot.
#[derive(Clone, Copy, Default)]
struct Reserved([u128; 2]);

impl Reserved {
    fn has(&self, n: u8) -> bool {
        self.0[usize::from(n >> 7)] >> (n & 127) & 1 == 1
    }

    fn set(&mut self, n: u8, on: bool) {
        let bit = 1u128 << (n & 127);
        let word = &mut self.0[usize::from(n >> 7)];
        *word = if on { *word | bit } else { *word & !bit };
    }

    fn count(&self) -> usize {
        (self.0[0].count_ones() + self.0[1].count_ones()) as usize
    }

    /// A press: two slots, its own and its release's, unless it already holds one.
    fn reserve(&mut self, n: u8) -> usize {
        if self.has(n) {
            return 1;
        }
        self.set(n, true);
        2
    }

    /// A release: the slot its press reserved, or a new one.
    fn release(&mut self, n: u8) -> usize {
        if !self.has(n) {
            return 1;
        }
        self.set(n, false);
        0
    }
}

/// What admission spends, walked in batch order: the reservations made or used, and whether a target
/// change would need a new entry (none is queued, or something fresh follows the last one). DEV: which
/// slots' pick places are free, and the unit whose pick the queue's tail entry would be after the
/// entries walked so far (a pick of that unit replaces it).
#[derive(Clone, Copy)]
struct Ledger {
    notes: Reserved,
    holds: Reserved,
    target_open: bool,
    #[cfg(debug_assertions)]
    picks_open: [bool; SLOT_COUNT],
    #[cfg(debug_assertions)]
    tail_pick: Option<UnitId>,
}

impl Ledger {
    /// The free room `out` takes.
    fn cost(&mut self, out: &Out) -> usize {
        let own = match (out.class, out.command) {
            (Class::Attack, Command::NoteOn(n, _)) => self.notes.reserve(n),
            (Class::Release, Command::NoteOff(n)) => self.notes.release(n),
            (Class::HoldPress, c) => hold_control(&c).map_or(1, |c| self.holds.reserve(c)),
            (Class::HoldRelease, c) => hold_control(&c).map_or(1, |c| self.holds.release(c)),
            // The target and a panic take the slot kept for them; a wheel coalesces or waits.
            (Class::Target, _) => {
                // In at the tail only when none is queued since the last fresh entry; else it replaces
                // that one in its place and the tail stays.
                #[cfg(debug_assertions)]
                if self.target_open {
                    self.tail_pick = None;
                }
                self.target_open = false;
                0
            }
            // A pick replaces its unit's at the tail, else takes its slot's place or free room.
            #[cfg(debug_assertions)]
            (Class::Pick(slot), Command::SetUnitParam(unit, ..)) => {
                if self.tail_pick == Some(unit) {
                    0
                } else {
                    self.tail_pick = Some(unit);
                    match self.picks_open.get_mut(usize::from(slot)) {
                        Some(open) if *open => {
                            *open = false;
                            0
                        }
                        _ => 1,
                    }
                }
            }
            (Class::Panic | Class::Wheel, _) => 0,
            _ => 1,
        };
        // Anything but a pick or a target change in its place moves the tail (a wheel may not, which
        // only overstates a later pick's cost).
        #[cfg(debug_assertions)]
        if !matches!(out.class, Class::Pick(_) | Class::Target) {
            self.tail_pick = None;
        }
        if !out.class.fresh() {
            return own;
        }
        // The first fresh entry after the last target change keeps a slot for the next one (DEV: and
        // each slot's pick place).
        let reopened = usize::from(!self.target_open);
        self.target_open = true;
        #[cfg(debug_assertions)]
        let reopened = reopened + std::mem::replace(&mut self.picks_open, [true; SLOT_COUNT]).iter().filter(|open| !**open).count();
        own + reopened
    }
}

/// HOLD's control, for its press and its release.
fn hold_control(command: &Command) -> Option<u8> {
    match command {
        Command::Action(Action::Hold(c) | Action::Release(c)) | Command::ActionOn(_, Action::Hold(c) | Action::Release(c)) => Some(*c),
        _ => None,
    }
}

/// A wheel update as (wheel, value): 0 the pitch wheel, 1 the mod wheel.
fn wheel(command: &Command) -> Option<(usize, f64)> {
    match *command {
        Command::PitchBend(v) => Some((0, v)),
        Command::Modulation(v) => Some((1, v)),
        _ => None,
    }
}

fn wheel_command(wheel: usize, value: f64) -> Command {
    if wheel == 0 {
        Command::PitchBend(value)
    } else {
        Command::Modulation(value)
    }
}

pub struct Queue {
    entries: VecDeque<Entry>,
    paused: bool,
    /// Notes whose attack went in and whose release has not: each holds a reserved slot.
    notes: Reserved,
    /// HOLD controls likewise.
    holds: Reserved,
    /// A wheel value that found no room, by wheel.
    waiting: [Option<f64>; 2],
    counters: QueueCounters,
}

impl Default for Queue {
    fn default() -> Self {
        Queue::new()
    }
}

impl Queue {
    /// An empty queue.
    pub fn new() -> Queue {
        Queue {
            entries: VecDeque::with_capacity(CAPACITY),
            paused: false,
            notes: Reserved::default(),
            holds: Reserved::default(),
            waiting: [None; 2],
            counters: QueueCounters::default(),
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn counters(&self) -> QueueCounters {
        self.counters
    }

    /// A target change would need a new entry: none is queued, or something fresh follows the last.
    fn target_open(&self) -> bool {
        self.open(Class::Target)
    }

    /// DEV: `slot`'s pick place is free: no pick of `slot` is queued after the last fresh entry.
    #[cfg(debug_assertions)]
    fn pick_open(&self, slot: u8) -> bool {
        self.open(Class::Pick(slot))
    }

    /// DEV: each slot's pick place, free or taken.
    #[cfg(debug_assertions)]
    fn picks_open(&self) -> [bool; SLOT_COUNT] {
        std::array::from_fn(|s| self.pick_open(s as u8))
    }

    /// DEV: the unit whose pick the tail entry is: a pick of that unit replaces it.
    #[cfg(debug_assertions)]
    fn tail_pick(&self) -> Option<UnitId> {
        self.entries.back().filter(|e| e.class.pick_slot().is_some()).and_then(|e| e.command.unit())
    }

    /// No entry of `class` is queued after the last fresh entry.
    fn open(&self, class: Class) -> bool {
        for e in self.entries.iter().rev() {
            if e.class == class {
                return false;
            }
            if e.class.fresh() {
                return true;
            }
        }
        true
    }

    /// DEV: whether the router's pick of `unit` on `slot` would go in now: it replaces its unit's pick at
    /// the tail, takes `slot`'s place, or finds free room a waiting wheel value does not take first
    /// (`admit` places those before its batch). The rest of a pick's batch costs nothing (its releases
    /// and its switch hold reserved slots; with its unit's pick at the tail there is nothing to release
    /// and a switch goes in that pick's place), so the host asks this, after the router's dedupe and
    /// before the router runs, and the batch is then taken.
    #[cfg(debug_assertions)]
    pub fn takes_pick(&self, slot: u8, unit: UnitId) -> bool {
        self.tail_pick() == Some(unit) || self.pick_open(slot) || self.free() > self.waiting.iter().flatten().count()
    }

    fn panic_queued(&self) -> bool {
        self.entries.iter().any(|e| e.class == Class::Panic)
    }

    /// Slots promised to releases not yet admitted, to the next target change and to a panic (DEV: and
    /// to each slot's next synth pick).
    fn reserved(&self) -> usize {
        let reserved = self.notes.count() + self.holds.count() + usize::from(self.target_open()) + usize::from(!self.panic_queued());
        #[cfg(debug_assertions)]
        return reserved + self.picks_open().into_iter().filter(|&open| open).count();
        #[cfg(not(debug_assertions))]
        reserved
    }

    /// Room no reservation holds.
    fn free(&self) -> usize {
        CAPACITY.saturating_sub(self.entries.len() + self.reserved())
    }

    fn ledger(&self) -> Ledger {
        Ledger {
            notes: self.notes,
            holds: self.holds,
            target_open: self.target_open(),
            #[cfg(debug_assertions)]
            picks_open: self.picks_open(),
            #[cfg(debug_assertions)]
            tail_pick: self.tail_pick(),
        }
    }

    /// One input's commands: queued whole, or refused whole.
    pub fn admit(&mut self, batch: &[Out], device_running: bool) -> Result<(), Refused> {
        if batch.is_empty() {
            return Ok(());
        }
        if batch.iter().any(|o| o.class.fresh()) && (self.paused || !device_running) {
            self.count_refused(batch);
            return Err(if self.paused { Refused::Paused } else { Refused::NoDevice });
        }
        // What waited is older than this batch: it goes in first.
        self.place_waiting();
        let mut ledger = self.ledger();
        let mut owed: usize = batch.iter().map(|o| ledger.cost(o)).sum();
        if owed > self.free() {
            self.count_refused(batch);
            return Err(Refused::Full);
        }
        for out in batch {
            if out.class == Class::Wheel {
                self.push_wheel(out.command, owed);
                continue;
            }
            // Never more than the walk above counted for it: the queue now is what that walk assumed.
            let mut ledger = self.ledger();
            owed -= ledger.cost(out);
            (self.notes, self.holds) = (ledger.notes, ledger.holds);
            self.push(out);
        }
        debug_assert!(self.entries.len() + self.reserved() <= CAPACITY, "a reservation was overbooked");
        Ok(())
    }

    /// Queue `out` (its room already counted): a target change replaces the last one queued while
    /// nothing fresh follows it; a panic moves one still queued to the tail. DEV: a synth pick replaces
    /// the tail entry when that is its unit's pick.
    fn push(&mut self, out: &Out) {
        match out.class {
            #[cfg(debug_assertions)]
            Class::Pick(_) => {
                if let Some(last) = self.entries.back_mut().filter(|e| e.class.pick_slot().is_some() && e.command.unit() == out.command.unit()) {
                    last.command = out.command;
                    self.counters.picks_replaced += 1;
                    return;
                }
            }
            Class::Target if !self.target_open() => {
                if let Some(k) = self.entries.iter().rposition(|e| e.class == Class::Target) {
                    self.entries[k].command = out.command;
                    self.counters.targets_replaced += 1;
                    return;
                }
            }
            Class::Panic => {
                if let Some(k) = self.entries.iter().position(|e| e.class == Class::Panic) {
                    self.entries.remove(k);
                    self.counters.panics_moved += 1;
                }
            }
            _ => {}
        }
        self.entries.push_back(Entry::new(out.command, out.class));
    }

    /// A wheel update: merged into the same wheel's value still waiting or queued behind nothing but
    /// wheel updates, else queued if the room `owed` to the rest of its batch leaves a slot, else left
    /// waiting.
    fn push_wheel(&mut self, command: Command, owed: usize) {
        let Some((wheel, value)) = wheel(&command) else { return };
        if self.waiting[wheel].is_some() {
            self.waiting[wheel] = Some(value);
            self.counters.coalesced += 1;
        } else if let Some(k) = self.coalescible(wheel) {
            self.entries[k].command = command;
            self.counters.coalesced += 1;
        } else if self.free() > owed {
            self.entries.push_back(Entry::new(command, Class::Wheel));
        } else {
            self.waiting[wheel] = Some(value);
            self.counters.wheels_deferred += 1;
        }
    }

    /// The queued update of `wheel` that only wheel updates follow.
    fn coalescible(&self, wheel_index: usize) -> Option<usize> {
        for (k, e) in self.entries.iter().enumerate().rev() {
            if e.class != Class::Wheel {
                return None;
            }
            if wheel(&e.command).is_some_and(|(w, _)| w == wheel_index) {
                return Some(k);
            }
        }
        None
    }

    /// Queue the wheel values that wait, as room allows. True when something went in.
    fn place_waiting(&mut self) -> bool {
        let mut placed = false;
        for wheel in 0..2 {
            let Some(value) = self.waiting[wheel] else { continue };
            let command = wheel_command(wheel, value);
            if let Some(k) = self.coalescible(wheel) {
                self.entries[k].command = command;
            } else if self.free() > 0 {
                self.entries.push_back(Entry::new(command, Class::Wheel));
            } else {
                continue;
            }
            self.waiting[wheel] = None;
            placed = true;
        }
        placed
    }

    fn count_refused(&mut self, batch: &[Out]) {
        let fresh = batch.iter().any(|o| o.class.fresh());
        let c = &mut self.counters;
        for out in batch {
            match out.class {
                Class::Attack => c.attacks_refused += 1,
                Class::HoldPress => c.holds_refused += 1,
                Class::Action => c.actions_refused += 1,
                // A release refused with an attack in its batch ends nothing that sounds.
                Class::Release | Class::HoldRelease if !fresh => c.releases_refused += 1,
                _ => {}
            }
        }
    }

    /// Send what the ring takes, in order, stopping at the first refusal (the head stays). Then what
    /// waited goes in, if the sent ones made room. Nothing while paused.
    pub fn drain(&mut self, send: &mut dyn FnMut(TimedCommand) -> Result<(), ()>) -> Drained {
        let mut drained = Drained::default();
        while !self.paused {
            while let Some(e) = self.entries.front() {
                let command = e.command;
                if send(TimedCommand { frame: None, command }).is_err() {
                    drained.blocked = true;
                    break;
                }
                self.entries.pop_front();
                drained.sent += 1;
            }
            if !self.place_waiting() || drained.blocked {
                break;
            }
        }
        drained.left = self.entries.len();
        drained
    }

    /// An engine rebuild starts: no fresh one-shot goes in and nothing drains until [`Queue::resume`].
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /// The engine was replaced: empty the queue, discarding the old engine's non-unit one-shots and
    /// their releases, and hand back the latest target and wheels (queued or waiting, as a [`Fold`] for
    /// the settings memory) beside every queued unit command and kind toggle in queue order (for the
    /// host's unit-command gate).
    pub fn rebuild(&mut self) -> (Fold, Vec<Command>) {
        let mut fold = Fold::default();
        let mut units = Vec::new();
        let mut discarded = 0;
        for e in self.entries.drain(..) {
            match e.command {
                Command::SelectInstrument(target) => fold.target = Some(target),
                Command::PitchBend(v) => fold.pitch_bend = Some(v),
                Command::Modulation(v) => fold.modulation = Some(v),
                c if c.unit().is_some() || c.kind_toggle().is_some() => units.push(c),
                _ => discarded += 1,
            }
        }
        // What waits is newer than anything queued: each admission and drain places it first.
        fold.pitch_bend = self.waiting[0].take().or(fold.pitch_bend);
        fold.modulation = self.waiting[1].take().or(fold.modulation);
        self.notes = Reserved::default();
        self.holds = Reserved::default();
        self.counters.discarded += discarded;
        (fold, units)
    }

    /// The settings replay has run: admission and draining go on.
    pub fn resume(&mut self) {
        self.paused = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_engine::{InputSendParam, Instrument, UnitParam};

    /// The places kept for synth picks: one per slot in a DEV build, none in a release build.
    const PICK_PLACES: usize = if cfg!(debug_assertions) { lf_engine::SLOT_COUNT } else { 0 };

    const LEAD: NoteTarget = NoteTarget::Builtin(Instrument::Lead);
    const PAD: NoteTarget = NoteTarget::Builtin(Instrument::Pad);

    fn outs(commands: &[Command]) -> Vec<Out> {
        commands.iter().copied().map(Out::new).collect()
    }

    fn admit(q: &mut Queue, commands: &[Command]) -> Result<(), Refused> {
        q.admit(&outs(commands), true)
    }

    /// Drain into a ring that takes `room` commands, returning what it took.
    fn take(q: &mut Queue, room: usize) -> Vec<Command> {
        let mut taken = Vec::new();
        q.drain(&mut |c| {
            assert!(c.frame.is_none(), "nothing is stamped");
            if taken.len() == room {
                return Err(());
            }
            taken.push(c.command);
            Ok(())
        });
        taken
    }

    /// Admit one-shot actions until the room is gone; how many went in.
    fn fill(q: &mut Queue) -> usize {
        let mut n = 0;
        while q.admit(&outs(&[Command::Action(Action::Undo)]), true).is_ok() {
            n += 1;
        }
        n
    }

    #[test]
    fn each_command_has_its_class() {
        let cases = [
            (Command::NoteOn(60, 0.5), Class::Attack),
            (Command::NoteOff(60), Class::Release),
            (Command::AllNotesOff, Class::Panic),
            (Command::PitchBend(1.0), Class::Wheel),
            (Command::Modulation(0.5), Class::Wheel),
            (Command::SelectInstrument(LEAD), Class::Target),
            (Command::SetUnitParam(7, UnitParam::Instrument, 2.0), Class::Action),
            (Command::SetUnitParam(7, UnitParam::Gain(Instrument::Pad), 0.5), Class::Action),
            (Command::Action(Action::Hold(3)), Class::HoldPress),
            (Command::ActionOn(1, Action::Hold(3)), Class::HoldPress),
            (Command::Action(Action::Release(3)), Class::HoldRelease),
            (Command::ActionOn(1, Action::Release(3)), Class::HoldRelease),
            (Command::Action(Action::RecDub), Class::Action),
            (Command::Press, Class::Action),
            (Command::SelectTrack(2), Class::Action),
        ];
        // A synth pick is classed by the router, which knows its slot (`Out::pick`); a release build has none.
        #[cfg(debug_assertions)]
        assert_eq!(Out::pick(1, 7, 2), Out { command: Command::SetUnitParam(7, UnitParam::Instrument, 2.0), class: Class::Pick(1) });
        for (command, class) in cases {
            assert_eq!(Class::of(&command), class, "{command:?}");
        }
    }

    // A command the ring refuses stays at the head and nothing behind it overtakes it.
    #[test]
    fn a_refused_head_stays_at_the_head_and_nothing_overtakes_it() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        admit(&mut q, &[Command::Action(Action::RecDub)]).unwrap();
        admit(&mut q, &[Command::NoteOff(60)]).unwrap();
        let mut tries = 0;
        let d = q.drain(&mut |_| {
            tries += 1;
            Err(())
        });
        assert_eq!((d, tries), (Drained { sent: 0, left: 3, blocked: true }, 1), "one try, at the head");
        assert_eq!(take(&mut q, 1), [Command::NoteOn(60, 0.5)]);
        assert_eq!(take(&mut q, 8), [Command::Action(Action::RecDub), Command::NoteOff(60)]);
        assert!(q.is_empty());
    }

    // A batch (a re-strike's NoteOff and NoteOn) is accepted whole or not at all.
    #[test]
    fn a_batch_goes_in_whole_or_not_at_all() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::NoteOn(62, 0.5)]).unwrap();
        fill(&mut q);
        let restrike = [Command::NoteOff(62), Command::NoteOn(62, 0.7)];
        let before = q.len();
        assert_eq!(admit(&mut q, &restrike), Err(Refused::Full));
        assert_eq!(q.len(), before, "not even the release went in");
        assert_eq!(q.counters().attacks_refused, 1);
        assert_eq!(q.counters().releases_refused, 0, "a release refused with its attack ends nothing");
        // The release alone still has its slot.
        admit(&mut q, &[Command::NoteOff(62)]).unwrap();
    }

    // Accepting an attack or a HOLD reserves the slot its release will need.
    #[test]
    fn an_attack_and_a_hold_press_reserve_their_release() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        admit(&mut q, &[Command::Action(Action::Hold(0))]).unwrap();
        assert_eq!(take(&mut q, 8).len(), 2, "both reached the engine; their reservations stay");
        let filled = fill(&mut q);
        assert_eq!(filled, CAPACITY - 4 - PICK_PLACES, "the two releases, the next target change, a panic (DEV: each slot's next pick) keep their slots");
        // One slot free: an action fits; an attack or a HOLD press does not, needing its release's too.
        take(&mut q, 1);
        assert_eq!(admit(&mut q, &[Command::NoteOn(61, 0.5)]), Err(Refused::Full));
        assert_eq!(admit(&mut q, &[Command::Action(Action::Hold(1))]), Err(Refused::Full));
        assert_eq!(admit(&mut q, &[Command::Action(Action::Undo)]), Ok(()));
        assert_eq!(admit(&mut q, &[Command::Action(Action::Release(0))]), Ok(()));
        assert_eq!(admit(&mut q, &[Command::NoteOff(60)]), Ok(()));
        assert_eq!(admit(&mut q, &[Command::SelectInstrument(LEAD)]), Ok(()));
        assert_eq!(admit(&mut q, &[Command::AllNotesOff]), Ok(()));
        #[cfg(debug_assertions)]
        for (slot, unit) in [(0, 7), (1, 9)] {
            assert_eq!(q.admit(&[Out::pick(slot, unit, 1)], true), Ok(()), "slot {slot}'s pick place");
        }
        assert_eq!(q.len(), CAPACITY);
        // A release nothing reserved finds no room.
        assert_eq!(admit(&mut q, &[Command::NoteOff(70)]), Err(Refused::Full));
        let c = q.counters();
        assert_eq!((c.attacks_refused, c.holds_refused, c.actions_refused, c.releases_refused), (1, 1, 1, 1));
    }

    // A target change replaces a target change still queued, bounded by routing staying chronological:
    // only while nothing fresh was admitted after it, so a note played on one target never moves onto
    // the next. Either way a switch always goes in.
    #[test]
    fn a_target_change_replaces_one_still_queued_only_while_nothing_fresh_follows() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::NoteOff(60), Command::SelectInstrument(LEAD)]).unwrap();
        admit(&mut q, &[Command::SelectInstrument(PAD)]).unwrap();
        admit(&mut q, &[Command::NoteOn(62, 0.5)]).unwrap();
        admit(&mut q, &[Command::NoteOff(62), Command::SelectInstrument(LEAD)]).unwrap();
        assert_eq!(
            take(&mut q, 8),
            [Command::NoteOff(60), Command::SelectInstrument(PAD), Command::NoteOn(62, 0.5), Command::NoteOff(62), Command::SelectInstrument(LEAD)],
            "62 sounds and ends on the target it was played on"
        );
        assert_eq!(q.counters().targets_replaced, 1);
        // With the room gone, a switch still goes in after the actions that filled it, and the next one
        // replaces it.
        admit(&mut q, &[Command::SelectInstrument(PAD)]).unwrap();
        let filled = fill(&mut q);
        assert_eq!(filled, CAPACITY - 1 - 2 - PICK_PLACES, "the first action after the queued switch kept a slot for the next");
        assert_eq!(admit(&mut q, &[Command::SelectInstrument(LEAD)]), Ok(()));
        assert_eq!(admit(&mut q, &[Command::SelectInstrument(PAD)]), Ok(()));
        assert_eq!(q.len(), CAPACITY - 1 - PICK_PLACES, "only the panic's slot (DEV: and the pick places) is left");
        let all = take(&mut q, CAPACITY);
        assert_eq!((all.first(), all.last()), (Some(&Command::SelectInstrument(PAD)), Some(&Command::SelectInstrument(PAD))));
        assert_eq!(all.iter().filter(|c| matches!(c, Command::SelectInstrument(_))).count(), 2);
    }

    // Review fix: a panic (AllNotesOff) always goes in, even with the room gone; a second one moves the
    // queued one to the tail, where it ends everything the first would have.
    #[test]
    fn a_panic_always_goes_in_and_moves_one_still_queued_to_the_tail() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        admit(&mut q, &[Command::NoteOff(60), Command::AllNotesOff]).unwrap();
        admit(&mut q, &[Command::NoteOn(61, 0.5)]).unwrap();
        fill(&mut q);
        assert_eq!(admit(&mut q, &[Command::NoteOff(61), Command::AllNotesOff]), Ok(()));
        assert_eq!(q.counters().panics_moved, 1);
        let all = take(&mut q, CAPACITY);
        assert_eq!(all.iter().filter(|c| **c == Command::AllNotesOff).count(), 1);
        assert_eq!(&all[all.len() - 2..], [Command::NoteOff(61), Command::AllNotesOff]);
        assert_eq!(&all[..3], [Command::NoteOn(60, 0.5), Command::NoteOff(60), Command::NoteOn(61, 0.5)]);
    }

    // Wheel updates coalesce, never across a note or an action.
    #[test]
    fn wheel_updates_coalesce_but_never_across_a_note_or_an_action() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::PitchBend(0.1)]).unwrap();
        admit(&mut q, &[Command::Modulation(0.1)]).unwrap();
        admit(&mut q, &[Command::PitchBend(0.2)]).unwrap();
        admit(&mut q, &[Command::Modulation(0.2)]).unwrap();
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        admit(&mut q, &[Command::PitchBend(0.3)]).unwrap();
        admit(&mut q, &[Command::Action(Action::RecDub)]).unwrap();
        admit(&mut q, &[Command::PitchBend(0.4)]).unwrap();
        admit(&mut q, &[Command::PitchBend(0.5)]).unwrap();
        assert_eq!(
            take(&mut q, 16),
            [
                Command::PitchBend(0.2),
                Command::Modulation(0.2),
                Command::NoteOn(60, 0.5),
                Command::PitchBend(0.3),
                Command::Action(Action::RecDub),
                Command::PitchBend(0.5),
            ]
        );
        assert_eq!(q.counters().coalesced, 3);
    }

    // With the unreserved room gone a wheel update is refused its place; its value waits and goes in at
    // the tail once there is room, so the wheel ends where it was left.
    #[test]
    fn a_wheel_update_with_no_room_waits_and_goes_in_when_room_frees() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        fill(&mut q);
        assert_eq!(admit(&mut q, &[Command::PitchBend(1.0)]), Ok(()));
        assert_eq!(admit(&mut q, &[Command::PitchBend(1.5)]), Ok(()));
        assert_eq!((q.counters().wheels_deferred, q.counters().coalesced), (1, 1));
        let first = take(&mut q, 4);
        assert_eq!(first.len(), 4);
        assert_eq!(q.entries.back().map(|e| e.command), Some(Command::PitchBend(1.5)), "in at the tail once room freed");
        let rest = take(&mut q, CAPACITY);
        assert_eq!(rest.last(), Some(&Command::PitchBend(1.5)));
        assert_eq!(rest.iter().filter(|c| matches!(c, Command::PitchBend(_))).count(), 1);
    }

    // The no-device rule: with no device running, fresh note-ons, actions and HOLD presses are dropped,
    // controller state is kept, and every release passes.
    #[test]
    fn with_no_device_fresh_one_shots_are_refused_and_releases_and_controller_state_pass() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        admit(&mut q, &[Command::Action(Action::Hold(2))]).unwrap();
        let gap = |q: &mut Queue, c: &[Command]| q.admit(&outs(c), false);
        assert_eq!(gap(&mut q, &[Command::NoteOn(61, 0.5)]), Err(Refused::NoDevice));
        assert_eq!(gap(&mut q, &[Command::Action(Action::RecDub)]), Err(Refused::NoDevice));
        assert_eq!(gap(&mut q, &[Command::Action(Action::Hold(3))]), Err(Refused::NoDevice));
        assert_eq!(gap(&mut q, &[Command::NoteOff(60)]), Ok(()));
        assert_eq!(gap(&mut q, &[Command::Action(Action::Release(2))]), Ok(()));
        assert_eq!(gap(&mut q, &[Command::PitchBend(0.5)]), Ok(()));
        assert_eq!(gap(&mut q, &[Command::SelectInstrument(PAD)]), Ok(()));
        let c = q.counters();
        assert_eq!((c.attacks_refused, c.actions_refused, c.holds_refused), (1, 1, 1));
        assert_eq!(q.len(), 6);
    }

    // A rebuild needs no WebView: pause stops admission of one-shots and draining; a rebuild empties
    // the queue, discarding the old engine's one-shots and their releases and folding the latest target
    // and wheels.
    #[test]
    fn a_rebuild_discards_the_old_one_shots_and_folds_the_target_and_wheels() {
        let mut q = Queue::new();
        admit(&mut q, &[Command::SelectInstrument(LEAD)]).unwrap();
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        admit(&mut q, &[Command::Action(Action::Hold(0))]).unwrap();
        admit(&mut q, &[Command::PitchBend(0.5)]).unwrap();
        admit(&mut q, &[Command::NoteOff(60), Command::SelectInstrument(PAD)]).unwrap();
        admit(&mut q, &[Command::Modulation(0.25)]).unwrap();
        q.pause();
        assert_eq!(admit(&mut q, &[Command::NoteOn(61, 0.5)]), Err(Refused::Paused));
        assert_eq!(admit(&mut q, &[Command::Action(Action::Release(0))]), Ok(()), "a release passes");
        assert_eq!(q.drain(&mut |_| panic!("nothing drains while paused")), Drained { sent: 0, left: 8, blocked: false });
        let (fold, units) = q.rebuild();
        assert_eq!(fold, Fold { target: Some(PAD), pitch_bend: Some(0.5), modulation: Some(0.25) });
        assert!(units.is_empty());
        assert_eq!(fold.commands().collect::<Vec<_>>(), [Command::SelectInstrument(PAD), Command::PitchBend(0.5), Command::Modulation(0.25)]);
        assert_eq!(q.counters().discarded, 4, "NoteOn, Hold, NoteOff, Release");
        assert!(q.is_empty());
        assert_eq!(admit(&mut q, &[Command::NoteOn(62, 0.5)]), Err(Refused::Paused));
        q.resume();
        assert_eq!(admit(&mut q, &[Command::NoteOn(62, 0.5)]), Ok(()));
        assert_eq!(take(&mut q, 8), [Command::NoteOn(62, 0.5)]);
        // The old engine's reservations went with it: the room is whole again but for 62's release, the
        // next target change and a panic (DEV: and each slot's next pick).
        assert_eq!(fill(&mut q), CAPACITY - 3 - PICK_PLACES);
    }

    // A wheel value waiting for room when the engine is rebuilt is the newest: it is the one folded.
    #[test]
    fn a_waiting_wheel_value_is_folded_into_a_rebuild() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::PitchBend(0.1)]).unwrap();
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        fill(&mut q);
        admit(&mut q, &[Command::PitchBend(0.9)]).unwrap();
        q.pause();
        assert_eq!(q.rebuild().0.pitch_bend, Some(0.9));
        assert!(q.is_empty());
    }

    // Unit commands survive a rebuild in queue order for the host's gate, without being counted as
    // discarded; only the old engine's one-shots (notes, HOLD presses) are discarded.
    #[test]
    fn a_rebuild_keeps_unit_commands_in_order_and_discards_only_old_one_shots() {
        let mut q = Queue::new();
        let bypass = Command::SetUnitBypass(7, true);
        let toggle = Command::ActionOn(1, Action::ToggleUnit(9));
        let param = Command::SetUnitParam(7, UnitParam::Send(InputSendParam::EchoLevel), 0.5);
        let kind = Command::Action(Action::Toggle(lf_engine::Toggle::Units(lf_engine::UnitKind::Echo)));
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        admit(&mut q, &[bypass]).unwrap();
        admit(&mut q, &[Command::Action(Action::Hold(0))]).unwrap();
        admit(&mut q, &[kind]).unwrap();
        admit(&mut q, &[toggle]).unwrap();
        admit(&mut q, &[param]).unwrap();
        admit(&mut q, &[Command::PitchBend(0.5)]).unwrap();
        q.pause();
        let (fold, units) = q.rebuild();
        assert_eq!(fold, Fold { target: None, pitch_bend: Some(0.5), modulation: None });
        assert_eq!(units, [bypass, kind, toggle, param], "a kind toggle kept in its place");
        assert_eq!(q.counters().discarded, 2, "only NoteOn and Hold are discarded");
        assert!(q.is_empty());
    }

    #[cfg(debug_assertions)]
    fn pick(unit: UnitId, instrument: u8) -> Command {
        Command::SetUnitParam(unit, UnitParam::Instrument, f64::from(instrument))
    }

    /// One batch: `commands`, then the router's pick of `instrument` on `unit`, the SYNTH of `slot`.
    #[cfg(debug_assertions)]
    fn with_pick(commands: &[Command], slot: u8, unit: UnitId, instrument: u8) -> Vec<Out> {
        let mut batch = outs(commands);
        batch.push(Out::pick(slot, unit, instrument));
        batch
    }

    // DEV: a synth pick takes its slot's place, with no device too (it starts nothing): with the room
    // gone its batch still goes in whole, behind a switch, and the router may forget the notes it
    // releases. Its unit's next pick replaces it only while it is the tail; the other slot has a place of
    // its own; a pick whose place is taken and whose unit's pick is not the tail needs free room, which
    // the host asks for first (`takes_pick`).
    #[cfg(debug_assertions)]
    #[test]
    fn a_pick_takes_its_slots_place_with_no_device_and_replaces_only_its_units_pick_at_the_tail() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        admit(&mut q, &[Command::NoteOn(62, 0.5)]).unwrap();
        fill(&mut q);
        assert_eq!(q.free(), 0);
        let gap = |q: &mut Queue, batch: &[Out]| q.admit(batch, false);
        assert!(q.takes_pick(0, 7));
        let batch = with_pick(&[Command::NoteOff(60), Command::NoteOff(62), Command::SelectInstrument(NoteTarget::Slot(0))], 0, 7, 2);
        assert_eq!(gap(&mut q, &batch), Ok(()), "a pick's batch goes in whole, the room gone and no device");
        assert!(q.takes_pick(0, 7), "its unit's pick is the tail");
        assert_eq!(gap(&mut q, &[Out::pick(0, 7, 4)]), Ok(()));
        assert_eq!(q.counters().picks_replaced, 1);
        assert!(q.takes_pick(1, 9), "the other slot's place");
        assert_eq!(gap(&mut q, &with_pick(&[Command::SelectInstrument(NoteTarget::Slot(1))], 1, 9, 1)), Ok(()));
        assert!(!q.takes_pick(0, 7), "slot 0's place is taken and its pick is no longer the tail");
        assert_eq!(gap(&mut q, &[Out::pick(0, 7, 5)]), Err(Refused::Full), "what the host's ask spares the router");
        assert_eq!(gap(&mut q, &outs(&[Command::AllNotesOff])), Ok(()));
        assert_eq!(q.len(), CAPACITY, "every reserved slot taken");
        let all = take(&mut q, CAPACITY);
        // The second switch replaced the first in its place (nothing fresh between: the target rule).
        assert_eq!(
            &all[all.len() - 6..],
            [Command::NoteOff(60), Command::NoteOff(62), Command::SelectInstrument(NoteTarget::Slot(1)), pick(7, 4), pick(9, 1), Command::AllNotesOff]
        );
        assert_eq!(all.iter().filter(|c| matches!(c, Command::SetUnitParam(..))).count(), 2);
        assert!(q.takes_pick(0, 7), "emptied, its place is free again");
    }

    // DEV, review fix: picks stay chronological. A pick after a switch or after the other slot's pick goes
    // in at the tail (unit 7's second pick reaches the engine after the switch back to its slot and after
    // unit 9's), and one queued before a note is never replaced by one after it (the note sounds on the
    // instrument it was played on). The first fresh entry after them keeps both slots' places again.
    #[cfg(debug_assertions)]
    #[test]
    fn a_pick_goes_in_at_the_tail_behind_a_switch_another_slots_pick_or_a_note() {
        let mut q = Queue::default();
        q.admit(&[Out::pick(0, 7, 1)], true).unwrap();
        q.admit(&with_pick(&[Command::SelectInstrument(NoteTarget::Slot(1))], 1, 9, 1), true).unwrap();
        q.admit(&with_pick(&[Command::SelectInstrument(NoteTarget::Slot(0))], 0, 7, 2), true).unwrap();
        assert_eq!(q.counters().picks_replaced, 0);
        assert_eq!(take(&mut q, 16), [pick(7, 1), Command::SelectInstrument(NoteTarget::Slot(0)), pick(9, 1), pick(7, 2)]);

        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        q.admit(&with_pick(&[Command::NoteOff(60)], 0, 7, 3), true).unwrap();
        q.admit(&[Out::pick(0, 7, 4)], true).unwrap();
        assert_eq!(q.counters().picks_replaced, 1, "the tail was unit 7's pick");
        assert_eq!(take(&mut q, 16), [Command::NoteOn(60, 0.5), Command::NoteOff(60), pick(7, 4)], "60 sounded on pick 2");

        // With the room gone right after a fresh entry, each slot's next pick still has its place.
        admit(&mut q, &[Command::NoteOn(61, 0.5)]).unwrap();
        fill(&mut q);
        assert!(q.takes_pick(0, 11) && q.takes_pick(1, 12));
        assert_eq!(q.admit(&[Out::pick(0, 11, 0)], true), Ok(()));
        assert_eq!(q.admit(&[Out::pick(1, 12, 0)], true), Ok(()));
        assert!(!q.takes_pick(0, 13));
    }

    // DEV: a rebuild keeps a queued pick in queue order with the other unit commands for the host's gate
    // (its unit reseats in the new engine), never folded with the target nor discarded; a pick goes in
    // while paused too.
    #[cfg(debug_assertions)]
    #[test]
    fn a_rebuild_keeps_a_queued_pick_for_the_unit_gate() {
        let mut q = Queue::new();
        let bypass = Command::SetUnitBypass(7, true);
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        q.admit(&with_pick(&[Command::NoteOff(60), Command::SelectInstrument(NoteTarget::Slot(1))], 1, 9, 2), true).unwrap();
        admit(&mut q, &[bypass]).unwrap();
        q.pause();
        assert_eq!(q.admit(&[Out::pick(0, 7, 4)], true), Ok(()), "a pick goes in during a rebuild too");
        let (fold, units) = q.rebuild();
        assert_eq!(fold.target, Some(NoteTarget::Slot(1)));
        assert_eq!(units, [pick(9, 2), bypass, pick(7, 4)]);
        assert_eq!(q.counters().discarded, 2, "NoteOn and NoteOff");
        q.resume();
        assert!(q.is_empty());
    }
}

/// The ordering and overload cases, router and queue together, against a model
/// of what the engine does with what it is sent.
#[cfg(test)]
mod pipeline {
    use super::super::router::{Owner, Router};
    use super::*;
    use lf_engine::grid::Frame;
    use lf_engine::{Engine as RealEngine, EngineConfig, EngineHandle, Instrument, ProcessContext};

    const LEAD: NoteTarget = NoteTarget::Builtin(Instrument::Lead);
    const PAD: NoteTarget = NoteTarget::Builtin(Instrument::Pad);

    fn midi(conn: u32, channel: u8) -> Owner {
        Owner::Midi { conn, channel }
    }

    fn ui(epoch: u64, id: &str) -> Owner {
        Owner::Ui { epoch, id: id.into() }
    }

    /// What the engine does with the commands it applies, as far as these tests look: a switch releases
    /// what the target it leaves sounds (`Instruments::select`, `Rack::select`).
    #[derive(Default)]
    struct Engine {
        target: Option<NoteTarget>,
        sounding: Vec<(Option<NoteTarget>, u8)>,
        pitch_bend: f64,
        modulation: f64,
        holds: Vec<u8>,
        log: Vec<Command>,
    }

    impl Engine {
        fn apply(&mut self, command: Command) {
            self.log.push(command);
            match command {
                Command::SelectInstrument(target) => {
                    let leaving = self.target;
                    self.sounding.retain(|(on, _)| *on != leaving);
                    self.target = Some(target);
                }
                Command::NoteOn(note, _) => self.sounding.push((self.target, note)),
                Command::NoteOff(note) => {
                    let target = self.target;
                    self.sounding.retain(|s| *s != (target, note));
                }
                Command::PitchBend(v) => self.pitch_bend = v,
                Command::Modulation(v) => self.modulation = v,
                Command::Action(Action::Hold(c)) => self.holds.push(c),
                Command::Action(Action::Release(c)) => self.holds.retain(|h| *h != c),
                _ => {}
            }
        }

        fn notes(&self) -> Vec<(Option<NoteTarget>, u8)> {
            self.sounding.clone()
        }
    }

    /// The glue the next phase builds, as these tests need it: one input's batch goes to the queue
    /// whole, a refused attack is undone in the router, and the queue drains into a ring that takes
    /// `room` commands a drain (the engine's 64-entry table) while a device runs.
    struct Rig {
        router: Router,
        queue: Queue,
        engine: Engine,
        running: bool,
        room: usize,
        blocked_drains: usize,
    }

    impl Rig {
        fn new() -> Rig {
            let mut rig = Rig { router: Router::default(), queue: Queue::new(), engine: Engine::default(), running: true, room: usize::MAX, blocked_drains: 0 };
            rig.router.ui_epoch(1, &mut Vec::new());
            rig
        }

        fn input(&mut self, f: impl FnOnce(&mut Router, &mut Vec<Out>)) -> Result<Vec<Command>, Refused> {
            let mut out = Vec::new();
            f(&mut self.router, &mut out);
            match self.queue.admit(&out, self.running) {
                Ok(()) => Ok(out.iter().map(|o| o.command).collect()),
                Err(why) => {
                    self.router.attack_refused(&out);
                    Err(why)
                }
            }
        }

        fn action(&mut self, command: Command) -> Result<(), Refused> {
            self.queue.admit(&[Out::new(command)], self.running)
        }

        fn drain(&mut self) -> Drained {
            if !self.running {
                return Drained { left: self.queue.len(), ..Drained::default() };
            }
            let (engine, room) = (&mut self.engine, self.room);
            let mut taken = 0;
            let d = self.queue.drain(&mut |c| {
                assert!(c.frame.is_none(), "nothing is stamped");
                if taken == room {
                    return Err(());
                }
                taken += 1;
                engine.apply(c.command);
                Ok(())
            });
            self.blocked_drains += usize::from(d.blocked);
            d
        }

        fn drain_all(&mut self) {
            let mut spins = 0;
            while self.drain().left > 0 {
                spins += 1;
                assert!(spins < 1000, "the queue never emptied");
            }
        }

        fn held(&self) -> Vec<u8> {
            let held = self.router.held().read();
            (0..128).filter(|n| held.contains(*n)).collect()
        }
    }

    // A target switch then a note-on back to back sounds on the new target, also when both
    // wait in the queue behind a stalled ring.
    #[test]
    fn a_switch_then_a_note_on_back_to_back_sounds_on_the_new_target() {
        let (a, b) = (midi(1, 0), midi(2, 0));
        for stalled in [false, true] {
            let mut rig = Rig::new();
            rig.input(|r, o| r.select_target(Some(0), LEAD, o)).unwrap();
            rig.input(|r, o| r.note_on(&a, 60, 100, o)).unwrap();
            rig.drain_all();
            if stalled {
                rig.room = 0;
            }
            rig.input(|r, o| r.select_target(Some(0), PAD, o)).unwrap();
            rig.input(|r, o| r.note_on(&b, 64, 100, o)).unwrap();
            rig.drain();
            rig.room = usize::MAX;
            rig.drain_all();
            assert_eq!(rig.engine.notes(), [(Some(PAD), 64)], "stalled: {stalled}");
            let tail = &rig.engine.log[rig.engine.log.len() - 3..];
            assert_eq!(tail, [Command::NoteOff(60), Command::SelectInstrument(PAD), Command::NoteOn(64, 100.0 / 127.0)]);
        }
    }

    // Mixed MIDI and UI owners on one note: one strike, one release when the last lets go.
    #[test]
    fn a_midi_and_a_ui_owner_share_one_note() {
        let mut rig = Rig::new();
        let (port, key) = (midi(1, 0), ui(1, "key:KeyA"));
        rig.input(|r, o| r.select_target(Some(0), LEAD, o)).unwrap();
        assert_eq!(rig.input(|r, o| r.note_on(&port, 60, 100, o)).unwrap(), [Command::NoteOn(60, 100.0 / 127.0)]);
        assert_eq!(rig.input(|r, o| r.note_on(&key, 60, 90, o)).unwrap(), []);
        assert_eq!(rig.input(|r, o| r.note_off(&port, 60, o)).unwrap(), []);
        rig.drain_all();
        assert_eq!(rig.engine.notes(), [(Some(LEAD), 60)], "the key still holds it");
        assert_eq!(rig.held(), [60]);
        assert_eq!(rig.input(|r, o| r.note_off(&key, 60, o)).unwrap(), [Command::NoteOff(60)]);
        rig.drain_all();
        assert_eq!(rig.engine.notes(), []);
        assert_eq!(rig.held(), [] as [u8; 0]);
    }

    // A dense multi-port CC stream with a release burst against the 64-entry table (a release
    // the engine did not accept is retried, wheel updates coalesce, never across a note or an action).
    #[test]
    fn a_dense_cc_stream_and_a_release_burst_reach_the_engine_in_order_64_a_drain() {
        let mut rig = Rig::new();
        rig.room = 64;
        let owners: Vec<Owner> = (0..4).flat_map(|conn| (0..2).map(move |channel| midi(conn, channel))).collect();
        rig.input(|r, o| r.select_target(Some(0), LEAD, o)).unwrap();
        for (k, owner) in owners.iter().enumerate() {
            for n in 0..10 {
                rig.input(|r, o| r.note_on(owner, 20 + (k * 10 + n) as u8, 100, o)).unwrap();
            }
        }
        rig.drain_all();
        assert_eq!(rig.engine.notes().len(), 80);

        // Every note and action the queue took, with the wheels the router had sent before it.
        let mut expected: Vec<(Command, f64, f64)> = Vec::new();
        let (mut bend, mut depth) = (0.0, 0.0);
        let mut wheels_sent = 0;
        let note = |sent: Vec<Command>, expected: &mut Vec<(Command, f64, f64)>, bend: &mut f64, depth: &mut f64, wheels: &mut usize| {
            for c in sent {
                match c {
                    Command::PitchBend(v) => (*bend, *wheels) = (v, *wheels + 1),
                    Command::Modulation(v) => (*depth, *wheels) = (v, *wheels + 1),
                    c => expected.push((c, *bend, *depth)),
                }
            }
        };
        for round in 0..400u32 {
            for (k, owner) in owners.iter().enumerate() {
                let v = f64::from((round * 7 + k as u32) % 128) / 127.0;
                let sent = rig.input(|r, o| r.pitch_bend(owner, v * 4.0 - 2.0, o)).unwrap();
                note(sent, &mut expected, &mut bend, &mut depth, &mut wheels_sent);
                let sent = rig.input(|r, o| r.modulation(owner, v, o)).unwrap();
                note(sent, &mut expected, &mut bend, &mut depth, &mut wheels_sent);
            }
            if round % 40 == 13 {
                rig.action(Command::Action(Action::PlayAll)).unwrap();
                expected.push((Command::Action(Action::PlayAll), bend, depth));
            }
            if round == 200 {
                // The burst: every port lets go of all it holds, its wheel moving between its releases.
                for (k, owner) in owners.iter().enumerate() {
                    for n in 0..10 {
                        let sent = rig.input(|r, o| r.note_off(owner, 20 + (k * 10 + n) as u8, o)).unwrap();
                        note(sent, &mut expected, &mut bend, &mut depth, &mut wheels_sent);
                        let sent = rig.input(|r, o| r.pitch_bend(owner, n as f64 / 10.0, o)).unwrap();
                        note(sent, &mut expected, &mut bend, &mut depth, &mut wheels_sent);
                    }
                }
            }
            rig.drain();
        }
        rig.drain_all();

        assert_eq!(rig.engine.notes(), [], "every release reached the engine");
        assert!(rig.blocked_drains > 0, "the burst outran the table: some releases waited for the next drain");
        // Never across a note or an action: before each, the engine has the wheels the router had sent.
        let mut seen = Vec::new();
        let (mut engine_bend, mut engine_depth) = (0.0, 0.0);
        let log = std::mem::take(&mut rig.engine.log);
        let mut engine_wheels = 0;
        for c in log.into_iter().skip(81) {
            match c {
                Command::PitchBend(v) => (engine_bend, engine_wheels) = (v, engine_wheels + 1),
                Command::Modulation(v) => (engine_depth, engine_wheels) = (v, engine_wheels + 1),
                c => seen.push((c, engine_bend, engine_depth)),
            }
        }
        assert_eq!(seen, expected);
        assert_eq!((rig.engine.pitch_bend, rig.engine.modulation), (bend, depth), "the wheels end where they were left");
        assert!(engine_wheels < wheels_sent, "wheel updates coalesced: {engine_wheels} of {wheels_sent}");
        assert_eq!(rig.queue.counters().wheels_deferred, 0);
    }

    // A FIFO filled with reserved entries, then a pedal-up, a disconnect's releases and a target
    // switch: all three go in, and once the ring takes them nothing is left sounding.
    #[test]
    fn with_the_fifo_full_a_pedal_up_a_disconnect_and_a_switch_still_go_in() {
        let mut rig = Rig::new();
        let (a, b) = (midi(1, 0), midi(2, 0));
        rig.input(|r, o| r.select_target(Some(0), LEAD, o)).unwrap();
        rig.drain_all();
        rig.room = 0;
        rig.input(|r, o| r.sustain(&a, true, o)).unwrap();
        rig.input(|r, o| r.pitch_bend(&a, 1.0, o)).unwrap();
        rig.input(|r, o| r.pitch_bend(&b, -1.0, o)).unwrap();
        for n in 0..40 {
            rig.input(|r, o| r.note_on(&a, n, 100, o)).unwrap();
            rig.input(|r, o| r.note_off(&a, n, o)).unwrap();
        }
        let mut n = 40;
        while rig.input(|r, o| r.note_on(&b, n, 100, o)).is_ok() {
            n += 1;
        }
        assert_eq!(n, 126, "the room went to the attacks and their reserved releases");
        assert!(!rig.held().contains(&126), "the refused attack records no owner");
        while rig.action(Command::Action(Action::Undo)).is_ok() {}
        assert_eq!(rig.action(Command::Action(Action::Undo)), Err(Refused::Full));
        assert_eq!(rig.queue.len() + rig.queue.reserved(), CAPACITY);

        assert_eq!(rig.input(|r, o| r.sustain(&a, false, o)).unwrap().len(), 40);
        let released = rig.input(|r, o| r.release_conn(2, o)).unwrap();
        assert_eq!(released.len(), 87, "b's 86 notes and the wheel handed back to a");
        assert_eq!(released.last(), Some(&Command::PitchBend(1.0)));
        assert_eq!(rig.input(|r, o| r.select_target(Some(0), PAD, o)).unwrap(), [Command::SelectInstrument(PAD)]);

        rig.room = 64;
        rig.drain_all();
        assert_eq!(rig.engine.notes(), []);
        assert_eq!(rig.engine.target, Some(PAD));
        assert_eq!(rig.engine.pitch_bend, 1.0, "the wheel that waited for room got in");
        // Every release reached the engine before the switch (which would end them anyway): replayed
        // up to it, nothing sounds.
        let switch = rig.engine.log.iter().position(|c| *c == Command::SelectInstrument(PAD)).unwrap();
        let mut before = Engine::default();
        rig.engine.log[..switch].iter().for_each(|c| before.apply(*c));
        assert_eq!(before.notes(), [], "nothing sounds before the switch");
        let offs = rig.engine.log[..switch].iter().filter(|c| matches!(c, Command::NoteOff(_))).count();
        assert_eq!(offs, 126);
    }

    // Review fix: with the room gone, a panic (every note released, then AllNotesOff) still goes in, so
    // the router may forget the notes it releases; a second panic moves the first to the tail.
    #[test]
    fn with_the_fifo_full_a_panic_still_goes_in() {
        let mut rig = Rig::new();
        let a = midi(1, 0);
        rig.input(|r, o| r.select_target(Some(0), LEAD, o)).unwrap();
        rig.drain_all();
        rig.room = 0;
        let mut n = 0;
        while rig.input(|r, o| r.note_on(&a, n, 100, o)).is_ok() {
            n += 1;
        }
        while rig.action(Command::Action(Action::Undo)).is_ok() {}
        let panic = |r: &mut Router, o: &mut Vec<Out>| {
            r.release_all(o);
            o.push(Out::new(Command::AllNotesOff));
        };
        assert_eq!(rig.input(panic).unwrap().len(), usize::from(n) + 1);
        assert_eq!(rig.held(), [] as [u8; 0]);
        assert_eq!(rig.input(panic).unwrap(), [Command::AllNotesOff]);
        rig.room = 64;
        rig.drain_all();
        assert_eq!(rig.engine.notes(), []);
        assert_eq!(rig.engine.log.last(), Some(&Command::AllNotesOff));
        assert_eq!(rig.engine.log.iter().filter(|c| **c == Command::AllNotesOff).count(), 1);
        assert_eq!(rig.engine.log.iter().filter(|c| matches!(c, Command::NoteOff(_))).count(), usize::from(n));
    }

    // A refused attack, switch or release followed by a re-strike.
    #[test]
    fn a_refused_attack_switch_or_release_then_a_restrike_sounds_once_and_ends() {
        let a = midi(1, 0);
        // An attack the queue refused: no owner, so the next press is a first strike.
        let mut rig = Rig::new();
        rig.input(|r, o| r.select_target(Some(0), LEAD, o)).unwrap();
        rig.running = false;
        assert_eq!(rig.input(|r, o| r.note_on(&a, 60, 100, o)), Err(Refused::NoDevice));
        assert_eq!(rig.held(), [] as [u8; 0]);
        assert_eq!(rig.input(|r, o| r.note_off(&a, 60, o)).unwrap(), [], "its release ends nothing");
        rig.running = true;
        assert_eq!(rig.input(|r, o| r.note_on(&a, 60, 100, o)).unwrap(), [Command::NoteOn(60, 100.0 / 127.0)]);
        rig.drain_all();
        assert_eq!(rig.engine.notes(), [(Some(LEAD), 60)]);

        // A re-strike under the pedal the queue refused: the pedal's voice goes on, the owner is not
        // recorded, and a later re-strike ends it first.
        rig.input(|r, o| r.sustain(&a, true, o)).unwrap();
        rig.input(|r, o| r.note_off(&a, 60, o)).unwrap();
        rig.queue.pause();
        assert_eq!(rig.input(|r, o| r.note_on(&a, 60, 90, o)), Err(Refused::Paused));
        assert_eq!(rig.held(), [] as [u8; 0]);
        rig.queue.resume();
        assert_eq!(rig.input(|r, o| r.note_on(&a, 60, 90, o)).unwrap(), [Command::NoteOff(60), Command::NoteOn(60, 90.0 / 127.0)]);
        rig.input(|r, o| r.note_off(&a, 60, o)).unwrap();
        assert_eq!(rig.input(|r, o| r.sustain(&a, false, o)).unwrap(), [Command::NoteOff(60)]);
        rig.drain_all();
        assert_eq!(rig.engine.notes(), []);

        // A switch and a release the ring refused stay at the head: the re-strike behind them sounds on
        // the new target, after its release.
        rig.input(|r, o| r.note_on(&a, 62, 100, o)).unwrap();
        rig.drain_all();
        rig.room = 0;
        rig.input(|r, o| r.note_off(&a, 62, o)).unwrap();
        rig.input(|r, o| r.select_target(Some(0), PAD, o)).unwrap();
        rig.input(|r, o| r.note_on(&a, 62, 100, o)).unwrap();
        assert!(rig.drain().blocked);
        rig.room = usize::MAX;
        rig.drain_all();
        let tail = &rig.engine.log[rig.engine.log.len() - 3..];
        assert_eq!(tail, [Command::NoteOff(62), Command::SelectInstrument(PAD), Command::NoteOn(62, 100.0 / 127.0)]);
        assert_eq!(rig.engine.notes(), [(Some(PAD), 62)]);
    }

    // An engine rebuild with notes and HOLD held, a refused attack, a HOLD and a wheel update
    // queued (a rebuild needs no WebView). The WebView plays no part: nothing here waits on it.
    #[test]
    fn a_rebuild_with_notes_and_hold_held_and_commands_queued() {
        let mut rig = Rig::new();
        let (a, b) = (midi(1, 0), midi(2, 0));
        rig.input(|r, o| r.select_target(Some(0), LEAD, o)).unwrap();
        rig.input(|r, o| r.note_on(&a, 60, 100, o)).unwrap();
        rig.input(|r, o| r.note_on(&a, 62, 100, o)).unwrap();
        rig.action(Command::Action(Action::Hold(0))).unwrap();
        rig.drain_all();
        assert_eq!((rig.engine.notes().len(), rig.engine.holds.clone()), (2, vec![0]));

        // The ring stalls; an attack, a HOLD press and a wheel update queue.
        rig.room = 0;
        rig.input(|r, o| r.note_on(&b, 64, 100, o)).unwrap();
        rig.action(Command::Action(Action::Hold(1))).unwrap();
        rig.input(|r, o| r.pitch_bend(&a, 0.5, o)).unwrap();
        // The rebuild begins: an attack now is refused and recorded nowhere.
        rig.queue.pause();
        assert_eq!(rig.input(|r, o| r.note_on(&b, 65, 100, o)), Err(Refused::Paused));
        let (fold, units) = rig.queue.rebuild();
        assert_eq!(fold, Fold { target: None, pitch_bend: Some(0.5), modulation: None });
        assert!(units.is_empty());
        assert_eq!(rig.queue.counters().discarded, 2, "the queued attack and HOLD press");
        // The settings replay hands the new engine the remembered target and the folded wheel, once.
        rig.engine = Engine::default();
        for c in [Command::SelectInstrument(LEAD)].into_iter().chain(fold.commands()) {
            rig.engine.apply(c);
        }
        rig.router.engine_rebuilt();
        rig.queue.resume();
        rig.room = usize::MAX;

        assert_eq!(rig.input(|r, o| r.pitch_bend(&a, 0.5, o)).unwrap(), [], "the wheel is not sent again");
        assert_eq!(rig.input(|r, o| r.note_off(&a, 60, o)).unwrap(), [], "a key held across: its release is harmless");
        assert_eq!(rig.input(|r, o| r.note_on(&b, 62, 100, o)).unwrap(), [Command::NoteOn(62, 100.0 / 127.0)], "a fresh press attacks again");
        assert_eq!(rig.input(|r, o| r.note_off(&a, 62, o)).unwrap(), [], "b still holds it");
        assert_eq!(rig.input(|r, o| r.note_off(&b, 64, o)).unwrap(), [], "its attack was discarded: nothing to end");
        assert_eq!(rig.input(|r, o| r.note_on(&b, 64, 100, o)).unwrap(), [Command::NoteOn(64, 100.0 / 127.0)]);
        rig.drain_all();
        assert_eq!(rig.engine.log, [Command::SelectInstrument(LEAD), Command::PitchBend(0.5), Command::NoteOn(62, 100.0 / 127.0), Command::NoteOn(64, 100.0 / 127.0)]);
        assert_eq!(rig.engine.holds, [] as [u8; 0], "no HOLD crossed into the new engine");
        // The HOLD held across has no reservation left: the whole room but for the new notes' releases,
        // the next target change and a panic (DEV: and each slot's next pick) is free.
        assert_eq!(rig.queue.reserved(), 4 + if cfg!(debug_assertions) { lf_engine::SLOT_COUNT } else { 0 });
    }

    // A retained-engine device gap with a HOLD press and its release (the no-device rule): the release
    // of a HOLD pressed before the gap passes and reaches the engine when it runs again; a HOLD pressed
    // in the gap is dropped, as a note-on is; controller state is kept.
    #[test]
    fn in_a_device_gap_a_hold_release_passes_and_a_fresh_press_is_dropped() {
        let mut rig = Rig::new();
        let a = midi(1, 0);
        rig.input(|r, o| r.select_target(Some(0), LEAD, o)).unwrap();
        rig.input(|r, o| r.note_on(&a, 60, 100, o)).unwrap();
        rig.action(Command::Action(Action::Hold(0))).unwrap();
        rig.drain_all();

        rig.running = false;
        assert_eq!(rig.action(Command::Action(Action::Hold(1))), Err(Refused::NoDevice));
        assert_eq!(rig.action(Command::Action(Action::Release(0))), Ok(()));
        assert_eq!(rig.input(|r, o| r.note_on(&a, 64, 100, o)), Err(Refused::NoDevice));
        assert_eq!(rig.input(|r, o| r.note_off(&a, 60, o)).unwrap(), [Command::NoteOff(60)]);
        for v in 0..100 {
            rig.input(|r, o| r.modulation(&a, f64::from(v) / 100.0, o)).unwrap();
        }
        assert_eq!(rig.drain(), Drained { sent: 0, left: 3, blocked: false }, "nothing drains into a stopped engine");
        assert_eq!(rig.engine.holds, [0], "the stopped engine keeps its capture");

        rig.running = true;
        rig.drain_all();
        assert_eq!(rig.engine.holds, [] as [u8; 0], "the release reached it");
        assert_eq!(rig.engine.notes(), []);
        assert_eq!(rig.engine.modulation, 0.99);
        assert_eq!(rig.held(), [] as [u8; 0]);
    }

    // Decision 7: a document's holds go when a new document replaces it or the window loses focus, and
    // a replaced document's late events are refused; MIDI holds stay.
    #[test]
    fn a_new_document_or_a_blur_releases_the_ui_holds_and_nothing_else() {
        let mut rig = Rig::new();
        let port = midi(1, 0);
        rig.input(|r, o| r.select_target(Some(0), LEAD, o)).unwrap();
        rig.input(|r, o| r.note_on(&ui(1, "pointer:1"), 60, 100, o)).unwrap();
        rig.input(|r, o| r.note_on(&ui(1, "key:KeyA"), 62, 100, o)).unwrap();
        rig.input(|r, o| r.note_on(&port, 62, 100, o)).unwrap();
        assert_eq!(rig.input(|r, o| r.ui_epoch(2, o)).unwrap(), [Command::NoteOff(60)]);
        assert_eq!(rig.input(|r, o| r.note_on(&ui(1, "key:KeyB"), 64, 100, o)).unwrap(), [], "a late event of the old document");
        assert_eq!(rig.input(|r, o| r.note_off(&port, 62, o)).unwrap(), [Command::NoteOff(62)]);
        rig.input(|r, o| r.note_on(&ui(2, "key:KeyA"), 65, 100, o)).unwrap();
        rig.input(|r, o| r.note_on(&port, 67, 100, o)).unwrap();
        assert_eq!(rig.input(|r, o| r.ui_blur(1, o)).unwrap(), [], "an old document's blur");
        assert_eq!(rig.input(|r, o| r.ui_blur(2, o)).unwrap(), [Command::NoteOff(65)]);
        rig.drain_all();
        assert_eq!(rig.engine.notes(), [(Some(LEAD), 67)]);
        assert_eq!(rig.held(), [67]);
    }

    /// The real engine with no device: the ring the queue drains into, rendered block by block.
    struct Real {
        engine: RealEngine,
        ring: rtrb::Producer<TimedCommand>,
        /// The rest of the handle, kept as the device side keeps it.
        _handle: (
            rtrb::Consumer<lf_engine::Event>,
            [lf_engine::SlotPort; lf_engine::SLOT_COUNT],
            std::sync::Arc<lf_engine::Overview>,
            lf_engine::SessionPort,
            rtrb::Consumer<lf_engine::scope::ScopeBin>,
        ),
        frame: Frame,
    }

    impl Real {
        const BLOCK: usize = 128;

        fn new() -> Real {
            let (engine, handle) = RealEngine::new(EngineConfig { max_loop_seconds: 2.0, ..EngineConfig::new(48_000) });
            let EngineHandle { commands, events, slots, overview, session, scope, .. } = handle;
            Real { engine, ring: commands, _handle: (events, slots, overview, session, scope), frame: 0 }
        }

        fn drain(&mut self, queue: &mut Queue) -> Drained {
            let ring = &mut self.ring;
            queue.drain(&mut |c| ring.push(c).map_err(|_| ()))
        }

        /// Commands in the ring, not yet taken into the engine's 64-entry table.
        fn in_ring(&self) -> usize {
            1024 - self.ring.slots()
        }

        /// Render `seconds`; the left channel of the last block.
        fn render(&mut self, seconds: f64) -> Vec<f32> {
            let (input, mut left, mut right) = (vec![0.0; Self::BLOCK], vec![0.0; Self::BLOCK], vec![0.0; Self::BLOCK]);
            for _ in 0..(seconds * 48_000.0 / Self::BLOCK as f64).ceil() as usize {
                let ctx = ProcessContext { frame: self.frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
                self.engine.process(&ctx, &input, &mut left, &mut right);
                self.frame += Self::BLOCK as Frame;
            }
            left
        }
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    // On the real engine: bursts of 120 attacks and 120 releases, each more than the
    // engine's 64-entry table takes a block, lose no release; and the pitch wheel set on Lead reaches Pad
    // through the switch alone (`Instruments::select` hands it over; the router sends none).
    #[test]
    fn into_the_real_engine_no_release_is_lost_past_its_table_and_the_wheel_reaches_a_new_instrument() {
        let run = |bend: f64| -> Vec<f32> {
            let (mut real, mut router, mut queue) = (Real::new(), Router::default(), Queue::new());
            let owners: Vec<Owner> = (0..4).map(|conn| midi(conn, 0)).collect();
            let input = |router: &mut Router, queue: &mut Queue, f: &dyn Fn(&mut Router, &mut Vec<Out>)| {
                let mut out = Vec::new();
                f(router, &mut out);
                queue.admit(&out, true).unwrap();
                out.into_iter().map(|o| o.command).collect::<Vec<_>>()
            };
            input(&mut router, &mut queue, &|r, o| r.select_target(Some(0), LEAD, o));
            input(&mut router, &mut queue, &|r, o| r.pitch_bend(&owners[0], bend, o));
            for (k, owner) in owners.iter().enumerate() {
                for n in 0..30 {
                    input(&mut router, &mut queue, &|r, o| r.note_on(owner, 4 + (k * 30 + n) as u8, 100, o));
                }
            }
            real.drain(&mut queue);
            assert!(real.in_ring() > 64, "{} commands wait for the table", real.in_ring());
            assert!(peak(&real.render(0.2)) > 0.01, "the notes sound");
            for (k, owner) in owners.iter().enumerate() {
                for n in 0..30 {
                    input(&mut router, &mut queue, &|r, o| r.note_off(owner, 4 + (k * 30 + n) as u8, o));
                }
            }
            real.drain(&mut queue);
            assert_eq!(real.in_ring(), 120);
            let tail = peak(&real.render(2.0));
            assert!(tail < 1e-4, "every release reached the voices: {tail}");
            assert_eq!(real.engine.diag().commands_dropped, 0);

            let switch = input(&mut router, &mut queue, &|r, o| r.select_target(Some(1), PAD, o));
            assert_eq!(switch, [Command::SelectInstrument(PAD)], "no wheel goes with the switch");
            input(&mut router, &mut queue, &|r, o| r.note_on(&owners[0], 60, 100, o));
            real.drain(&mut queue);
            real.render(0.4)
        };
        let (bent, flat) = (run(2.0), run(0.0));
        assert!(peak(&bent) > 0.001 && peak(&flat) > 0.001, "Pad sounds");
        assert_ne!(bent, flat, "the wheel moved on Lead bends Pad's note");
    }
}
