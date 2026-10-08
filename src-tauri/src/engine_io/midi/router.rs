//! OWNS: the one note router for every note source (plan decision 1), ported from
//! `src/ui/state/input-router.ts`, the play-path branch of `src/ui/state/midi.ts` and the order of
//! `src/ui/state/instrument.ts` `routeEngine`: which owner holds each note (a MIDI port's channel, or
//! a pointer or key of a WebView document), each owner's sustain pedal and the releases it defers, the
//! wheels with the last moved one winning, the note target, which notes the current engine was told
//! to sound, and the held-note set the on-screen keyboard lights. The engine plays what it is told
//! (`lf_engine::Command::NoteOn`); the router decides when to tell it. Each call hands its commands
//! out as one batch, in order, for `super::queue` to take whole.
//!
//! # Rules
//!
//! - **A note sounds from its first hold until its last owner lets go and no pedal defers it.** The
//!   first hold sends `NoteOn` (velocity `v/127`; velocity 0 is a note-off); the last release sends
//!   `NoteOff`, or leaves it to the pedals of the owners that let go under them. Striking a note only a
//!   pedal keeps sounding ends that voice first: `NoteOff` then `NoteOn`, one batch.
//! - **Sustain is per owner.** A pedal-up releases only what its owner deferred; CC123 releases only
//!   its owner's keys and honours its own pedal; an unplug (`disconnected`) also lifts the pedal and
//!   forgets the owner's wheels.
//! - **The last moved wheel wins** across owners; when its owner goes, the survivor's value applies.
//!   Only a value that changed is sent. A new target needs none: the engine hands the instrument it
//!   selects the wheels it keeps (`Instruments::select`; a plugin slot gets no wheels, D12).
//! - **A target switch releases what sounds, then switches** (`routeEngine`), in one batch: every note
//!   held or sustained gets its `NoteOff` before `SelectInstrument`, and is forgotten (a key down across
//!   the switch releases nothing); pedals and wheels stay. `NoteTarget::Off` routes nowhere (a plugin
//!   swap while it unloads). The caller switches on a slot pick or a slot's new source, never per note.
//! - **A refused attack records no owner.** The queue decides after the router; when it refuses a
//!   `note_on`'s batch, [`Router::attack_refused`] undoes what that call recorded. Every other batch
//!   the router makes (releases, wheels, the target) is one the queue always takes (its reservations),
//!   so nothing else is ever undone.
//! - **A rebuilt engine sounds nothing** ([`Router::engine_rebuilt`]): the owners and pedals stay, so a
//!   later release is harmless (nothing is sent for a note the new engine never sounded) and a fresh
//!   press attacks again; the target and wheels the router last sent reach the new engine through the
//!   settings replay alone.
//! - **A WebView document's owners live as long as its epoch** (`frontendEpoch`, never 0): a new
//!   document ([`Router::ui_epoch`]) releases the older documents' holds and refuses their late events;
//!   a window blur releases the document's holds.
//! - **[`HeldNotes`] is what is physically held, not sustained,** published without the router's lock.
//!
//! Collections keep insertion order, as the TS `Map`s and `Set`s do, so a release sweeps its notes in
//! the order the web sent their note-offs.

use std::sync::atomic::{fence, AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use std::sync::Arc;

use lf_engine::{Command, NoteTarget};

use super::parse::Message;
use super::queue::Out;

/// `midi.ts` `PITCH_BEND_RANGE_SEMITONES`: the wheel's full throw is ±2 semitones.
const PITCH_BEND_RANGE_SEMITONES: f64 = 2.0;

/// Who holds a note: a physical source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// A connection (one open port) and a channel (`midi.ts` `midiOwner(port, channel)`).
    Midi { conn: u32, channel: u8 },
    /// A pointer or a key of the WebView document of `epoch` (`frontendEpoch`); `id` is the UI's own,
    /// `pointer:<pointerId>` or `key:<KeyboardEvent.code>` (`Keyboard.tsx`).
    Ui { epoch: u64, id: String },
}

/// What [`Router::attack_refused`] undoes: the last `note_on`'s attack.
struct Undo {
    owner: Owner,
    note: u8,
    /// The call added the owner to the note.
    added: bool,
    /// The note sounded before it (a re-strike under the pedal).
    sounding: bool,
}

#[derive(Default)]
pub struct Router {
    /// Note → the owners holding it down (`heldBySource`).
    held: Vec<(u8, Vec<Owner>)>,
    /// Owners whose pedal is down.
    pedals: Vec<Owner>,
    /// Note → the pedals deferring its release (`sustained`).
    sustained: Vec<(u8, Vec<Owner>)>,
    /// Each owner's wheel, last moved last.
    bends: Vec<(Owner, f64)>,
    modulation: Vec<(Owner, f64)>,
    /// What the router last sent: the engine keeps it, and the settings memory replays it.
    pitch_bend: f64,
    mod_depth: f64,
    target: Option<NoteTarget>,
    /// Notes this engine was told to sound and not yet to release, bit n for note n.
    sounding: u128,
    /// The current document's `frontendEpoch` (0: none yet, so no UI event is taken).
    ui_epoch: u64,
    undo: Option<Undo>,
    published: HeldNotes,
}

fn entry(list: &mut Vec<(u8, Vec<Owner>)>, note: u8) -> &mut Vec<Owner> {
    let i = match list.iter().position(|(n, _)| *n == note) {
        Some(i) => i,
        None => {
            list.push((note, Vec::new()));
            list.len() - 1
        }
    };
    &mut list[i].1
}

fn has(list: &[(u8, Vec<Owner>)], note: u8) -> bool {
    list.iter().any(|(n, _)| *n == note)
}

/// Remove `owner` from `note`'s set, dropping the note when its set empties. True when it was there.
fn remove(list: &mut Vec<(u8, Vec<Owner>)>, note: u8, owner: &Owner) -> bool {
    let Some(i) = list.iter().position(|(n, _)| *n == note) else { return false };
    let Some(k) = list[i].1.iter().position(|o| o == owner) else { return false };
    list[i].1.remove(k);
    if list[i].1.is_empty() {
        list.remove(i);
    }
    true
}

/// Move `owner`'s value to the end (`Map.delete` then `Map.set`).
fn set_last(list: &mut Vec<(Owner, f64)>, owner: &Owner, value: f64) {
    list.retain(|(o, _)| o != owner);
    list.push((owner.clone(), value));
}

impl Router {
    /// The play-path branch of `midi.ts` `parseMidiMessage`, for a message MIDI learn did not consume.
    pub(crate) fn message(&mut self, owner: &Owner, message: Message, out: &mut Vec<Out>) {
        match message {
            Message::NoteOn { note, velocity, .. } => self.note_on(owner, note, velocity, out),
            Message::NoteOff { note, .. } => self.note_off(owner, note, out),
            Message::Cc { controller: 64, value, .. } => self.sustain(owner, value >= 64, out),
            Message::Cc { controller: 1, value, .. } => self.modulation(owner, f64::from(value) / 127.0, out),
            Message::Cc { controller: 123, .. } => self.release_owner(owner, false, out),
            Message::Cc { .. } => {}
            Message::PitchBend { value, .. } => {
                let raw = f64::from(value) - 8192.0;
                self.pitch_bend(owner, raw / 8192.0 * PITCH_BEND_RANGE_SEMITONES, out);
            }
        }
    }

    /// Whether an event of `owner` is taken: a replaced document's are not.
    fn admits(&self, owner: &Owner) -> bool {
        match owner {
            Owner::Midi { .. } => true,
            Owner::Ui { epoch, .. } => *epoch == self.ui_epoch,
        }
    }

    fn sounds(&self, note: u8) -> bool {
        self.sounding >> note & 1 == 1
    }

    fn set_sounding(&mut self, note: u8, on: bool) {
        if on {
            self.sounding |= 1 << note;
        } else {
            self.sounding &= !(1 << note);
        }
    }

    /// `handle({ type: 'on' })`: the first owner to hold a note sounds it (velocity `clampMidi(v) / 127`);
    /// a strike of a note only a pedal keeps sounding ends that voice first. Velocity 0 is a note-off.
    pub fn note_on(&mut self, owner: &Owner, note: u8, velocity: u8, out: &mut Vec<Out>) {
        if velocity == 0 {
            return self.note_off(owner, note, out);
        }
        self.undo = None;
        if note > 127 || !self.admits(owner) {
            return;
        }
        let sounding = self.sounds(note);
        let owners = entry(&mut self.held, note);
        let first_hold = owners.is_empty();
        let added = !owners.contains(owner);
        if added {
            owners.push(owner.clone());
        }
        // Another owner holds it and it sounds: nothing to strike.
        if !(sounding && !first_hold) {
            if sounding {
                out.push(Out::new(Command::NoteOff(note)));
            }
            out.push(Out::new(Command::NoteOn(note, f32::from(velocity.min(127)) / 127.0)));
            self.set_sounding(note, true);
            self.undo = Some(Undo { owner: owner.clone(), note, added, sounding });
        }
        self.publish();
    }

    /// The queue refused the batch the last [`Router::note_on`] made: forget the owner it recorded and
    /// put the note back as it was. Nothing after any other call.
    pub fn attack_refused(&mut self) {
        let Some(Undo { owner, note, added, sounding }) = self.undo.take() else { return };
        if added {
            remove(&mut self.held, note, &owner);
        }
        self.set_sounding(note, sounding);
        self.publish();
    }

    /// `handle({ type: 'off' })`: only an owner that holds the note lets it go; while its pedal is down
    /// the release waits for the pedal.
    pub fn note_off(&mut self, owner: &Owner, note: u8, out: &mut Vec<Out>) {
        self.undo = None;
        if note > 127 || !self.admits(owner) {
            return;
        }
        self.let_go_of(owner, note, out);
        self.publish();
    }

    fn let_go_of(&mut self, owner: &Owner, note: u8, out: &mut Vec<Out>) {
        if !remove(&mut self.held, note, owner) {
            return;
        }
        if self.pedals.contains(owner) {
            let owners = entry(&mut self.sustained, note);
            if !owners.contains(owner) {
                owners.push(owner.clone());
            }
        }
        self.release_if_unheld(note, out);
    }

    fn release_if_unheld(&mut self, note: u8, out: &mut Vec<Out>) {
        if !has(&self.held, note) && !has(&self.sustained, note) && self.sounds(note) {
            out.push(Out::new(Command::NoteOff(note)));
            self.set_sounding(note, false);
        }
    }

    /// `setSustain`: CC64 is scoped to its owner; pedal-up releases what it deferred and nobody holds.
    pub fn sustain(&mut self, owner: &Owner, down: bool, out: &mut Vec<Out>) {
        self.undo = None;
        if self.admits(owner) {
            self.set_sustain(owner, down, out);
        }
    }

    fn set_sustain(&mut self, owner: &Owner, down: bool, out: &mut Vec<Out>) {
        if down {
            if !self.pedals.contains(owner) {
                self.pedals.push(owner.clone());
            }
            return;
        }
        self.pedals.retain(|o| o != owner);
        let notes: Vec<u8> = self.sustained.iter().map(|(n, _)| *n).collect();
        for note in notes {
            if remove(&mut self.sustained, note, owner) {
                self.release_if_unheld(note, out);
            }
        }
    }

    /// `releaseSource`: CC123 (`disconnected` false) lets go of this owner's keys and respects its
    /// pedal; an unplug also lifts the pedal and forgets the owner's wheels.
    pub fn release_owner(&mut self, owner: &Owner, disconnected: bool, out: &mut Vec<Out>) {
        self.undo = None;
        if !self.admits(owner) {
            return;
        }
        self.release(owner, disconnected, out);
        if disconnected {
            self.apply_wheels(out);
        }
        self.publish();
    }

    fn release(&mut self, owner: &Owner, disconnected: bool, out: &mut Vec<Out>) {
        if disconnected {
            self.set_sustain(owner, false, out);
        }
        let notes: Vec<u8> = self.held.iter().filter(|(_, o)| o.contains(owner)).map(|(n, _)| *n).collect();
        for note in notes {
            self.let_go_of(owner, note, out);
        }
        if disconnected {
            self.bends.retain(|(o, _)| o != owner);
            self.modulation.retain(|(o, _)| o != owner);
        }
    }

    /// A port went away (`midi.ts` `attachInputs`): every channel of it, as unplugged.
    pub fn release_conn(&mut self, conn: u32, out: &mut Vec<Out>) {
        self.undo = None;
        for channel in 0..16 {
            self.release(&Owner::Midi { conn, channel }, true, out);
        }
        self.apply_wheels(out);
        self.publish();
    }

    /// `setPitchBend`: the last moved wheel wins across owners.
    pub fn pitch_bend(&mut self, owner: &Owner, semitones: f64, out: &mut Vec<Out>) {
        self.undo = None;
        if semitones.is_finite() && self.admits(owner) {
            set_last(&mut self.bends, owner, semitones);
            self.apply_wheels(out);
        }
    }

    pub fn modulation(&mut self, owner: &Owner, depth: f64, out: &mut Vec<Out>) {
        self.undo = None;
        if depth.is_finite() && self.admits(owner) {
            set_last(&mut self.modulation, owner, depth);
            self.apply_wheels(out);
        }
    }

    /// `releaseController` (`midi.ts`): MIDI learn took `controller` on this owner over, so let go of
    /// what it last set there: 64 lifts the pedal, 1 drops the mod wheel (`dropModulation`), so the
    /// wheel moved before it applies again, as after an unplug.
    pub fn release_controller(&mut self, owner: &Owner, controller: u8, out: &mut Vec<Out>) {
        self.undo = None;
        match controller {
            64 => self.set_sustain(owner, false, out),
            1 => {
                let before = self.modulation.len();
                self.modulation.retain(|(o, _)| o != owner);
                if self.modulation.len() != before {
                    self.apply_wheels(out);
                }
            }
            _ => {}
        }
    }

    /// `applyControllers`, sending only a value that changed: the engine keeps its wheels and hands
    /// them to the next instrument itself.
    fn apply_wheels(&mut self, out: &mut Vec<Out>) {
        let bend = self.bends.last().map_or(0.0, |(_, v)| *v);
        let depth = self.modulation.last().map_or(0.0, |(_, v)| *v);
        if bend != self.pitch_bend {
            self.pitch_bend = bend;
            out.push(Out::new(Command::PitchBend(bend)));
        }
        if depth != self.mod_depth {
            self.mod_depth = depth;
            out.push(Out::new(Command::Modulation(depth)));
        }
    }

    /// Move the notes to `target` (`routeEngine`): release every note held or sustained (`allNotesOff`),
    /// then `SelectInstrument`. The held notes are forgotten; the pedals and the wheels stay.
    pub fn select_target(&mut self, target: NoteTarget, out: &mut Vec<Out>) {
        self.undo = None;
        self.release_all(out);
        out.push(Out::new(Command::SelectInstrument(target)));
        self.target = Some(target);
        self.publish();
    }

    /// `allNotesOff`: a `NoteOff` for every note that sounds, held ones first, and forget them.
    pub(crate) fn release_all(&mut self, out: &mut Vec<Out>) {
        let held = self.held.iter().map(|(n, _)| *n);
        let sustained = self.sustained.iter().map(|(n, _)| *n).filter(|n| !has(&self.held, *n));
        for note in held.chain(sustained).filter(|n| self.sounds(*n)) {
            out.push(Out::new(Command::NoteOff(note)));
        }
        self.held.clear();
        self.sustained.clear();
        self.sounding = 0;
    }

    /// The note target last selected.
    pub fn target(&self) -> Option<NoteTarget> {
        self.target
    }

    /// Every UI owner the router knows (holding a note, a pedal or a wheel) whose epoch `pick` takes,
    /// each once.
    fn ui_owners(&self, pick: impl Fn(u64) -> bool) -> Vec<Owner> {
        let held = self.held.iter().flat_map(|(_, o)| o.iter());
        let wheels = self.bends.iter().chain(&self.modulation).map(|(o, _)| o);
        let mut owners: Vec<Owner> = Vec::new();
        for owner in held.chain(&self.pedals).chain(wheels) {
            if matches!(owner, Owner::Ui { epoch, .. } if pick(*epoch)) && !owners.contains(owner) {
                owners.push(owner.clone());
            }
        }
        owners
    }

    /// A new WebView document (`host_init`'s `frontendEpoch`): the older documents' owners let go, as
    /// unplugged, and their later events are refused. An epoch not newer than the current does nothing.
    pub fn ui_epoch(&mut self, epoch: u64, out: &mut Vec<Out>) {
        self.undo = None;
        if epoch <= self.ui_epoch {
            return;
        }
        self.ui_epoch = epoch;
        for owner in self.ui_owners(|e| e < epoch) {
            self.release(&owner, true, out);
        }
        self.apply_wheels(out);
        self.publish();
    }

    /// The window lost focus: document `epoch`'s keys and pointers are up. A replaced document's
    /// were released with it.
    pub fn ui_blur(&mut self, epoch: u64, out: &mut Vec<Out>) {
        self.undo = None;
        if epoch != self.ui_epoch {
            return;
        }
        for owner in self.ui_owners(|e| e == epoch) {
            self.release(&owner, true, out);
        }
        self.apply_wheels(out);
        self.publish();
    }

    /// The engine was rebuilt (`owner.rs` `swap_engine`): it sounds nothing. The owners and pedals stay,
    /// so a later release is harmless; the deferred releases go with the voices they deferred. The
    /// target and the wheels stay as sent: the settings replay hands them over, once.
    pub fn engine_rebuilt(&mut self) {
        self.undo = None;
        self.sounding = 0;
        self.sustained.clear();
    }

    /// The physically held notes, for the on-screen keyboard (clone it to read from another thread).
    pub fn held(&self) -> &HeldNotes {
        &self.published
    }

    fn publish(&self) {
        self.published.publish(self.held.iter().fold(0, |set, (n, _)| set | 1 << n));
    }
}

/// The physically held notes (not the sustained ones) as a 128-bit set, published by the router and
/// read without its lock: a seqlock over atomics, as `FrameClock`. Cheap to clone; one writer.
#[derive(Clone, Default)]
pub struct HeldNotes(Arc<HeldCell>);

#[derive(Default)]
struct HeldCell {
    /// Odd while a write is in progress; twice the number of changes.
    seq: AtomicU64,
    low: AtomicU64,
    high: AtomicU64,
}

/// A read of [`HeldNotes`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Held {
    /// Bit n for note n.
    pub notes: u128,
    /// How many times the set has changed: equal counts, equal sets.
    pub changes: u64,
}

impl Held {
    pub fn contains(&self, note: u8) -> bool {
        note < 128 && self.notes >> note & 1 == 1
    }
}

impl HeldNotes {
    pub fn read(&self) -> Held {
        let c = &self.0;
        loop {
            let before = c.seq.load(Acquire);
            if before % 2 == 0 {
                let (low, high) = (c.low.load(Relaxed), c.high.load(Relaxed));
                fence(Acquire);
                if c.seq.load(Relaxed) == before {
                    return Held { notes: u128::from(high) << 64 | u128::from(low), changes: before / 2 };
                }
            }
            // The writer is between its two stores.
            std::thread::yield_now();
        }
    }

    /// The router's, under its lock: a change only.
    fn publish(&self, notes: u128) {
        let c = &self.0;
        let (low, high) = (notes as u64, (notes >> 64) as u64);
        if c.low.load(Relaxed) == low && c.high.load(Relaxed) == high {
            return;
        }
        let seq = c.seq.load(Relaxed);
        c.seq.store(seq.wrapping_add(1), Relaxed);
        fence(Release);
        c.low.store(low, Relaxed);
        c.high.store(high, Relaxed);
        c.seq.store(seq.wrapping_add(2), Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_engine::Instrument;

    fn midi(conn: u32, channel: u8) -> Owner {
        Owner::Midi { conn, channel }
    }

    fn ui(epoch: u64, id: &str) -> Owner {
        Owner::Ui { epoch, id: id.into() }
    }

    fn a0() -> Owner {
        midi(0, 0)
    }

    fn a1() -> Owner {
        midi(0, 1)
    }

    fn b0() -> Owner {
        midi(1, 0)
    }

    fn on(note: u8) -> Command {
        Command::NoteOn(note, 100.0 / 127.0)
    }

    /// Runs `f` and returns what it sent.
    fn sent(r: &mut Router, f: impl FnOnce(&mut Router, &mut Vec<Out>)) -> Vec<Command> {
        let mut out = Vec::new();
        f(r, &mut out);
        out.into_iter().map(|o| o.command).collect()
    }

    fn msg(r: &mut Router, owner: &Owner, bytes: [u8; 3]) -> Vec<Command> {
        let m = super::super::parse::parse(&bytes).unwrap();
        sent(r, |r, out| r.message(owner, m, out))
    }

    fn held(r: &Router) -> Vec<u8> {
        let held = r.held().read();
        (0..128).filter(|n| held.contains(*n)).collect()
    }

    // probe midi-note-ownership: "one MIDI port must not release another port's held note" and "one MIDI
    // channel must not release another channel's held note" (input-router.ts handle, heldBySource).
    #[test]
    fn a_note_held_on_two_ports_or_channels_sounds_until_the_last_lets_go() {
        let mut r = Router::default();
        assert_eq!(msg(&mut r, &a0(), [0x90, 60, 100]), [on(60)]);
        assert_eq!(msg(&mut r, &b0(), [0x90, 60, 100]), [], "a second owner does not strike again");
        assert_eq!(msg(&mut r, &a0(), [0x80, 60, 0]), []);
        assert_eq!(held(&r), [60]);
        assert_eq!(msg(&mut r, &b0(), [0x80, 60, 0]), [Command::NoteOff(60)]);

        assert_eq!(msg(&mut r, &a0(), [0x90, 60, 100]), [on(60)]);
        assert_eq!(msg(&mut r, &a1(), [0x91, 60, 100]), []);
        assert_eq!(msg(&mut r, &a0(), [0x80, 60, 0]), []);
        assert_eq!(held(&r), [60]);
    }

    // probe keyboard: two pointers, or a key and a pointer, on one note (input-router.ts handle: owners
    // are physical, `pointer:<id>` and `key:<code>`); a MIDI owner on the same note likewise.
    #[test]
    fn ui_and_midi_owners_share_a_note_until_the_last_lets_go() {
        let mut r = Router::default();
        sent(&mut r, |r, out| r.ui_epoch(3, out));
        let (p1, p2, key) = (ui(3, "pointer:1"), ui(3, "pointer:2"), ui(3, "key:KeyA"));
        assert_eq!(sent(&mut r, |r, out| r.note_on(&p1, 60, 100, out)), [on(60)]);
        assert_eq!(sent(&mut r, |r, out| r.note_on(&p2, 60, 100, out)), []);
        assert_eq!(sent(&mut r, |r, out| r.note_on(&key, 60, 100, out)), []);
        assert_eq!(sent(&mut r, |r, out| r.note_on(&a0(), 60, 100, out)), []);
        for owner in [&p1, &key, &a0()] {
            assert_eq!(sent(&mut r, |r, out| r.note_off(owner, 60, out)), []);
        }
        assert_eq!(sent(&mut r, |r, out| r.note_off(&p2, 60, out)), [Command::NoteOff(60)]);
    }

    // input-router.ts handle: a note-off from an owner that does not hold the note does nothing.
    #[test]
    fn a_note_off_from_an_owner_not_holding_the_note_does_nothing() {
        let mut r = Router::default();
        assert_eq!(msg(&mut r, &a0(), [0x80, 60, 0]), []);
        msg(&mut r, &a0(), [0x90, 60, 100]);
        assert_eq!(msg(&mut r, &b0(), [0x80, 60, 0]), []);
        assert_eq!(held(&r), [60]);
    }

    // input-router.ts handle: velocity is clampMidi(velocity) / 127, the CLAP-normalised 0..1 form;
    // midi.ts: velocity 0 is a note-off.
    #[test]
    fn velocity_reaches_the_engine_as_zero_to_one_and_zero_is_a_note_off() {
        let mut r = Router::default();
        assert_eq!(msg(&mut r, &a0(), [0x90, 60, 127]), [Command::NoteOn(60, 1.0)]);
        assert_eq!(msg(&mut r, &a0(), [0x90, 61, 1]), [Command::NoteOn(61, 1.0 / 127.0)]);
        assert_eq!(sent(&mut r, |r, out| r.note_on(&a0(), 61, 0, out)), [Command::NoteOff(61)]);
        assert_eq!(sent(&mut r, |r, out| r.note_on(&a0(), 62, 200, out)), [Command::NoteOn(62, 1.0)]);
    }

    // probe midi-note-ownership: "one port pedal must not sustain another port", "unrelated pedal-up must
    // preserve a sustained note", "own pedal-up must release the note" (input-router.ts setSustain).
    #[test]
    fn a_pedal_sustains_only_its_own_port_and_channel() {
        let mut r = Router::default();
        msg(&mut r, &a0(), [0xb0, 64, 127]);
        msg(&mut r, &b0(), [0x90, 67, 100]);
        assert_eq!(msg(&mut r, &b0(), [0x80, 67, 0]), [Command::NoteOff(67)], "port a's pedal must not hold port b");
        msg(&mut r, &a0(), [0x90, 67, 100]);
        assert_eq!(msg(&mut r, &a0(), [0x80, 67, 0]), [], "port a's own pedal defers it");
        assert_eq!(held(&r), [] as [u8; 0], "sustained is not held");
        assert_eq!(msg(&mut r, &b0(), [0xb0, 64, 0]), [], "port b's pedal-up leaves it sustained");
        assert_eq!(msg(&mut r, &a0(), [0xb0, 64, 0]), [Command::NoteOff(67)]);
        assert_eq!(msg(&mut r, &a0(), [0xb0, 64, 0]), [], "released exactly once");
    }

    // midi.ts parseMidiMessage: "value >= 64 = down".
    #[test]
    fn the_pedal_is_down_from_value_64() {
        let mut r = Router::default();
        msg(&mut r, &a0(), [0xb0, 64, 64]);
        msg(&mut r, &a0(), [0x90, 60, 100]);
        assert_eq!(msg(&mut r, &a0(), [0x80, 60, 0]), []);
        assert_eq!(msg(&mut r, &a0(), [0xb0, 64, 63]), [Command::NoteOff(60)]);
    }

    // probe instrument-routing (D12-S): "a sustained re-strike sends noteOff before noteOn" and "the
    // re-struck note releases on pedal up" (input-router.ts handle, the firstHold branch).
    #[test]
    fn a_restrike_under_the_pedal_ends_the_sustained_voice_first_and_pedal_up_releases_once() {
        let mut r = Router::default();
        let mut all = Vec::new();
        for bytes in [[0xb0, 64, 127], [0x90, 62, 100], [0x80, 62, 0], [0x90, 62, 90]] {
            all.extend(msg(&mut r, &a0(), bytes));
        }
        assert_eq!(all, [on(62), Command::NoteOff(62), Command::NoteOn(62, 90.0 / 127.0)]);
        assert_eq!(msg(&mut r, &a0(), [0x80, 62, 0]), []);
        assert_eq!(msg(&mut r, &a0(), [0xb0, 64, 0]), [Command::NoteOff(62)]);
    }

    // probe midi-note-ownership: "CC123 must leave the other owner held", "CC123 must release its own
    // keys", "CC123 honors the physical pedal", "pedal-up completes a deferred CC123 release"
    // (input-router.ts releaseSource).
    #[test]
    fn all_notes_off_releases_only_its_owner_and_respects_its_pedal() {
        let mut r = Router::default();
        msg(&mut r, &a0(), [0x90, 60, 100]);
        msg(&mut r, &b0(), [0x90, 60, 100]);
        assert_eq!(msg(&mut r, &a0(), [0xb0, 123, 0]), []);
        assert_eq!(held(&r), [60]);
        assert_eq!(msg(&mut r, &b0(), [0xb0, 123, 0]), [Command::NoteOff(60)]);
        assert!(held(&r).is_empty());

        msg(&mut r, &a0(), [0xb0, 64, 127]);
        msg(&mut r, &a0(), [0x90, 67, 100]);
        assert_eq!(msg(&mut r, &a0(), [0xb0, 123, 0]), [], "the pedal holds it");
        assert!(held(&r).is_empty());
        assert_eq!(msg(&mut r, &a0(), [0xb0, 64, 0]), [Command::NoteOff(67)]);
    }

    // probe midi-note-ownership: "unplugging one port must preserve the other port"; midi.ts
    // attachInputs releases all 16 channels of a vanished port with releaseSource(owner, true), which
    // lifts its pedal too.
    #[test]
    fn a_port_that_goes_away_releases_its_notes_and_its_pedal_and_nothing_else() {
        let mut r = Router::default();
        msg(&mut r, &a0(), [0xb0, 64, 127]);
        msg(&mut r, &a0(), [0x90, 64, 100]);
        msg(&mut r, &a0(), [0x80, 64, 0]);
        msg(&mut r, &midi(0, 9), [0x99, 36, 100]);
        msg(&mut r, &b0(), [0x90, 60, 100]);
        let released = sent(&mut r, |r, out| r.release_conn(0, out));
        assert_eq!(released, [Command::NoteOff(64), Command::NoteOff(36)]);
        assert_eq!(held(&r), [60]);
        // The pedal is gone with its port: a note on the port's next connection is not sustained.
        msg(&mut r, &a0(), [0x90, 65, 100]);
        assert_eq!(msg(&mut r, &a0(), [0x80, 65, 0]), [Command::NoteOff(65)]);
    }

    // midi.ts parseMidiMessage: pitch bend scales to ±PITCH_BEND_RANGE_SEMITONES around 8192; CC1 is
    // value / 127.
    #[test]
    fn the_wheels_scale_as_the_web_does() {
        let mut r = Router::default();
        assert_eq!(msg(&mut r, &a0(), [0xe0, 0x7f, 0x7f]), [Command::PitchBend(8191.0 / 8192.0 * 2.0)]);
        assert_eq!(msg(&mut r, &a0(), [0xe0, 0x00, 0x00]), [Command::PitchBend(-2.0)]);
        assert_eq!(msg(&mut r, &a0(), [0xe0, 0x00, 0x40]), [Command::PitchBend(0.0)]);
        assert_eq!(msg(&mut r, &a0(), [0xb0, 1, 64]), [Command::Modulation(64.0 / 127.0)]);
    }

    // input-router.ts setPitchBend/setModulation and applyControllers: the last moved wheel wins across
    // owners; unplugging it restores the surviving owner's setting (releaseSource with disconnected).
    // Plan § Parity: unchanged values are suppressed.
    #[test]
    fn the_last_moved_wheel_wins_an_unplug_hands_back_the_survivor_and_repeats_are_not_sent() {
        let mut r = Router::default();
        msg(&mut r, &b0(), [0xb0, 1, 50]);
        assert_eq!(msg(&mut r, &a0(), [0xb0, 1, 100]), [Command::Modulation(100.0 / 127.0)]);
        assert_eq!(msg(&mut r, &a0(), [0xb0, 1, 100]), [], "unchanged");
        assert_eq!(msg(&mut r, &b0(), [0xb0, 1, 100]), [], "another owner, the same value");
        msg(&mut r, &a0(), [0xe0, 0x00, 0x60]);
        assert_eq!(sent(&mut r, |r, out| r.release_conn(0, out)), [Command::PitchBend(0.0)], "b moved last");
        assert_eq!(msg(&mut r, &a0(), [0xb0, 1, 20]), [Command::Modulation(20.0 / 127.0)]);
        assert_eq!(sent(&mut r, |r, out| r.release_owner(&a0(), true, out)), [Command::Modulation(100.0 / 127.0)]);
    }

    // probe midi-learn "learning CC1 hands the vibrato back to the wheel moved before it"
    // (midi.ts releaseController → input-router.ts dropModulation); CC64 lets the pedal go.
    #[test]
    fn releasing_a_learned_controller_lets_go_of_what_it_set() {
        let mut r = Router::default();
        msg(&mut r, &b0(), [0xb0, 1, 50]);
        msg(&mut r, &midi(0, 5), [0xb5, 1, 100]);
        assert_eq!(sent(&mut r, |r, out| r.release_controller(&midi(0, 5), 1, out)), [Command::Modulation(50.0 / 127.0)]);

        msg(&mut r, &b0(), [0xb0, 64, 127]);
        msg(&mut r, &b0(), [0x90, 60, 100]);
        msg(&mut r, &b0(), [0x80, 60, 0]);
        assert_eq!(sent(&mut r, |r, out| r.release_controller(&b0(), 64, out)), [Command::NoteOff(60)]);
    }

    // probe instrument-routing: "switching slots releases the sustained note exactly once"
    // (input-router.ts allNotesOff, then instrument.ts routeEngine's SelectInstrument): the held and
    // sustained notes are released before the switch and forgotten; the pedal stays down. The engine
    // hands the new instrument the wheels (instruments.rs `select`), so the switch sends none.
    #[test]
    fn a_target_switch_releases_held_and_sustained_notes_first_and_keeps_the_pedal_and_wheels() {
        let mut r = Router::default();
        let pad = NoteTarget::Builtin(Instrument::Pad);
        msg(&mut r, &a0(), [0xe0, 0x00, 0x60]);
        msg(&mut r, &a0(), [0xb0, 64, 127]);
        msg(&mut r, &a0(), [0x90, 64, 100]);
        msg(&mut r, &a0(), [0x80, 64, 0]);
        msg(&mut r, &a0(), [0x90, 60, 100]);
        msg(&mut r, &b0(), [0x90, 61, 100]);
        assert_eq!(
            sent(&mut r, |r, out| r.select_target(pad, out)),
            [Command::NoteOff(60), Command::NoteOff(61), Command::NoteOff(64), Command::SelectInstrument(pad)]
        );
        assert_eq!(r.target(), Some(pad));
        assert!(held(&r).is_empty());
        assert_eq!(msg(&mut r, &a0(), [0xb0, 64, 0]), []);
        assert_eq!(msg(&mut r, &a0(), [0x80, 60, 0]), [], "a key down across the switch releases nothing");
        assert_eq!(msg(&mut r, &b0(), [0x90, 61, 100]), [on(61)], "a note held across the switch strikes again");
        msg(&mut r, &a0(), [0xb0, 64, 127]);
        assert_eq!(sent(&mut r, |r, out| r.select_target(NoteTarget::Off, out)), [Command::NoteOff(61), Command::SelectInstrument(NoteTarget::Off)]);
        msg(&mut r, &a0(), [0x90, 63, 100]);
        assert_eq!(msg(&mut r, &a0(), [0x80, 63, 0]), [], "the pedal survived the switch");
        assert_eq!(msg(&mut r, &a0(), [0xe0, 0x00, 0x60]), [], "and the wheel");
    }

    // Plan decision 5: "a refused attack records no owner": undone, a fresh strike or a re-strike under
    // the pedal leaves the router as it was.
    #[test]
    fn a_refused_attack_records_no_owner() {
        let mut r = Router::default();
        sent(&mut r, |r, out| r.note_on(&a0(), 60, 100, out));
        r.attack_refused();
        assert!(held(&r).is_empty());
        assert_eq!(msg(&mut r, &a0(), [0x80, 60, 0]), [], "its release ends nothing");
        assert_eq!(msg(&mut r, &a0(), [0x90, 60, 100]), [on(60)], "the next strike is a first strike");

        msg(&mut r, &a0(), [0xb0, 64, 127]);
        msg(&mut r, &a0(), [0x80, 60, 0]);
        assert_eq!(msg(&mut r, &b0(), [0x90, 60, 90]), [Command::NoteOff(60), Command::NoteOn(60, 90.0 / 127.0)]);
        r.attack_refused();
        assert!(held(&r).is_empty());
        assert_eq!(msg(&mut r, &b0(), [0x80, 60, 0]), [], "b holds nothing");
        assert_eq!(msg(&mut r, &a0(), [0xb0, 64, 0]), [Command::NoteOff(60)], "the pedal's voice still sounds");

        // A refusal reported after another call undoes nothing.
        msg(&mut r, &a0(), [0x90, 62, 100]);
        msg(&mut r, &a0(), [0x90, 63, 100]);
        msg(&mut r, &a0(), [0x80, 63, 0]);
        r.attack_refused();
        assert_eq!(held(&r), [62]);
    }

    // Plan decision 6: a rebuilt engine sounds nothing; owners stay (a release is harmless), a fresh
    // press attacks again, and neither the target nor the wheels are sent again.
    #[test]
    fn after_a_rebuild_a_release_is_harmless_a_press_attacks_again_and_nothing_is_resent() {
        let mut r = Router::default();
        sent(&mut r, |r, out| r.select_target(NoteTarget::Builtin(Instrument::Lead), out));
        msg(&mut r, &a0(), [0xe0, 0x00, 0x60]);
        msg(&mut r, &a0(), [0x90, 60, 100]);
        msg(&mut r, &a0(), [0xb0, 64, 127]);
        msg(&mut r, &a0(), [0x90, 62, 100]);
        msg(&mut r, &a0(), [0x80, 62, 0]);
        r.engine_rebuilt();
        assert_eq!(held(&r), [60], "the owners stay");
        assert_eq!(msg(&mut r, &b0(), [0x90, 60, 100]), [on(60)], "another owner's press attacks again");
        assert_eq!(msg(&mut r, &a0(), [0x80, 60, 0]), [], "b holds it");
        assert_eq!(msg(&mut r, &a0(), [0xb0, 64, 0]), [], "the deferred 62 went with the old engine");
        assert_eq!(msg(&mut r, &a0(), [0xe0, 0x00, 0x60]), [], "the wheel is replayed by the settings, not sent");
        assert_eq!(msg(&mut r, &b0(), [0x80, 60, 0]), [Command::NoteOff(60)]);
    }

    // Plan decision 7: a UI hold is released when its document is replaced and on blur; a replaced
    // document's event is refused; MIDI holds are untouched.
    #[test]
    fn a_document_s_holds_end_with_its_epoch_and_on_blur() {
        let mut r = Router::default();
        assert_eq!(sent(&mut r, |r, out| r.note_on(&ui(1, "pointer:1"), 60, 100, out)), [], "no document yet");
        sent(&mut r, |r, out| r.ui_epoch(1, out));
        let (old, key) = (ui(1, "pointer:1"), ui(1, "key:KeyZ"));
        sent(&mut r, |r, out| r.note_on(&old, 60, 100, out));
        sent(&mut r, |r, out| r.note_on(&key, 61, 100, out));
        sent(&mut r, |r, out| r.note_on(&a0(), 61, 100, out));
        assert_eq!(sent(&mut r, |r, out| r.ui_epoch(2, out)), [Command::NoteOff(60)]);
        assert_eq!(sent(&mut r, |r, out| r.ui_epoch(1, out)), [], "an older epoch changes nothing");
        assert_eq!(sent(&mut r, |r, out| r.note_on(&old, 64, 100, out)), [], "refused");
        assert_eq!(sent(&mut r, |r, out| r.note_off(&key, 61, out)), [], "refused: the port still holds 61");
        assert_eq!(held(&r), [61]);
        let new = ui(2, "pointer:7");
        sent(&mut r, |r, out| r.note_on(&new, 65, 100, out));
        assert_eq!(sent(&mut r, |r, out| r.ui_blur(2, out)), [Command::NoteOff(65)]);
        assert_eq!(held(&r), [61]);
        assert_eq!(sent(&mut r, |r, out| r.note_on(&new, 65, 100, out)), [on(65)], "the document plays on after a blur");
    }

    // Plan decision 1: the held set is published without the router's lock, physical holds only, with
    // a change counter that moves only when the set does.
    #[test]
    fn the_held_set_is_published_on_change_and_read_from_another_thread() {
        let mut r = Router::default();
        let reader = r.held().clone();
        assert_eq!(reader.read(), Held::default());
        msg(&mut r, &a0(), [0xb0, 64, 127]);
        msg(&mut r, &a0(), [0x90, 0, 100]);
        msg(&mut r, &b0(), [0x90, 127, 100]);
        msg(&mut r, &b0(), [0x90, 0, 100]);
        let read = std::thread::spawn(move || reader.read()).join().unwrap();
        assert_eq!(read, Held { notes: 1 | 1 << 127, changes: 2 });
        msg(&mut r, &a0(), [0x80, 0, 0]);
        msg(&mut r, &b0(), [0x80, 0, 0]);
        assert_eq!(r.held().read(), Held { notes: 1 << 127, changes: 3 }, "sustained, not held");
    }

    // midi.ts parseMidiMessage: other controllers reach nothing.
    #[test]
    fn other_controllers_do_nothing() {
        let mut r = Router::default();
        assert_eq!(msg(&mut r, &a0(), [0xb0, 7, 100]), []);
        assert_eq!(msg(&mut r, &a0(), [0xb0, 120, 0]), []);
    }
}
