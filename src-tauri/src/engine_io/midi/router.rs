//! OWNS: what the sender keeps for the engine's note commands, ported from `src/audio/input-router.ts`
//! and the play-path branch of `src/audio/midi.ts`: which owner (a port's channel) holds each note,
//! each owner's sustain pedal and the notes it defers, and the wheels with the last moved one winning.
//! The engine plays what it is told (`lf_engine::Command::NoteOn`); the router decides when to tell it.
//!
//! Collections keep insertion order, as the TS `Map`s and `Set`s do, so a release sweeps its notes in
//! the order the web sent their note-offs. Only MIDI owners exist here: the web's debug `'global'` pedal
//! and the on-screen and computer keys (other `NoteEvent` sources) are not routed through it.

use lf_engine::Command;

use super::parse::Message;

/// A note's owner: a connection (one open port) and a channel (`midi.ts` `midiOwner(port, channel)`).
pub(crate) type Owner = (u32, u8);

/// `midi.ts` `PITCH_BEND_RANGE_SEMITONES`: the wheel's full throw is ±2 semitones.
const PITCH_BEND_RANGE_SEMITONES: f64 = 2.0;

#[derive(Default)]
pub(crate) struct Router {
    /// Note → the owners holding it down (`heldBySource`).
    held: Vec<(u8, Vec<Owner>)>,
    /// Owners whose pedal is down.
    pedals: Vec<Owner>,
    /// Note → the pedals deferring its release (`sustained`).
    sustained: Vec<(u8, Vec<Owner>)>,
    /// Each owner's wheel, last moved last.
    bends: Vec<(Owner, f64)>,
    modulation: Vec<(Owner, f64)>,
    /// What the engine was last told.
    pitch_bend: f64,
    mod_depth: f64,
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
fn remove(list: &mut Vec<(u8, Vec<Owner>)>, note: u8, owner: Owner) -> bool {
    let Some(i) = list.iter().position(|(n, _)| *n == note) else { return false };
    let Some(k) = list[i].1.iter().position(|o| *o == owner) else { return false };
    list[i].1.remove(k);
    if list[i].1.is_empty() {
        list.remove(i);
    }
    true
}

/// Move `owner`'s value to the end (`Map.delete` then `Map.set`).
fn set_last(list: &mut Vec<(Owner, f64)>, owner: Owner, value: f64) {
    list.retain(|(o, _)| *o != owner);
    list.push((owner, value));
}

impl Router {
    /// The play-path branch of `midi.ts` `parseMidiMessage`, for a message MIDI learn did not consume.
    pub(crate) fn message(&mut self, owner: Owner, message: Message, out: &mut impl FnMut(Command)) {
        match message {
            Message::NoteOn { note, velocity, .. } => self.note_on(owner, note, velocity, out),
            Message::NoteOff { note, .. } => self.note_off(owner, note, out),
            Message::Cc { controller: 64, value, .. } => self.set_sustain(value >= 64, owner, out),
            Message::Cc { controller: 1, value, .. } => self.set_modulation(f64::from(value) / 127.0, owner, out),
            Message::Cc { controller: 123, .. } => self.release_source(owner, false, out),
            Message::Cc { .. } => {}
            Message::PitchBend { value, .. } => {
                let raw = f64::from(value) - 8192.0;
                self.set_pitch_bend(raw / 8192.0 * PITCH_BEND_RANGE_SEMITONES, owner, out);
            }
        }
    }

    /// `handle({ type: 'on' })`: the first owner to hold a note sounds it (velocity `clampMidi(v) / 127`);
    /// a re-strike under the pedal ends the sustained voice first.
    pub(crate) fn note_on(&mut self, owner: Owner, note: u8, velocity: u8, out: &mut impl FnMut(Command)) {
        if note > 127 {
            return;
        }
        let sources = entry(&mut self.held, note);
        let first_hold = sources.is_empty();
        if !sources.contains(&owner) {
            sources.push(owner);
        }
        if first_hold {
            if has(&self.sustained, note) {
                out(Command::NoteOff(note));
            }
            out(Command::NoteOn(note, f32::from(velocity.min(127)) / 127.0));
        }
    }

    /// `handle({ type: 'off' })`: only the owner that holds the note lets it go; while its pedal is down
    /// the release waits for the pedal.
    pub(crate) fn note_off(&mut self, owner: Owner, note: u8, out: &mut impl FnMut(Command)) {
        if note > 127 || !remove(&mut self.held, note, owner) {
            return;
        }
        if self.pedals.contains(&owner) {
            let owners = entry(&mut self.sustained, note);
            if !owners.contains(&owner) {
                owners.push(owner);
            }
        }
        self.release_if_unheld(note, out);
    }

    fn release_if_unheld(&mut self, note: u8, out: &mut impl FnMut(Command)) {
        if !has(&self.held, note) && !has(&self.sustained, note) {
            out(Command::NoteOff(note));
        }
    }

    /// `setSustain`: CC64 is scoped to its owner; pedal-up releases what it deferred and nobody holds.
    pub(crate) fn set_sustain(&mut self, on: bool, owner: Owner, out: &mut impl FnMut(Command)) {
        if on {
            if !self.pedals.contains(&owner) {
                self.pedals.push(owner);
            }
            return;
        }
        self.pedals.retain(|o| *o != owner);
        let notes: Vec<u8> = self.sustained.iter().map(|(n, _)| *n).collect();
        for note in notes {
            if remove(&mut self.sustained, note, owner) {
                self.release_if_unheld(note, out);
            }
        }
    }

    /// `releaseSource`: CC123 lets go of this owner's keys and respects its pedal; a disconnect also
    /// lifts the pedal and forgets the owner's wheels.
    pub(crate) fn release_source(&mut self, owner: Owner, disconnected: bool, out: &mut impl FnMut(Command)) {
        if disconnected {
            self.set_sustain(false, owner, out);
        }
        let notes: Vec<u8> = self.held.iter().filter(|(_, o)| o.contains(&owner)).map(|(n, _)| *n).collect();
        for note in notes {
            self.note_off(owner, note, out);
        }
        if disconnected {
            self.bends.retain(|(o, _)| *o != owner);
            self.modulation.retain(|(o, _)| *o != owner);
            self.apply_controllers(out);
        }
    }

    /// A port went away (`midi.ts` `attachInputs`): every channel of it, as a disconnect.
    pub(crate) fn release_port(&mut self, conn: u32, out: &mut impl FnMut(Command)) {
        for channel in 0..16 {
            self.release_source((conn, channel), true, out);
        }
    }

    /// `setPitchBend`: the last moved wheel wins across owners.
    pub(crate) fn set_pitch_bend(&mut self, semitones: f64, owner: Owner, out: &mut impl FnMut(Command)) {
        set_last(&mut self.bends, owner, semitones);
        self.apply_controllers(out);
    }

    pub(crate) fn set_modulation(&mut self, depth: f64, owner: Owner, out: &mut impl FnMut(Command)) {
        set_last(&mut self.modulation, owner, depth);
        self.apply_controllers(out);
    }

    /// `dropModulation`: MIDI learn took this owner's CC1 over, so its wheel stops counting, as an
    /// unplug does.
    pub(crate) fn drop_modulation(&mut self, owner: Owner, out: &mut impl FnMut(Command)) {
        let before = self.modulation.len();
        self.modulation.retain(|(o, _)| *o != owner);
        if self.modulation.len() != before {
            self.apply_controllers(out);
        }
    }

    /// `releaseController` (`midi.ts`): MIDI learn took `controller` on this owner over, so let go of
    /// what it last set there.
    pub(crate) fn release_controller(&mut self, owner: Owner, controller: u8, out: &mut impl FnMut(Command)) {
        match controller {
            64 => self.set_sustain(false, owner, out),
            1 => self.drop_modulation(owner, out),
            _ => {}
        }
    }

    /// `applyControllers`. The web set both wheels on the synth every time; the engine keeps its wheels
    /// and hands them to the next instrument itself, so only a value that changed is sent.
    fn apply_controllers(&mut self, out: &mut impl FnMut(Command)) {
        let bend = self.bends.last().map_or(0.0, |(_, v)| *v);
        let depth = self.modulation.last().map_or(0.0, |(_, v)| *v);
        if bend != self.pitch_bend {
            self.pitch_bend = bend;
            out(Command::PitchBend(bend));
        }
        if depth != self.mod_depth {
            self.mod_depth = depth;
            out(Command::Modulation(depth));
        }
    }

    /// `allNotesOff` on a sink swap: the engine releases what the old target held (on
    /// `SelectInstrument` or `AllNotesOff`); here the held and sustained notes are forgotten, the
    /// pedals and wheels stay.
    pub(crate) fn all_notes_off(&mut self) {
        self.held.clear();
        self.sustained.clear();
    }

    /// Notes held down by any owner (`get held`).
    #[cfg(test)]
    pub(crate) fn held(&self) -> Vec<u8> {
        self.held.iter().map(|(n, _)| *n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A0: Owner = (0, 0);
    const A1: Owner = (0, 1);
    const B0: Owner = (1, 0);

    fn on(note: u8) -> Command {
        Command::NoteOn(note, 100.0 / 127.0)
    }

    /// Runs `f` and returns what it sent.
    fn sent(r: &mut Router, f: impl FnOnce(&mut Router, &mut dyn FnMut(Command))) -> Vec<Command> {
        let mut out = Vec::new();
        f(r, &mut |c| out.push(c));
        out
    }

    fn msg(r: &mut Router, owner: Owner, bytes: [u8; 3]) -> Vec<Command> {
        let m = super::super::parse::parse(&bytes).unwrap();
        sent(r, |r, out| r.message(owner, m, &mut |c| out(c)))
    }

    // probe midi-note-ownership: "one MIDI port must not release another port's held note" and "one MIDI
    // channel must not release another channel's held note" (input-router.ts handle, heldBySource).
    #[test]
    fn a_note_held_on_two_ports_or_channels_sounds_until_the_last_lets_go() {
        let mut r = Router::default();
        assert_eq!(msg(&mut r, A0, [0x90, 60, 100]), [on(60)]);
        assert_eq!(msg(&mut r, B0, [0x90, 60, 100]), [], "a second owner does not strike again");
        assert_eq!(msg(&mut r, A0, [0x80, 60, 0]), []);
        assert_eq!(r.held(), [60]);
        assert_eq!(msg(&mut r, B0, [0x80, 60, 0]), [Command::NoteOff(60)]);

        assert_eq!(msg(&mut r, A0, [0x90, 60, 100]), [on(60)]);
        assert_eq!(msg(&mut r, A1, [0x91, 60, 100]), []);
        assert_eq!(msg(&mut r, A0, [0x80, 60, 0]), []);
        assert_eq!(r.held(), [60]);
    }

    // input-router.ts handle: a note-off from an owner that does not hold the note does nothing.
    #[test]
    fn a_note_off_from_an_owner_not_holding_the_note_does_nothing() {
        let mut r = Router::default();
        assert_eq!(msg(&mut r, A0, [0x80, 60, 0]), []);
        msg(&mut r, A0, [0x90, 60, 100]);
        assert_eq!(msg(&mut r, B0, [0x80, 60, 0]), []);
        assert_eq!(r.held(), [60]);
    }

    // input-router.ts handle: velocity is clampMidi(velocity) / 127, the CLAP-normalised 0..1 form.
    #[test]
    fn velocity_reaches_the_engine_as_zero_to_one() {
        let mut r = Router::default();
        assert_eq!(msg(&mut r, A0, [0x90, 60, 127]), [Command::NoteOn(60, 1.0)]);
        assert_eq!(msg(&mut r, A0, [0x90, 61, 1]), [Command::NoteOn(61, 1.0 / 127.0)]);
    }

    // probe midi-note-ownership: "one port pedal must not sustain another port", "unrelated pedal-up must
    // preserve a sustained note", "own pedal-up must release the note" (input-router.ts setSustain).
    #[test]
    fn a_pedal_sustains_only_its_own_port_and_channel() {
        let mut r = Router::default();
        msg(&mut r, A0, [0xb0, 64, 127]);
        msg(&mut r, B0, [0x90, 67, 100]);
        assert_eq!(msg(&mut r, B0, [0x80, 67, 0]), [Command::NoteOff(67)], "port a's pedal must not hold port b");
        msg(&mut r, A0, [0x90, 67, 100]);
        assert_eq!(msg(&mut r, A0, [0x80, 67, 0]), [], "port a's own pedal defers it");
        assert_eq!(msg(&mut r, B0, [0xb0, 64, 0]), [], "port b's pedal-up leaves it sustained");
        assert_eq!(msg(&mut r, A0, [0xb0, 64, 0]), [Command::NoteOff(67)]);
        assert_eq!(msg(&mut r, A0, [0xb0, 64, 0]), [], "released exactly once");
    }

    // midi.ts parseMidiMessage: "value >= 64 = down".
    #[test]
    fn the_pedal_is_down_from_value_64() {
        let mut r = Router::default();
        msg(&mut r, A0, [0xb0, 64, 64]);
        msg(&mut r, A0, [0x90, 60, 100]);
        assert_eq!(msg(&mut r, A0, [0x80, 60, 0]), []);
        assert_eq!(msg(&mut r, A0, [0xb0, 64, 63]), [Command::NoteOff(60)]);
    }

    // probe instrument-routing (D12-S): "a sustained re-strike sends noteOff before noteOn" and "the
    // re-struck note releases on pedal up" (input-router.ts handle, the firstHold branch).
    #[test]
    fn a_restrike_under_the_pedal_ends_the_sustained_voice_first_and_pedal_up_releases_once() {
        let mut r = Router::default();
        let mut all = Vec::new();
        for bytes in [[0xb0, 64, 127], [0x90, 62, 100], [0x80, 62, 0], [0x90, 62, 90]] {
            all.extend(msg(&mut r, A0, bytes));
        }
        assert_eq!(all, [on(62), Command::NoteOff(62), Command::NoteOn(62, 90.0 / 127.0)]);
        assert_eq!(msg(&mut r, A0, [0x80, 62, 0]), []);
        assert_eq!(msg(&mut r, A0, [0xb0, 64, 0]), [Command::NoteOff(62)]);
    }

    // probe midi-note-ownership: "CC123 must leave the other owner held", "CC123 must release its own
    // keys", "CC123 honors the physical pedal", "pedal-up completes a deferred CC123 release"
    // (input-router.ts releaseSource).
    #[test]
    fn all_notes_off_releases_only_its_owner_and_respects_its_pedal() {
        let mut r = Router::default();
        msg(&mut r, A0, [0x90, 60, 100]);
        msg(&mut r, B0, [0x90, 60, 100]);
        assert_eq!(msg(&mut r, A0, [0xb0, 123, 0]), []);
        assert_eq!(r.held(), [60]);
        assert_eq!(msg(&mut r, B0, [0xb0, 123, 0]), [Command::NoteOff(60)]);
        assert!(r.held().is_empty());

        msg(&mut r, A0, [0xb0, 64, 127]);
        msg(&mut r, A0, [0x90, 67, 100]);
        assert_eq!(msg(&mut r, A0, [0xb0, 123, 0]), [], "the pedal holds it");
        assert!(r.held().is_empty());
        assert_eq!(msg(&mut r, A0, [0xb0, 64, 0]), [Command::NoteOff(67)]);
    }

    // probe midi-note-ownership: "unplugging one port must preserve the other port"; midi.ts
    // attachInputs releases all 16 channels of a vanished port with releaseSource(owner, true), which
    // lifts its pedal too.
    #[test]
    fn a_port_that_goes_away_releases_its_notes_and_its_pedal_and_nothing_else() {
        let mut r = Router::default();
        msg(&mut r, A0, [0xb0, 64, 127]);
        msg(&mut r, A0, [0x90, 64, 100]);
        msg(&mut r, A0, [0x80, 64, 0]);
        msg(&mut r, (0, 9), [0x99, 36, 100]);
        msg(&mut r, B0, [0x90, 60, 100]);
        let released = sent(&mut r, |r, out| r.release_port(0, &mut |c| out(c)));
        assert_eq!(released, [Command::NoteOff(64), Command::NoteOff(36)]);
        assert_eq!(r.held(), [60]);
        // The pedal is gone with its port: a note on the port's next connection is not sustained.
        msg(&mut r, A0, [0x90, 65, 100]);
        assert_eq!(msg(&mut r, A0, [0x80, 65, 0]), [Command::NoteOff(65)]);
    }

    // midi.ts parseMidiMessage: pitch bend scales to ±PITCH_BEND_RANGE_SEMITONES around 8192; CC1 is
    // value / 127.
    #[test]
    fn the_wheels_scale_as_the_web_does() {
        let mut r = Router::default();
        assert_eq!(msg(&mut r, A0, [0xe0, 0x7f, 0x7f]), [Command::PitchBend(8191.0 / 8192.0 * 2.0)]);
        assert_eq!(msg(&mut r, A0, [0xe0, 0x00, 0x00]), [Command::PitchBend(-2.0)]);
        assert_eq!(msg(&mut r, A0, [0xe0, 0x00, 0x40]), [Command::PitchBend(0.0)]);
        assert_eq!(msg(&mut r, A0, [0xb0, 1, 64]), [Command::Modulation(64.0 / 127.0)]);
    }

    // input-router.ts setPitchBend/setModulation and applyControllers: the last moved wheel wins across
    // owners; unplugging it restores the surviving owner's setting (releaseSource with disconnected).
    #[test]
    fn the_last_moved_wheel_wins_and_an_unplug_hands_back_the_survivor() {
        let mut r = Router::default();
        msg(&mut r, B0, [0xb0, 1, 50]);
        assert_eq!(msg(&mut r, A0, [0xb0, 1, 100]), [Command::Modulation(100.0 / 127.0)]);
        msg(&mut r, A0, [0xe0, 0x00, 0x60]);
        assert_eq!(sent(&mut r, |r, out| r.release_port(0, &mut |c| out(c))), [Command::PitchBend(0.0), Command::Modulation(50.0 / 127.0)]);
    }

    // probe midi-learn "learning CC1 hands the vibrato back to the wheel moved before it"
    // (midi.ts releaseController → input-router.ts dropModulation); CC64 lets the pedal go.
    #[test]
    fn releasing_a_learned_controller_lets_go_of_what_it_set() {
        let mut r = Router::default();
        msg(&mut r, B0, [0xb0, 1, 50]);
        msg(&mut r, (0, 5), [0xb5, 1, 100]);
        assert_eq!(sent(&mut r, |r, out| r.release_controller((0, 5), 1, &mut |c| out(c))), [Command::Modulation(50.0 / 127.0)]);

        msg(&mut r, B0, [0xb0, 64, 127]);
        msg(&mut r, B0, [0x90, 60, 100]);
        msg(&mut r, B0, [0x80, 60, 0]);
        assert_eq!(sent(&mut r, |r, out| r.release_controller(B0, 64, &mut |c| out(c))), [Command::NoteOff(60)]);
    }

    // probe instrument-routing: "switching slots releases the sustained note exactly once"
    // (input-router.ts allNotesOff on setActivePlugin); the engine releases it on SelectInstrument, so
    // pedal-up sends nothing more, while the pedal itself stays down.
    #[test]
    fn a_target_switch_forgets_held_and_sustained_notes_but_keeps_the_pedal() {
        let mut r = Router::default();
        msg(&mut r, A0, [0xb0, 64, 127]);
        msg(&mut r, A0, [0x90, 64, 100]);
        msg(&mut r, A0, [0x80, 64, 0]);
        msg(&mut r, A0, [0x90, 60, 100]);
        r.all_notes_off();
        assert_eq!(msg(&mut r, A0, [0xb0, 64, 0]), []);
        assert_eq!(msg(&mut r, A0, [0x80, 60, 0]), [], "a key down across the switch releases nothing");
        // Another owner striking a note held across the switch sounds it.
        msg(&mut r, A0, [0xb0, 64, 127]);
        r.all_notes_off();
        assert_eq!(msg(&mut r, B0, [0x90, 62, 100]), [on(62)]);
        msg(&mut r, A0, [0x90, 63, 100]);
        assert_eq!(msg(&mut r, A0, [0x80, 63, 0]), [], "the pedal survived the switch");
    }

    // midi.ts parseMidiMessage: other controllers reach nothing.
    #[test]
    fn other_controllers_do_nothing() {
        let mut r = Router::default();
        assert_eq!(msg(&mut r, A0, [0xb0, 7, 100]), []);
        assert_eq!(msg(&mut r, A0, [0xb0, 120, 0]), []);
    }
}
