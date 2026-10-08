//! OWNS: the one ordered path from every input to the engine's command ring (plan decision 5): a
//! bounded FIFO that the router's batches (notes, wheels, the note target), the MIDI-learn actions and
//! the UI's input commands (`engine_send`'s looper presses, `Press`, the toggles) all join, in the
//! order they happened, so a pedal and a click keep their order. Its owner drains it into
//! `EngineHost::send`, which keeps the settings memory; the router and the queue sit under one lock.
//!
//! # Rules
//!
//! - **One FIFO, head of line.** A command the ring refuses stays at the head, and nothing behind it
//!   overtakes it: the next drain (after the next input, or the owner's short timer) retries it.
//!   Nothing is stamped (`TimedCommand::frame` `None`, decision 4), so the ring's admission order is
//!   also the order the engine applies them in.
//! - **Drained only while a device runs.** A stopped engine's ring is drained by nothing, so what
//!   waits for the device waits here, where wheel updates still coalesce and every release has its
//!   slot; the engine (retained, or rebuilt) gets it all, in order, once a device runs.
//! - **Capacity is reserved, not hoped for.** An admitted attack (`NoteOn`) reserves the slot its
//!   `NoteOff` will take, a HOLD press the slot of its release, and one slot is kept for the note target
//!   while none is queued. So a release and a target switch always go in. When the unreserved room is
//!   gone, a batch that needs some is refused (counted): fresh attacks, HOLD presses, actions, and a
//!   release nothing reserved a slot for.
//! - **A batch goes in whole or not at all** (one input's commands: a re-strike's `NoteOff` and
//!   `NoteOn`, a switch's releases and the switch); a refused batch leaves nothing queued, and the
//!   router undoes the attack it recorded (`Router::attack_refused`).
//! - **A target change replaces one still queued, in place.** Everything queued behind the old change
//!   was played after it, so it now sounds on the newest target, and the new switch's releases (all for
//!   notes struck after the old change: that one released and forgot the rest) go in at the tail, after
//!   their attacks, and end those notes on the target they sound on. The releases queued ahead of the
//!   old change stay ahead of it and end the notes of the target before it. No release ever moves ahead
//!   of its attack, and one slot is all the target ever needs.
//! - **Wheel updates coalesce:** a new value replaces the queued value of the same wheel when only wheel
//!   updates follow it, never across a note, an action or a target change. One that finds no room is
//!   not queued at its place: its value waits outside the FIFO (the latest wins, counted) and goes in
//!   at the tail as soon as there is room, so the wheel still ends where the player left it.
//! - **Fresh one-shots need a running device and the engine generation the queue feeds** (decisions 6
//!   and 7): while no device runs, while paused for a rebuild, or for another generation, attacks,
//!   actions and HOLD presses are refused (the router records no owner); releases, the target and the
//!   wheels still go in (every release passes, controller state is kept).
//! - **A rebuild** (`pause`, `rebuild`, then `resume` once the settings replay has run): the old
//!   engine's attacks, actions, HOLD presses and their releases are discarded (the new engine has no
//!   voice or capture for them to end), and the latest target and wheels, queued or waiting, come back
//!   as a [`Fold`] for the settings memory, so they reach the new engine through its replay alone.

use std::collections::VecDeque;

use lf_engine::{Action, Command, NoteTarget, TimedCommand};

/// The FIFO's bound. It fills only while the engine takes nothing (no device runs, or its ring of 1024
/// commands, `EngineConfig::new`, refuses), so it is sized for what it must honour then: a reserved
/// release for every note (128) and for each HOLD control the engine tells apart (16,
/// `lf_engine::HOLD_CONTROLS`), and the target's slot (1), leaving 111 entries of backlog for fresh
/// attacks, actions and wheel updates; past that, input that cannot reach the engine is stale and
/// refused. Preallocated.
pub const CAPACITY: usize = 256;

/// What a command is to the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// `NoteOn`: reserves the slot its release takes.
    Attack,
    /// `NoteOff` (takes its attack's slot), `AllNotesOff`.
    Release,
    /// `PitchBend`, `Modulation`: coalesces.
    Wheel,
    /// `SelectInstrument`: replaces one still queued.
    Target,
    /// Every other command: a looper press, `Press`, a toggle.
    Action,
    /// HOLD's press (`Action::Hold`): reserves the slot of its release.
    HoldPress,
    /// HOLD's release (`Action::Release`).
    HoldRelease,
}

impl Class {
    pub fn of(command: &Command) -> Class {
        match command {
            Command::NoteOn(..) => Class::Attack,
            Command::NoteOff(_) | Command::AllNotesOff => Class::Release,
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
}

/// Why a batch was refused (nothing of it was queued).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// No device runs: a fresh note-on, action or HOLD press would fire at the next open.
    NoDevice,
    /// A rebuild is under way.
    Paused,
    /// Made for another engine generation than the one the queue feeds.
    Stale,
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
    /// The old engine's one-shots a rebuild discarded.
    pub discarded: u64,
}

struct Entry {
    command: Command,
    class: Class,
    generation: u64,
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

/// The free room `out` takes, with the reservations it makes or uses.
fn cost(notes: &mut Reserved, holds: &mut Reserved, out: &Out) -> usize {
    match (out.class, out.command) {
        (Class::Attack, Command::NoteOn(n, _)) => notes.reserve(n),
        (Class::Release, Command::NoteOff(n)) => notes.release(n),
        (Class::HoldPress, c) => hold_control(&c).map_or(1, |c| holds.reserve(c)),
        (Class::HoldRelease, c) => hold_control(&c).map_or(1, |c| holds.release(c)),
        // A target takes the slot kept for it, or replaces the one queued; a wheel coalesces or waits.
        (Class::Target | Class::Wheel, _) => 0,
        _ => 1,
    }
}

pub struct Queue {
    entries: VecDeque<Entry>,
    /// The engine generation it feeds.
    generation: u64,
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
        Queue::new(0)
    }
}

impl Queue {
    /// An empty queue feeding engine generation `generation`.
    pub fn new(generation: u64) -> Queue {
        Queue {
            entries: VecDeque::with_capacity(CAPACITY),
            generation,
            paused: false,
            notes: Reserved::default(),
            holds: Reserved::default(),
            waiting: [None; 2],
            counters: QueueCounters::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn counters(&self) -> QueueCounters {
        self.counters
    }

    fn target_queued(&self) -> bool {
        self.entries.iter().any(|e| e.class == Class::Target)
    }

    /// Slots promised to releases not yet admitted, and to the target while none is queued.
    fn reserved(&self) -> usize {
        self.notes.count() + self.holds.count() + usize::from(!self.target_queued())
    }

    /// Room no reservation holds.
    fn free(&self) -> usize {
        CAPACITY.saturating_sub(self.entries.len() + self.reserved())
    }

    /// One input's commands, made for engine generation `generation`: queued whole, or refused whole.
    pub fn admit(&mut self, batch: &[Out], generation: u64, device_running: bool) -> Result<(), Refused> {
        if batch.is_empty() {
            return Ok(());
        }
        if batch.iter().any(|o| o.class.fresh()) {
            let refused = if self.paused {
                Some(Refused::Paused)
            } else if generation != self.generation {
                Some(Refused::Stale)
            } else if !device_running {
                Some(Refused::NoDevice)
            } else {
                None
            };
            if let Some(why) = refused {
                self.count_refused(batch);
                return Err(why);
            }
        }
        // A value that waited for room is older than this batch: it goes in first.
        self.place_waiting();
        let (mut notes, mut holds) = (self.notes, self.holds);
        let mut owed: usize = batch.iter().map(|o| cost(&mut notes, &mut holds, o)).sum();
        if owed > self.free() {
            self.count_refused(batch);
            return Err(Refused::Full);
        }
        for out in batch {
            if out.class == Class::Wheel {
                self.push_wheel(out.command, generation, owed);
                continue;
            }
            owed -= cost(&mut self.notes, &mut self.holds, out);
            self.push(out, generation);
        }
        debug_assert!(self.entries.len() + self.reserved() <= CAPACITY, "a reservation was overbooked");
        Ok(())
    }

    /// One of the UI's input commands (`engine_send`'s looper presses, `Press`, the toggles), in the
    /// same FIFO as everything else. Notes, wheels and the note target go through the router. A setting
    /// (an absolute setter) goes straight to `EngineHost::send`: a rebuild would discard it here.
    pub fn admit_ui(&mut self, command: Command, generation: u64, device_running: bool) -> Result<(), Refused> {
        self.admit(&[Out::new(command)], generation, device_running)
    }

    fn push(&mut self, out: &Out, generation: u64) {
        if out.class == Class::Target {
            if let Some(queued) = self.entries.iter_mut().find(|e| e.class == Class::Target) {
                queued.command = out.command;
                queued.generation = generation;
                self.counters.targets_replaced += 1;
                return;
            }
        }
        self.entries.push_back(Entry { command: out.command, class: out.class, generation });
    }

    /// A wheel update: merged into the same wheel's value still waiting or queued behind nothing but
    /// wheel updates, else queued if the room `owed` to the rest of its batch leaves a slot, else left
    /// waiting.
    fn push_wheel(&mut self, command: Command, generation: u64, owed: usize) {
        let Some((wheel, value)) = wheel(&command) else { return };
        if self.waiting[wheel].is_some() {
            self.waiting[wheel] = Some(value);
            self.counters.coalesced += 1;
        } else if let Some(k) = self.coalescible(wheel, generation) {
            self.entries[k].command = command;
            self.counters.coalesced += 1;
        } else if self.free() > owed {
            self.entries.push_back(Entry { command, class: Class::Wheel, generation });
        } else {
            self.waiting[wheel] = Some(value);
            self.counters.wheels_deferred += 1;
        }
    }

    /// The queued update of `wheel` that only wheel updates follow.
    fn coalescible(&self, wheel_index: usize, generation: u64) -> Option<usize> {
        for (k, e) in self.entries.iter().enumerate().rev() {
            if e.class != Class::Wheel {
                return None;
            }
            if e.generation == generation && wheel(&e.command).is_some_and(|(w, _)| w == wheel_index) {
                return Some(k);
            }
        }
        None
    }

    /// Queue the waiting wheel values there is room for. True when one went in.
    fn place_waiting(&mut self) -> bool {
        let mut placed = false;
        for wheel in 0..2 {
            let Some(value) = self.waiting[wheel] else { continue };
            let command = wheel_command(wheel, value);
            if let Some(k) = self.coalescible(wheel, self.generation) {
                self.entries[k].command = command;
            } else if self.free() > 0 {
                self.entries.push_back(Entry { command, class: Class::Wheel, generation: self.generation });
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

    /// Send what the ring takes, in order, stopping at the first refusal (the head stays). Then the
    /// waiting wheel values go in, if the sent ones made room. Nothing while paused.
    pub fn drain(&mut self, send: &mut dyn FnMut(TimedCommand) -> Result<(), ()>) -> Drained {
        let mut drained = Drained::default();
        while !self.paused {
            while let Some(e) = self.entries.front() {
                if send(TimedCommand { frame: None, command: e.command }).is_err() {
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

    /// The engine was replaced by generation `new_generation`: discard what was queued for the old one
    /// (its one-shots and their releases) and hand back the latest target and wheels, queued or
    /// waiting, for the settings memory. Releases already made for the new generation stay.
    pub fn rebuild(&mut self, new_generation: u64) -> Fold {
        let mut fold = Fold::default();
        let mut discarded = 0;
        self.entries.retain(|e| {
            match e.command {
                Command::SelectInstrument(target) => fold.target = Some(target),
                Command::PitchBend(v) => fold.pitch_bend = Some(v),
                Command::Modulation(v) => fold.modulation = Some(v),
                _ if e.generation == new_generation => return true,
                _ => discarded += 1,
            }
            false
        });
        // A waiting value is newer than any queued one of its wheel.
        fold.pitch_bend = self.waiting[0].take().or(fold.pitch_bend);
        fold.modulation = self.waiting[1].take().or(fold.modulation);
        self.notes = Reserved::default();
        self.holds = Reserved::default();
        self.generation = new_generation;
        self.counters.discarded += discarded;
        fold
    }

    /// The settings replay has run: admission and draining go on.
    pub fn resume(&mut self) {
        self.paused = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_engine::Instrument;

    const LEAD: NoteTarget = NoteTarget::Builtin(Instrument::Lead);
    const PAD: NoteTarget = NoteTarget::Builtin(Instrument::Pad);

    fn outs(commands: &[Command]) -> Vec<Out> {
        commands.iter().copied().map(Out::new).collect()
    }

    fn admit(q: &mut Queue, commands: &[Command]) -> Result<(), Refused> {
        q.admit(&outs(commands), 0, true)
    }

    /// Drain into a ring that takes `room` commands, returning what it took.
    fn take(q: &mut Queue, room: usize) -> Vec<Command> {
        let mut taken = Vec::new();
        q.drain(&mut |c| {
            assert_eq!(c.frame, None, "nothing is stamped");
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
        while q.admit(&outs(&[Command::Action(Action::Undo)]), q.generation, true).is_ok() {
            n += 1;
        }
        n
    }

    #[test]
    fn each_command_has_its_class() {
        let cases = [
            (Command::NoteOn(60, 0.5), Class::Attack),
            (Command::NoteOff(60), Class::Release),
            (Command::AllNotesOff, Class::Release),
            (Command::PitchBend(1.0), Class::Wheel),
            (Command::Modulation(0.5), Class::Wheel),
            (Command::SelectInstrument(LEAD), Class::Target),
            (Command::Action(Action::Hold(3)), Class::HoldPress),
            (Command::ActionOn(1, Action::Hold(3)), Class::HoldPress),
            (Command::Action(Action::Release(3)), Class::HoldRelease),
            (Command::ActionOn(1, Action::Release(3)), Class::HoldRelease),
            (Command::Action(Action::RecDub), Class::Action),
            (Command::Press, Class::Action),
            (Command::SelectTrack(2), Class::Action),
        ];
        for (command, class) in cases {
            assert_eq!(Class::of(&command), class, "{command:?}");
        }
    }

    // Plan decision 5: a command the ring refuses stays at the head and nothing behind it overtakes it.
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

    // Plan decision 5: a batch (a re-strike's NoteOff and NoteOn) is accepted whole or not at all.
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

    // Plan decision 5: "accepting an attack or a HOLD reserves the slot its release will need".
    #[test]
    fn an_attack_and_a_hold_press_reserve_their_release() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        admit(&mut q, &[Command::Action(Action::Hold(0))]).unwrap();
        assert_eq!(take(&mut q, 8).len(), 2, "both reached the engine; their reservations stay");
        let filled = fill(&mut q);
        assert_eq!(filled, CAPACITY - 3, "the two releases and the target keep their slots");
        // One slot free: an action fits; an attack or a HOLD press does not, needing its release's too.
        take(&mut q, 1);
        assert_eq!(admit(&mut q, &[Command::NoteOn(61, 0.5)]), Err(Refused::Full));
        assert_eq!(admit(&mut q, &[Command::Action(Action::Hold(1))]), Err(Refused::Full));
        assert_eq!(admit(&mut q, &[Command::Action(Action::Undo)]), Ok(()));
        assert_eq!(admit(&mut q, &[Command::Action(Action::Release(0))]), Ok(()));
        assert_eq!(admit(&mut q, &[Command::NoteOff(60)]), Ok(()));
        assert_eq!(admit(&mut q, &[Command::SelectInstrument(LEAD)]), Ok(()));
        assert_eq!(q.len(), CAPACITY);
        // A release nothing reserved finds no room.
        assert_eq!(admit(&mut q, &[Command::NoteOff(70)]), Err(Refused::Full));
        let c = q.counters();
        assert_eq!((c.attacks_refused, c.holds_refused, c.actions_refused, c.releases_refused), (1, 1, 1, 1));
    }

    // Plan decision 5: "a target change replaces a target change still queued"; the rule's reasoning is
    // in the module doc.
    #[test]
    fn a_target_change_replaces_one_still_queued_in_place() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::NoteOff(60), Command::SelectInstrument(LEAD)]).unwrap();
        admit(&mut q, &[Command::NoteOn(62, 0.5)]).unwrap();
        admit(&mut q, &[Command::NoteOff(62), Command::SelectInstrument(PAD)]).unwrap();
        assert_eq!(
            take(&mut q, 8),
            [Command::NoteOff(60), Command::SelectInstrument(PAD), Command::NoteOn(62, 0.5), Command::NoteOff(62)],
            "62 sounds and ends on the newest target; 60 ends before the switch"
        );
        assert_eq!(q.counters().targets_replaced, 1);
        // With the room gone, a switch still goes in: its slot was kept.
        fill(&mut q);
        assert_eq!(admit(&mut q, &[Command::SelectInstrument(LEAD)]), Ok(()));
        assert_eq!(admit(&mut q, &[Command::SelectInstrument(PAD)]), Ok(()), "and replaces");
    }

    // Plan decision 5: "wheel updates coalesce, never across a note or an action".
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

    // Plan decision 5: with the unreserved room gone a wheel update is refused its place; its value
    // waits and goes in at the tail once there is room, so the wheel ends where it was left.
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

    // Plan decision 7: with no device running, fresh note-ons, actions and HOLD presses are dropped,
    // controller state is kept, and every release passes.
    #[test]
    fn with_no_device_fresh_one_shots_are_refused_and_releases_and_controller_state_pass() {
        let mut q = Queue::default();
        admit(&mut q, &[Command::NoteOn(60, 0.5)]).unwrap();
        admit(&mut q, &[Command::Action(Action::Hold(2))]).unwrap();
        let gap = |q: &mut Queue, c: &[Command]| q.admit(&outs(c), 0, false);
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

    // Plan decision 6: pause stops admission of one-shots and draining; a rebuild discards the old
    // engine's one-shots and their releases and folds the latest target and wheels, queued or waiting;
    // releases made for the new engine stay.
    #[test]
    fn a_rebuild_discards_the_old_one_shots_and_folds_the_target_and_wheels() {
        let mut q = Queue::new(4);
        let at = |q: &mut Queue, g: u64, c: &[Command]| q.admit(&outs(c), g, true);
        at(&mut q, 4, &[Command::SelectInstrument(LEAD)]).unwrap();
        at(&mut q, 4, &[Command::NoteOn(60, 0.5)]).unwrap();
        at(&mut q, 4, &[Command::Action(Action::Hold(0))]).unwrap();
        at(&mut q, 4, &[Command::PitchBend(0.5)]).unwrap();
        at(&mut q, 4, &[Command::NoteOff(60), Command::SelectInstrument(PAD)]).unwrap();
        at(&mut q, 4, &[Command::Modulation(0.25)]).unwrap();
        q.pause();
        assert_eq!(at(&mut q, 4, &[Command::NoteOn(61, 0.5)]), Err(Refused::Paused));
        assert_eq!(at(&mut q, 4, &[Command::Action(Action::Release(0))]), Ok(()), "a release passes");
        assert_eq!(q.drain(&mut |_| panic!("nothing drains while paused")), Drained { sent: 0, left: 7, blocked: false });
        // Made for the engine being built: a release is taken, an attack is not.
        assert_eq!(at(&mut q, 5, &[Command::NoteOff(64)]), Ok(()));
        let fold = q.rebuild(5);
        assert_eq!(fold, Fold { target: Some(PAD), pitch_bend: Some(0.5), modulation: Some(0.25) });
        assert_eq!(fold.commands().collect::<Vec<_>>(), [Command::SelectInstrument(PAD), Command::PitchBend(0.5), Command::Modulation(0.25)]);
        assert_eq!(q.counters().discarded, 4, "NoteOn, Hold, NoteOff, Release");
        assert_eq!(at(&mut q, 4, &[Command::NoteOn(62, 0.5)]), Err(Refused::Paused));
        q.resume();
        assert_eq!(at(&mut q, 4, &[Command::NoteOn(62, 0.5)]), Err(Refused::Stale));
        assert_eq!(at(&mut q, 5, &[Command::NoteOn(62, 0.5)]), Ok(()));
        assert_eq!(take(&mut q, 8), [Command::NoteOff(64), Command::NoteOn(62, 0.5)]);
        // The old engine's reservations went with it: the room is whole again but for 62's release and
        // the target's slot.
        assert_eq!(fill(&mut q), CAPACITY - 2);
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
        assert_eq!(q.rebuild(1).pitch_bend, Some(0.9));
        assert!(q.is_empty());
    }
}

/// The ordering and overload cases of the plan's step 4, router and queue together, against a model
/// of what the engine does with what it is sent.
#[cfg(test)]
mod pipeline {
    use super::super::router::{Owner, Router};
    use super::*;
    use lf_engine::Instrument;

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
        generation: u64,
        room: usize,
        blocked_drains: usize,
    }

    impl Rig {
        fn new() -> Rig {
            let mut rig =
                Rig { router: Router::default(), queue: Queue::new(1), engine: Engine::default(), running: true, generation: 1, room: usize::MAX, blocked_drains: 0 };
            rig.router.ui_epoch(1, &mut Vec::new());
            rig
        }

        fn input(&mut self, f: impl FnOnce(&mut Router, &mut Vec<Out>)) -> Result<Vec<Command>, Refused> {
            let mut out = Vec::new();
            f(&mut self.router, &mut out);
            match self.queue.admit(&out, self.generation, self.running) {
                Ok(()) => Ok(out.iter().map(|o| o.command).collect()),
                Err(why) => {
                    self.router.attack_refused();
                    Err(why)
                }
            }
        }

        fn action(&mut self, command: Command) -> Result<(), Refused> {
            self.queue.admit_ui(command, self.generation, self.running)
        }

        fn drain(&mut self) -> Drained {
            if !self.running {
                return Drained { left: self.queue.len(), ..Drained::default() };
            }
            let (engine, room) = (&mut self.engine, self.room);
            let mut taken = 0;
            let d = self.queue.drain(&mut |c| {
                assert_eq!(c.frame, None, "nothing is stamped (decision 4)");
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

    // Step 4: "a target switch then a note-on back to back sounds on the new target", also when both
    // wait in the queue behind a stalled ring.
    #[test]
    fn a_switch_then_a_note_on_back_to_back_sounds_on_the_new_target() {
        let (a, b) = (midi(1, 0), midi(2, 0));
        for stalled in [false, true] {
            let mut rig = Rig::new();
            rig.input(|r, o| r.select_target(LEAD, o)).unwrap();
            rig.input(|r, o| r.note_on(&a, 60, 100, o)).unwrap();
            rig.drain_all();
            if stalled {
                rig.room = 0;
            }
            rig.input(|r, o| r.select_target(PAD, o)).unwrap();
            rig.input(|r, o| r.note_on(&b, 64, 100, o)).unwrap();
            rig.drain();
            rig.room = usize::MAX;
            rig.drain_all();
            assert_eq!(rig.engine.notes(), [(Some(PAD), 64)], "stalled: {stalled}");
            let tail = &rig.engine.log[rig.engine.log.len() - 3..];
            assert_eq!(tail, [Command::NoteOff(60), Command::SelectInstrument(PAD), Command::NoteOn(64, 100.0 / 127.0)]);
        }
    }

    // Step 4: "mixed MIDI and UI owners on one note": one strike, one release when the last lets go.
    #[test]
    fn a_midi_and_a_ui_owner_share_one_note() {
        let mut rig = Rig::new();
        let (port, key) = (midi(1, 0), ui(1, "key:KeyA"));
        rig.input(|r, o| r.select_target(LEAD, o)).unwrap();
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

    // Step 4: "a dense multi-port CC stream with a release burst against the 64-entry table (a release
    // the engine did not accept is retried, wheel updates coalesce, never across a note or an action)".
    #[test]
    fn a_dense_cc_stream_and_a_release_burst_reach_the_engine_in_order_64_a_drain() {
        let mut rig = Rig::new();
        rig.room = 64;
        let owners: Vec<Owner> = (0..4).flat_map(|conn| (0..2).map(move |channel| midi(conn, channel))).collect();
        rig.input(|r, o| r.select_target(LEAD, o)).unwrap();
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

    // Step 4: "a FIFO filled with reserved entries, then a pedal-up, a disconnect's releases and a target
    // switch": all three go in, and once the ring takes them nothing is left sounding.
    #[test]
    fn with_the_fifo_full_a_pedal_up_a_disconnect_and_a_switch_still_go_in() {
        let mut rig = Rig::new();
        let (a, b) = (midi(1, 0), midi(2, 0));
        rig.input(|r, o| r.select_target(LEAD, o)).unwrap();
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
        assert_eq!(n, 127, "the room went to the attacks and their reserved releases");
        assert!(!rig.held().contains(&127), "the refused attack records no owner");
        assert_eq!(rig.action(Command::Action(Action::Undo)), Err(Refused::Full));
        assert_eq!(rig.queue.len() + rig.queue.reserved(), CAPACITY);

        assert!(rig.input(|r, o| r.sustain(&a, false, o)).unwrap().len() == 40);
        let released = rig.input(|r, o| r.release_conn(2, o)).unwrap();
        assert_eq!(released.len(), 88, "b's 87 notes and the wheel handed back to a");
        assert_eq!(released.last(), Some(&Command::PitchBend(1.0)));
        assert_eq!(rig.input(|r, o| r.select_target(PAD, o)).unwrap(), [Command::SelectInstrument(PAD)]);

        rig.room = 64;
        rig.drain_all();
        assert_eq!(rig.engine.notes(), []);
        assert_eq!(rig.engine.target, Some(PAD));
        assert_eq!(rig.engine.pitch_bend, 1.0, "the wheel that waited for room got in");
    }

    // Step 4: "a refused attack, switch or release followed by a re-strike".
    #[test]
    fn a_refused_attack_switch_or_release_then_a_restrike_sounds_once_and_ends() {
        let a = midi(1, 0);
        // An attack the queue refused: no owner, so the next press is a first strike.
        let mut rig = Rig::new();
        rig.input(|r, o| r.select_target(LEAD, o)).unwrap();
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
        rig.input(|r, o| r.select_target(PAD, o)).unwrap();
        rig.input(|r, o| r.note_on(&a, 62, 100, o)).unwrap();
        assert!(rig.drain().blocked);
        rig.room = usize::MAX;
        rig.drain_all();
        let tail = &rig.engine.log[rig.engine.log.len() - 3..];
        assert_eq!(tail, [Command::NoteOff(62), Command::SelectInstrument(PAD), Command::NoteOn(62, 100.0 / 127.0)]);
        assert_eq!(rig.engine.notes(), [(Some(PAD), 62)]);
    }

    // Step 4: "an engine rebuild with notes and HOLD held, a refused attack, a HOLD and a wheel update
    // queued" (decision 6). The WebView plays no part: nothing here waits on it.
    #[test]
    fn a_rebuild_with_notes_and_hold_held_and_commands_queued() {
        let mut rig = Rig::new();
        let (a, b) = (midi(1, 0), midi(2, 0));
        rig.input(|r, o| r.select_target(LEAD, o)).unwrap();
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
        let fold = rig.queue.rebuild(2);
        rig.generation = 2;
        assert_eq!(fold, Fold { target: None, pitch_bend: Some(0.5), modulation: None });
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
        assert_eq!(rig.input(|r, o| r.note_on(&b, 64, 100, o)).unwrap(), [Command::NoteOn(64, 100.0 / 127.0)], "its discarded attack holds nothing");
        rig.drain_all();
        assert_eq!(rig.engine.log, [Command::SelectInstrument(LEAD), Command::PitchBend(0.5), Command::NoteOn(62, 100.0 / 127.0), Command::NoteOn(64, 100.0 / 127.0)]);
        assert_eq!(rig.engine.holds, [] as [u8; 0], "no HOLD crossed into the new engine");
        // The HOLD held across has no reservation left: the whole room but for the new notes' releases
        // and the target's slot is free.
        assert_eq!(rig.queue.reserved(), 3);
    }

    // Step 4: "a retained-engine device gap with a HOLD press and its release" (decision 7): the release
    // of a HOLD pressed before the gap passes and reaches the engine when it runs again; a HOLD pressed
    // in the gap is dropped, as a note-on is; controller state is kept.
    #[test]
    fn in_a_device_gap_a_hold_release_passes_and_a_fresh_press_is_dropped() {
        let mut rig = Rig::new();
        let a = midi(1, 0);
        rig.input(|r, o| r.select_target(LEAD, o)).unwrap();
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
        rig.input(|r, o| r.select_target(LEAD, o)).unwrap();
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
}
