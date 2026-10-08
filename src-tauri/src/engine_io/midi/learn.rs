//! OWNS: MIDI learn, ported from `src/app/midi-actions.ts`: learn capture, matching, consume-first, the
//! footswitch read (momentary versus latching), HOLD's control numbers, and what an edit or a lost port
//! releases. Pure: time is injected, nothing here does I/O or sends a command. A message's outcome names
//! what to run ([`Fire`]); mapping that to engine commands, persisting the list and the timer that calls
//! [`Learn::tick`] are the caller's.
//!
//! Consume-first: a message that matches a binding, or is learned, is consumed here and never reaches
//! the note router, so a CC learned onto 64 never sustains and a learned note never sounds (its note-off
//! is consumed too). Learning a CC tells the router to let go of what that port and channel's controller
//! last set ([`Outcome::release_controller`]).
//!
//! Footswitches (the rule `midi-actions.ts` states in its header). A momentary pedal sends one value on
//! press and the other on release (127, 0); a latching pedal sends one value per press, alternating; some
//! send the same value on every press. The values alone cannot tell a momentary release from a latching
//! press, so the learn gesture decides, and runs nothing while it does: after the learning press the
//! binding waits up to [`RELEASE_WAIT`] for its release. The other side seen reads as momentary: it fires
//! on each message on the press side and swallows the release (or, with HOLD, ends the capture its press
//! started). The same side again, or no release within the wait, leaves it latching: it fires on every
//! message, so a latching or same-value pedal fires once per press too. Level rather than edge on the
//! press side, so a lost release cannot swallow the next press. The wait belongs to the binding, not to
//! the learn: another learn or a cancel while the pedal is still down leaves it running, and that release
//! is checked before learn capture, so it is never a learn press.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use lf_engine::HOLD_CONTROLS;

use super::bindings::{target_for, ActionId, Binding, Kind, Target};
use super::parse::Message;

/// How long a learned binding waits for its learning press's release before it stays latching
/// (`RELEASE_WAIT_MS`).
pub const RELEASE_WAIT: Duration = Duration::from_secs(10);

/// What a message or an edit runs, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fire {
    /// Run `action` (`actions.ts` `runAction`), a lane action on `target`.
    Run { action: ActionId, target: Target },
    /// HOLD's press (`pressHold`): REC/DUB on `target`, under `control`, the number its release repeats.
    HoldPress { target: Target, control: u8 },
    /// HOLD's release (`releaseHold`): end the capture `control`'s press started.
    HoldRelease { control: u8 },
}

/// Why learn consumed a message and ran nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LearnRefusal {
    /// A HOLD press while every one of the engine's `HOLD_CONTROLS` numbers is held: the engine would
    /// remember it nowhere, so its release could not end the capture. Its release runs nothing.
    HoldControlsTaken,
}

/// What one message did to MIDI learn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Learn took the message: it never reaches the note router.
    pub consumed: bool,
    /// A learn captured this binding (the learn is over).
    pub learned: Option<Binding>,
    /// The binding list changed (a learn, or a pedal read as momentary): persist and republish it.
    pub changed: bool,
    /// What to run, in order.
    pub fire: Vec<Fire>,
    /// A CC was learned on this message's port and channel: the router lets go of that owner's sustain
    /// (64) or modulation (1), as an unplug does (`midi.ts` `releaseController`).
    pub release_controller: Option<u8>,
    /// The message was consumed and refused.
    pub refused: Option<LearnRefusal>,
}

/// A control as bindings match it (`controlKey`): one port, channel, kind and number.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Control {
    port_id: String,
    channel: u8,
    kind: Kind,
    number: u8,
}

impl Control {
    fn of(b: &Binding) -> Control {
        Control { port_id: b.port_id.clone(), channel: b.channel, kind: b.kind, number: b.number }
    }
}

/// `midi-actions.ts` module state. The release waits, HOLD presses and spent releases are keyed by
/// control: one action per message keeps a control to one binding.
#[derive(Default)]
pub struct Learn {
    /// In learn order.
    bindings: Vec<Binding>,
    learning: Option<(ActionId, Target)>,
    /// Each binding whose learning press's release may still come, until when (`waits`).
    waits: HashMap<Control, Instant>,
    /// The latest learned binding still waiting (`awaitingRelease`): a newer learn takes it over, and it
    /// clears when that binding's wait ends, whatever older waits still run.
    awaiting: Option<Control>,
    /// Each HOLD press, by its control, until its release ends the capture: the number the engine knows
    /// the control by (`held`).
    held: HashMap<Control, u8>,
    /// Controls whose HOLD press an edit released while the pedal was down: the pedal's own release still
    /// comes, and is spent on nothing (as latching it would run the action again).
    spent: HashSet<Control>,
}

impl Learn {
    /// `consume`: a learned pedal's release first, then learn capture, then the bound message.
    pub fn consume(&mut self, port_id: &str, port_name: &str, msg: &Message, now: Instant) -> Outcome {
        let (kind, number, high) = match *msg {
            Message::Cc { controller, value, .. } => (Kind::Cc, controller, value >= 64),
            Message::NoteOn { note, .. } => (Kind::Note, note, true),
            // A note-on at velocity 0 reaches learn as a note-off (`parse`), as in midi.ts.
            Message::NoteOff { note, .. } => (Kind::Note, note, false),
            Message::PitchBend { .. } => return Outcome::default(),
        };
        let control = Control { port_id: port_id.to_owned(), channel: msg.channel(), kind, number };
        let found = self.position(&control);

        // A learned pedal's release is its release, before anything else: even with a learn listening
        // again, it is never a learn press. A wait past its deadline the timer has not ended yet has
        // timed out all the same.
        let wait = self.waits.get(&control).copied();
        if let (Some(i), Some(deadline)) = (found, wait) {
            self.end_wait(&control);
            if now < deadline && high != self.bindings[i].press_high {
                let next = Binding { momentary: true, ..self.bindings[i].clone() };
                let fire = self.update(i, next);
                return Outcome { consumed: true, changed: true, fire, ..Outcome::default() };
            }
            // The press side again with no release between, or too late: latching. It runs (or is
            // learned) below.
        }

        // CC 120–127 are channel-mode messages (all sound off, all notes off, …), never a switch; a note
        // learns only on its note-on.
        if let Some((action, target)) = self.learning.filter(|_| if kind == Kind::Cc { number < 120 } else { high }) {
            let binding = Binding {
                port_id: port_id.to_owned(),
                port_name: port_name.to_owned(),
                channel: control.channel,
                kind,
                number,
                action,
                target,
                press_high: high,
                momentary: false,
                hold: false,
            };
            // One action per message: learning a bound message again moves it (its wait ended above),
            // and lets go of a HOLD it held.
            let fire = self.release_held(&control).into_iter().collect();
            self.bindings.retain(|b| Control::of(b) != control);
            self.spent.remove(&control);
            self.bindings.push(binding.clone());
            self.learning = None;
            self.waits.insert(control.clone(), now + RELEASE_WAIT);
            self.awaiting = Some(control);
            return Outcome {
                consumed: true,
                learned: Some(binding),
                changed: true,
                fire,
                release_controller: (kind == Kind::Cc).then_some(number),
                refused: None,
            };
        }

        let Some(i) = found else { return Outcome::default() };
        let mut out = Outcome { consumed: true, ..Outcome::default() };
        let b = &self.bindings[i];
        // The first message after an edit released this pedal's HOLD: its release is spent, a press runs.
        if self.spent.remove(&control) && high != b.press_high {
            return out;
        }
        let (action, target) = (b.action, b.target);
        if !b.momentary {
            out.fire.push(Fire::Run { action, target });
        } else if high == b.press_high {
            if b.hold {
                match self.hold_control(&control) {
                    Some(n) => {
                        self.held.insert(control, n);
                        out.fire.push(Fire::HoldPress { target, control: n });
                    }
                    None => out.refused = Some(LearnRefusal::HoldControlsTaken),
                }
            } else {
                out.fire.push(Fire::Run { action, target });
            }
        } else {
            out.fire.extend(self.release_held(&control));
        }
        out
    }

    /// End every release wait whose deadline has come (the TS wait's timer). True when
    /// [`Learn::awaiting_release`] changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let before = self.awaiting.clone();
        let due: Vec<Control> = self.waits.iter().filter(|(_, d)| **d <= now).map(|(c, _)| c.clone()).collect();
        for c in &due {
            self.end_wait(c);
        }
        self.awaiting != before
    }

    /// When [`Learn::tick`] next has work, or `None` while nothing waits.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.waits.values().min().copied()
    }

    /// Learn the next CC or note-on, from any port, onto `action`: a lane action on `target`, which a
    /// global action, or a track that does not exist, drops (the selected track).
    pub fn learn(&mut self, action: ActionId, target: Target) {
        self.learning = Some((action, target_for(action, target)));
    }

    /// Stop listening. A learned pedal's wait for its release goes on. True when a learn was pending.
    pub fn cancel_learn(&mut self) -> bool {
        self.learning.take().is_some()
    }

    /// What the next CC or note-on will be learned onto.
    pub fn learning(&self) -> Option<(ActionId, Target)> {
        self.learning
    }

    /// The latest learned binding still waiting for its release (the learn row's hint).
    pub fn awaiting_release(&self) -> Option<&Binding> {
        let c = self.awaiting.as_ref()?;
        self.bindings.iter().find(|b| Control::of(b) == *c)
    }

    /// The learned bindings, in learn order.
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    /// Replace the list (a store load). Each binding keeps its invariants (`Binding`'s target and HOLD
    /// rules). A control whose binding is gone or changed loses its release wait and its HOLD press,
    /// which is released (and, while the control is still bound, its pedal's release is spent, as
    /// [`Learn::set_momentary`]'s); an unchanged binding keeps both.
    pub fn set_bindings(&mut self, list: Vec<Binding>) -> Vec<Fire> {
        let old = std::mem::replace(&mut self.bindings, list.into_iter().map(Binding::normalized).collect());
        let first = |list: &[Binding], c: &Control| list.iter().find(|b| Control::of(b) == *c).cloned();
        let waiting: Vec<Control> = self.waits.keys().cloned().collect();
        for c in waiting {
            if first(&old, &c) != first(&self.bindings, &c) {
                self.end_wait(&c);
            }
        }
        let mut fire = Vec::new();
        for c in self.held_in_order(|_| true) {
            let now = first(&self.bindings, &c);
            if first(&old, &c) != now {
                if now.is_some() {
                    self.spent.insert(c.clone());
                }
                fire.extend(self.release_held(&c));
            }
        }
        let bindings = &self.bindings;
        self.spent.retain(|c| bindings.iter().any(|b| Control::of(b) == *c));
        fire
    }

    /// Drop binding `index` (`forget`): its messages reach the play path again, and a HOLD it held is
    /// released.
    pub fn forget(&mut self, index: usize) -> Vec<Fire> {
        if index >= self.bindings.len() {
            return Vec::new();
        }
        let c = Control::of(&self.bindings.remove(index));
        self.end_wait(&c);
        let fire = self.release_held(&c).into_iter().collect();
        if self.position(&c).is_none() {
            self.spent.remove(&c);
        }
        fire
    }

    /// Read binding `index`'s pedal as momentary or latching (the list's switch). A latching pedal has no
    /// HOLD.
    pub fn set_momentary(&mut self, index: usize, momentary: bool) -> Vec<Fire> {
        let Some(b) = self.bindings.get(index).cloned() else { return Vec::new() };
        self.update(index, Binding { momentary, hold: b.hold && momentary, ..b })
    }

    /// HOLD on or off for binding `index`: on for a momentary REC/DUB pedal only (refused otherwise,
    /// nothing changed).
    pub fn set_hold(&mut self, index: usize, hold: bool) -> Vec<Fire> {
        let Some(b) = self.bindings.get(index).cloned() else { return Vec::new() };
        if hold && !(b.momentary && b.action == ActionId::RecDub) {
            return Vec::new();
        }
        self.update(index, Binding { hold, ..b })
    }

    /// Port `port_id` is gone: release each HOLD press on it, as its pedal's release never comes. A spent
    /// release there is forgotten too: it can no longer come either.
    pub fn port_gone(&mut self, port_id: &str) -> Vec<Fire> {
        let mut fire = Vec::new();
        for c in self.held_in_order(|c| c.port_id == port_id) {
            fire.extend(self.release_held(&c));
        }
        self.spent.retain(|c| c.port_id != port_id);
        fire
    }

    /// The engine was rebuilt: it holds no capture for any HOLD, so the bookkeeping goes with no release.
    /// A pedal still down then releases nothing.
    pub fn clear_holds(&mut self) {
        self.held.clear();
    }

    fn position(&self, c: &Control) -> Option<usize> {
        self.bindings.iter().position(|b| Control::of(b) == *c)
    }

    /// End control `c`'s wait for its release, if it has one.
    fn end_wait(&mut self, c: &Control) {
        if self.waits.remove(c).is_some() && self.awaiting.as_ref() == Some(c) {
            self.awaiting = None;
        }
    }

    /// Replace binding `index` with `next`, in place (`update`). Its wait ends (its kind is settled); a
    /// HOLD it held is released and its pedal's release spent.
    fn update(&mut self, index: usize, next: Binding) -> Vec<Fire> {
        let c = Control::of(&self.bindings[index]);
        self.end_wait(&c);
        if self.held.contains_key(&c) {
            self.spent.insert(c.clone());
        }
        let fire = self.release_held(&c).into_iter().collect();
        self.bindings[index] = next.normalized();
        fire
    }

    /// The number HOLD's press on control `c` goes to the engine with, which its release repeats
    /// (`holdControl`): the one it holds already (its release was lost), else the smallest no held
    /// control has, so two pedals down at once are two controls to the engine. `None` when all
    /// `HOLD_CONTROLS` are held.
    fn hold_control(&self, c: &Control) -> Option<u8> {
        if let Some(&own) = self.held.get(c) {
            return Some(own);
        }
        (0..HOLD_CONTROLS as u8).find(|n| !self.held.values().any(|h| h == n))
    }

    /// Release the HOLD press control `c` holds, if any (`releaseHeld`).
    fn release_held(&mut self, c: &Control) -> Option<Fire> {
        self.held.remove(c).map(|control| Fire::HoldRelease { control })
    }

    /// The held controls `keep` picks, in their control numbers' order (so releases come out stable).
    fn held_in_order(&self, keep: impl Fn(&Control) -> bool) -> Vec<Control> {
        let mut held: Vec<(&Control, u8)> = self.held.iter().filter(|(c, _)| keep(c)).map(|(c, &n)| (c, n)).collect();
        held.sort_by_key(|&(_, n)| n);
        held.into_iter().map(|(c, _)| c.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_io::midi::parse::parse;

    const A: &str = "port a";
    const B: &str = "port b";
    const S: Duration = Duration::from_secs(1);

    fn cc(channel: u8, controller: u8, value: u8) -> Message {
        Message::Cc { channel, controller, value }
    }

    /// The bytes as the port delivers them (a note-on at velocity 0 is a note-off there).
    fn raw(bytes: [u8; 3]) -> Message {
        parse(&bytes).unwrap()
    }

    fn send(l: &mut Learn, port: &str, m: Message, at: Instant) -> Outcome {
        l.consume(port, "Pedal", &m, at)
    }

    fn run(action: ActionId) -> Fire {
        Fire::Run { action, target: None }
    }

    /// Learn `msgs` (one burst on port `A` at `at`) onto `action`.
    fn learn(l: &mut Learn, action: ActionId, msgs: &[Message], at: Instant) {
        l.learn(action, None);
        for &m in msgs {
            send(l, A, m, at);
        }
    }

    /// A momentary REC/DUB pedal with HOLD on CC `number`, port `port`, channel 0.
    fn hold_pedal(port: &str, number: u8) -> Binding {
        Binding {
            port_id: port.into(),
            port_name: "Pedal".into(),
            channel: 0,
            kind: Kind::Cc,
            number,
            action: ActionId::RecDub,
            target: None,
            press_high: true,
            momentary: true,
            hold: true,
        }
    }

    // midi-actions.ts header: a learned CC64 never sustains and a learned note's note-off is consumed;
    // unlearned traffic reaches the play path exactly as before. A binding matches on its port id,
    // channel, kind and number; the port's name is for display only.
    #[test]
    fn learned_messages_are_consumed_and_unlearned_ones_are_not() {
        let mut l = Learn::default();
        let t = Instant::now();
        learn(&mut l, ActionId::Undo, &[cc(0, 64, 127)], t);
        learn(&mut l, ActionId::PlayAll, &[raw([0x92, 60, 100])], t);
        let later = t + RELEASE_WAIT;
        for m in [cc(0, 64, 0), cc(0, 64, 127), raw([0x92, 60, 0]), raw([0x82, 60, 0]), raw([0x92, 60, 90])] {
            assert!(send(&mut l, A, m, later).consumed, "{m:?}");
        }
        assert!(l.consume(A, "Renamed", &cc(0, 64, 127), later).consumed, "matched by port id, not name");
        for (port, m) in [
            (B, cc(0, 64, 127)),
            (A, cc(1, 64, 127)),
            (A, cc(0, 65, 127)),
            (A, raw([0x90, 64, 100])),
            (A, raw([0x92, 61, 100])),
            (A, raw([0x82, 61, 0])),
            (A, Message::PitchBend { channel: 0, value: 8192 }),
        ] {
            assert_eq!(send(&mut l, port, m, later), Outcome::default(), "{port} {m:?}");
        }
    }

    // midi-actions.ts consume: "CC 120–127 are channel-mode messages …, never a switch", and a note learns
    // only on a note-on (high): a note-off, 0x80 or velocity 0, leaves the learn listening.
    #[test]
    fn channel_mode_ccs_and_note_offs_never_learn() {
        let mut l = Learn::default();
        let t = Instant::now();
        l.learn(ActionId::StopAll, None);
        for n in 120..=127 {
            assert_eq!(send(&mut l, A, cc(0, n, 127), t), Outcome::default(), "CC{n}");
        }
        for m in [raw([0x80, 60, 64]), raw([0x90, 60, 0])] {
            assert_eq!(send(&mut l, A, m, t), Outcome::default(), "{m:?}");
        }
        assert_eq!(l.learning(), Some((ActionId::StopAll, None)));
        assert!(l.bindings().is_empty());
        let out = send(&mut l, A, cc(0, 119, 127), t);
        assert!(out.consumed && out.learned.is_some());
        assert_eq!(l.learning(), None);
    }

    // midi.ts releaseController: learning a CC lets go of that port and channel's controller; a note
    // learns none.
    #[test]
    fn learning_a_cc_releases_its_controller() {
        let mut l = Learn::default();
        let t = Instant::now();
        l.learn(ActionId::Undo, None);
        assert_eq!(send(&mut l, A, cc(3, 64, 0), t).release_controller, Some(64));
        l.learn(ActionId::Undo, None);
        assert_eq!(send(&mut l, A, cc(3, 1, 90), t).release_controller, Some(1));
        l.learn(ActionId::Undo, None);
        assert_eq!(send(&mut l, A, raw([0x93, 64, 100]), t).release_controller, None);
    }

    // midi-actions.ts Footswitches and consume: each binding waits RELEASE_WAIT for its own release; two
    // pedals can wait at once; the wait survives a cancel and a new learn, and its release is checked
    // before learn capture, so it is never a learn press.
    #[test]
    fn release_waits_are_per_binding_and_outlive_cancel_and_a_new_learn() {
        assert_eq!(RELEASE_WAIT, Duration::from_secs(10));
        let mut l = Learn::default();
        let t = Instant::now();
        learn(&mut l, ActionId::Undo, &[cc(0, 20, 127)], t);
        learn(&mut l, ActionId::Mute, &[cc(0, 21, 127)], t + S);
        l.learn(ActionId::Clear, None);
        assert!(l.cancel_learn());
        assert!(!l.cancel_learn(), "nothing pending the second time");
        l.learn(ActionId::Reverse, None);
        let out = send(&mut l, A, cc(0, 20, 0), t + 9 * S);
        assert_eq!(out, Outcome { consumed: true, changed: true, ..Outcome::default() });
        assert_eq!(l.learning(), Some((ActionId::Reverse, None)), "the release was no learn press");
        let out = send(&mut l, A, cc(0, 21, 0), t + 10 * S);
        assert!(out.consumed && out.changed && out.learned.is_none() && out.fire.is_empty());
        assert!(l.bindings().iter().all(|b| b.momentary));
        assert_eq!(l.bindings().len(), 2);
    }

    // midi-actions.ts awaitingRelease: the latest learned binding still waiting; its timer, not the next
    // message, ends it at RELEASE_WAIT.
    #[test]
    fn awaiting_release_is_the_latest_wait_and_ends_at_its_deadline() {
        let mut l = Learn::default();
        let t = Instant::now();
        assert_eq!(l.next_deadline(), None);
        learn(&mut l, ActionId::Undo, &[cc(0, 20, 127)], t);
        assert_eq!(l.awaiting_release().map(|b| b.number), Some(20));
        learn(&mut l, ActionId::Undo, &[cc(0, 21, 127)], t + S);
        assert_eq!(l.awaiting_release().map(|b| b.number), Some(21));
        assert_eq!(l.next_deadline(), Some(t + RELEASE_WAIT));
        assert!(!l.tick(t + RELEASE_WAIT - Duration::from_millis(1)));
        assert!(!l.tick(t + RELEASE_WAIT), "CC20's wait ends; CC21 is still the latest");
        assert_eq!(l.next_deadline(), Some(t + S + RELEASE_WAIT));
        assert!(l.tick(t + S + RELEASE_WAIT));
        assert_eq!((l.awaiting_release(), l.next_deadline()), (None, None));
        // Its wait over, CC21's late release is a latching press.
        assert_eq!(send(&mut l, A, cc(0, 21, 0), t + 20 * S).fire, [run(ActionId::Undo)]);

        // The release ends the latest wait at once; an older wait ending later leaves the hint empty.
        learn(&mut l, ActionId::Mute, &[cc(0, 30, 127)], t);
        learn(&mut l, ActionId::Mute, &[cc(0, 31, 127)], t);
        send(&mut l, A, cc(0, 31, 0), t + S);
        assert_eq!(l.awaiting_release(), None);
        assert!(!l.tick(t + RELEASE_WAIT));
    }

    // midi-actions.ts Footswitches: the other side within the wait reads momentary (press fires, release
    // swallowed, level-based so a lost release cannot swallow a press); the same side or a timeout reads
    // latching (every message fires, a same-value pedal once per press); a reversed pedal presses on 0.
    #[test]
    fn the_learn_gesture_reads_momentary_latching_and_reversed_pedals() {
        let mut l = Learn::default();
        let t = Instant::now();
        let later = t + 2 * RELEASE_WAIT;
        let fired = |l: &mut Learn, msgs: &[Message]| -> Vec<usize> {
            msgs.iter().map(|&m| send(l, A, m, later).fire.len()).collect()
        };

        learn(&mut l, ActionId::NextTrack, &[cc(0, 21, 127), cc(0, 21, 0)], t);
        assert!(l.bindings()[0].momentary);
        assert_eq!(fired(&mut l, &[cc(0, 21, 127), cc(0, 21, 0), cc(0, 21, 127), cc(0, 21, 127)]), [1, 0, 1, 1]);

        learn(&mut l, ActionId::NextTrack, &[cc(0, 22, 127)], t);
        assert_eq!(send(&mut l, A, cc(0, 22, 0), t + RELEASE_WAIT).fire, [run(ActionId::NextTrack)], "timed out");
        assert!(!l.bindings()[1].momentary);
        assert_eq!(fired(&mut l, &[cc(0, 22, 127), cc(0, 22, 0)]), [1, 1]);

        learn(&mut l, ActionId::NextTrack, &[cc(0, 23, 127)], t);
        assert_eq!(send(&mut l, A, cc(0, 23, 127), t + S).fire, [run(ActionId::NextTrack)], "same side");
        assert!(!l.bindings()[2].momentary);
        assert_eq!(fired(&mut l, &[cc(0, 23, 127), cc(0, 23, 127)]), [1, 1]);

        learn(&mut l, ActionId::NextTrack, &[cc(0, 24, 0), cc(0, 24, 127)], t);
        assert_eq!((l.bindings()[3].press_high, l.bindings()[3].momentary), (false, true));
        assert_eq!(fired(&mut l, &[cc(0, 24, 0), cc(0, 24, 127), cc(0, 24, 0)]), [1, 0, 1]);

        learn(&mut l, ActionId::PrevTrack, &[raw([0x91, 60, 100]), raw([0x91, 60, 0])], t);
        assert!(l.bindings()[4].momentary);
        assert_eq!(fired(&mut l, &[raw([0x91, 60, 100]), raw([0x81, 60, 0]), raw([0x91, 60, 90]), raw([0x91, 60, 0])]), [1, 0, 1, 0]);
    }

    // midi-actions.ts update and `spent`: an edit while a HOLD pedal is down releases its HOLD, and the
    // pedal's own release is spent on nothing (as latching it would run the action); the press after it
    // runs.
    #[test]
    fn an_edit_under_a_held_hold_pedal_spends_its_release() {
        let mut l = Learn::default();
        let t = Instant::now();
        l.set_bindings(vec![hold_pedal(A, 20), hold_pedal(A, 21)]);
        assert_eq!(send(&mut l, A, cc(0, 20, 127), t).fire, [Fire::HoldPress { target: None, control: 0 }]);
        assert_eq!(l.set_momentary(0, false), [Fire::HoldRelease { control: 0 }]);
        assert_eq!((l.bindings()[0].momentary, l.bindings()[0].hold), (false, false));
        let out = send(&mut l, A, cc(0, 20, 0), t);
        assert!(out.consumed && out.fire.is_empty(), "the release is spent");
        assert_eq!(send(&mut l, A, cc(0, 20, 0), t).fire, [Fire::Run { action: ActionId::RecDub, target: None }]);

        assert_eq!(send(&mut l, A, cc(0, 21, 127), t).fire, [Fire::HoldPress { target: None, control: 0 }]);
        assert_eq!(l.set_hold(1, false), [Fire::HoldRelease { control: 0 }]);
        assert!(send(&mut l, A, cc(0, 21, 0), t).fire.is_empty());
        assert_eq!(send(&mut l, A, cc(0, 21, 127), t).fire, [Fire::Run { action: ActionId::RecDub, target: None }]);
    }

    // actions.ts: every action is learnable; a lane action keeps its target, a global one gets none, a
    // track that does not exist is none; HOLD only on a momentary recDub, and a latching read drops it.
    #[test]
    fn all_25_actions_learn_with_a_target_on_lane_actions_only_and_hold_on_a_momentary_rec_dub_only() {
        let mut l = Learn::default();
        let t = Instant::now();
        for (n, action) in ActionId::ALL.into_iter().enumerate() {
            l.learn(action, Some(3));
            send(&mut l, A, cc(0, n as u8, 127), t);
            let want = if action.is_lane() { Some(3) } else { None };
            assert_eq!(l.bindings()[n].target, want, "{action:?}");
            assert_eq!(send(&mut l, A, cc(0, n as u8, 127), t + RELEASE_WAIT).fire, [Fire::Run { action, target: want }]);
        }
        l.learn(ActionId::Undo, Some(5));
        assert_eq!(l.learning(), Some((ActionId::Undo, None)));
        l.cancel_learn();

        assert!(l.set_hold(0, true).is_empty());
        assert!(!l.bindings()[0].hold, "a latching recDub has no HOLD");
        l.set_momentary(0, true);
        l.set_momentary(1, true);
        l.set_hold(1, true);
        assert!(!l.bindings()[1].hold, "playStop has no HOLD");
        l.set_hold(0, true);
        assert!(l.bindings()[0].hold);
        assert_eq!(send(&mut l, A, cc(0, 0, 127), t + RELEASE_WAIT).fire, [Fire::HoldPress { target: Some(3), control: 0 }]);
        l.set_momentary(0, false);
        assert!(!l.bindings()[0].hold);
    }

    // midi-actions.ts holdControl, bounded by the engine's HOLD_CONTROLS: a press takes the smallest
    // number no held control has (its own if its release was lost) and its release repeats it; with
    // every number held a press is refused, consumed and fires nothing, and so does its release.
    #[test]
    fn hold_presses_take_the_smallest_free_control_and_refuse_past_the_engines_bound() {
        assert_eq!(HOLD_CONTROLS, 16);
        let mut l = Learn::default();
        let t = Instant::now();
        l.set_bindings((0..=16).map(|n| hold_pedal(A, n)).collect());
        let press = |l: &mut Learn, n: u8| send(l, A, cc(0, n, 127), t);
        let release = |l: &mut Learn, n: u8| send(l, A, cc(0, n, 0), t);

        assert_eq!(press(&mut l, 0).fire, [Fire::HoldPress { target: None, control: 0 }]);
        assert_eq!(press(&mut l, 1).fire, [Fire::HoldPress { target: None, control: 1 }]);
        assert_eq!(release(&mut l, 0).fire, [Fire::HoldRelease { control: 0 }]);
        assert_eq!(press(&mut l, 2).fire, [Fire::HoldPress { target: None, control: 0 }]);
        assert_eq!(press(&mut l, 1).fire, [Fire::HoldPress { target: None, control: 1 }], "a lost release keeps its own");
        for n in 3..=15 {
            assert_eq!(press(&mut l, n).fire, [Fire::HoldPress { target: None, control: n - 1 }]);
        }
        assert_eq!(press(&mut l, 0).fire, [Fire::HoldPress { target: None, control: 15 }]);
        let refused = press(&mut l, 16);
        assert_eq!(refused, Outcome { consumed: true, refused: Some(LearnRefusal::HoldControlsTaken), ..Outcome::default() });
        assert_eq!(release(&mut l, 16), Outcome { consumed: true, ..Outcome::default() });
        assert_eq!(release(&mut l, 5).fire, [Fire::HoldRelease { control: 4 }]);
        assert_eq!(press(&mut l, 16).fire, [Fire::HoldPress { target: None, control: 4 }]);
    }

    // midi-actions.ts forget, update and portGone: a forgotten, edited or replaced binding and a port that
    // goes release their HOLD; an unchanged binding keeps it; an engine rebuild forgets them unreleased.
    #[test]
    fn edits_and_a_lost_port_release_a_held_hold() {
        let mut l = Learn::default();
        let t = Instant::now();
        let list = vec![hold_pedal(A, 20), hold_pedal(A, 21), hold_pedal(B, 20), hold_pedal(A, 22)];
        l.set_bindings(list.clone());
        for (port, n) in [(A, 20), (A, 21), (B, 20), (A, 22)] {
            send(&mut l, port, cc(0, n, 127), t);
        }
        assert_eq!(l.forget(0), [Fire::HoldRelease { control: 0 }]);
        assert!(!send(&mut l, A, cc(0, 20, 0), t).consumed, "forgotten: the release reaches the play path");

        // The same list keeps its holds; a changed binding and a dropped one release theirs.
        assert_eq!(l.set_bindings(list[1..].to_vec()), []);
        let changed = Binding { action: ActionId::Undo, ..list[1].clone() };
        assert_eq!(l.set_bindings(vec![changed, list[2].clone()]), [Fire::HoldRelease { control: 1 }, Fire::HoldRelease { control: 3 }]);
        assert!(send(&mut l, A, cc(0, 21, 0), t).fire.is_empty(), "the changed binding's release is spent");

        assert_eq!(l.port_gone(A), []);
        assert_eq!(l.port_gone(B), [Fire::HoldRelease { control: 2 }]);

        send(&mut l, B, cc(0, 20, 127), t);
        l.clear_holds();
        assert_eq!(send(&mut l, B, cc(0, 20, 0), t), Outcome { consumed: true, ..Outcome::default() });
        assert_eq!(send(&mut l, B, cc(0, 20, 127), t).fire, [Fire::HoldPress { target: None, control: 0 }]);
    }

    // midi-actions.ts consume: "One action per message: learning a bound message again moves it", and a
    // HOLD it held is released.
    #[test]
    fn learning_a_bound_message_again_moves_it() {
        let mut l = Learn::default();
        let t = Instant::now();
        learn(&mut l, ActionId::Undo, &[cc(0, 20, 127)], t);
        learn(&mut l, ActionId::Mute, &[cc(0, 21, 127)], t);
        learn(&mut l, ActionId::PlayAll, &[cc(0, 20, 127)], t + RELEASE_WAIT);
        let got: Vec<(u8, ActionId)> = l.bindings().iter().map(|b| (b.number, b.action)).collect();
        assert_eq!(got, [(21, ActionId::Mute), (20, ActionId::PlayAll)]);

        l.set_bindings(vec![hold_pedal(A, 30)]);
        send(&mut l, A, cc(0, 30, 127), t);
        l.learn(ActionId::Undo, None);
        let out = send(&mut l, A, cc(0, 30, 127), t);
        assert_eq!(out.fire, [Fire::HoldRelease { control: 0 }]);
        assert_eq!((l.bindings().len(), l.bindings()[0].action, l.bindings()[0].hold), (1, ActionId::Undo, false));
    }
}
