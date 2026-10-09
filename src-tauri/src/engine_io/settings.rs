//! OWNS: the last value of every setting, replayed into each new engine (the first, one at another
//! sample rate, one that replaced a faulted engine), so the settings survive a rebuild and a setting sent
//! before the first open is kept; a reset frame hands them to the UI.
//!
//! A setting is a command that sets a value (the tempo, the click, the master, the input sends, the
//! looper's modes and FADE's length, a lane's volume, mute, DUB FEEDBACK, pan and FX, the note target, the
//! wheels, a built-in instrument's level, a plugin slot's live flag and gain, the selected lane);
//! everything else (the looper's gestures, notes) acts once and is not kept. Every setting but a lane's
//! mix is kept as the command the engine's ring took (or the UI sent before the first open); one the
//! full ring refused is not kept (`EngineHost::send`).
//!
//! A lane's mix is a projection of what the engine applied: its last `Event::Mix` the feed drained
//! (`mixed`), from the engine generation it follows (`follow`), kept as the commands that set it where
//! it differs from a fresh lane's. So a COPY, a CLEAR, a pedal's MUTE and a load reach it as the engine
//! applied them, and a command the engine has not applied yet is not in it. Until a lane's first `Mix`
//! (before the first engine, or under one that has not taken all the commands queued ahead of its
//! first block) the commands sent for it are kept
//! instead, as bootstrap: replayed into the next engine, then replaced by its first `Mix` for the lane.
//! A rebuild projects the replaced engine's last mixes before its replay (`owner.rs` `swap_engine`):
//! what its event ring still holds, then each lane's mix read from the engine itself
//! (`Engine::applied_mixes`), so a mix the full ring refused is not lost.
//!
//! A toggled setting (the click, END STOP, FIXED, RETAKE, AUTO REC, each input send: `lf_engine::Toggle`)
//! is switched by an action (`Action::Toggle`), which the engine judges and applies against its own
//! value, so the memory takes its value from what the engine applied. Its setter is kept as sent
//! (initialization, a replay, a script), and the engine answers every applied command of the setting,
//! setter or toggle, with exactly one event of it (`Event::Toggled` with the value it left, or a refused
//! toggle's `Event::Refused`; an answer the full ring refused is owed and comes later, or is read at a
//! rebuild: `Engine::unsent_toggles`). The memory counts the commands of each setting the engine's ring
//! took (`pushed`) against those answers, which arrive in the order the commands applied, and keeps an
//! answer's value (as the setter that sets it) only when no setter pushed after its command is still
//! unanswered (`toggled`): an answer older than a setter in flight never overwrites that setter, and the
//! setter's own answer brings the applied value. A rebuild drains the replaced engine's ring and its owed
//! answers, so a toggle accepted just before it survives whether or not the feed saw it, and a setter
//! still queued there stays kept as sent; a toggle still queued there is lost, as every queued action.

use std::collections::BTreeMap;

use lf_engine::dsp::fx::{FxKind, FxParam, MAX_PARAMS};
use lf_engine::grid::Frame;
use lf_engine::{Action, Command, CompactMix, Event, LaneMix, Refusal, Toggle, SLOT_COUNT, TRACK_COUNT};

/// What a setting sets; replayed in this order (the note target before the wheels it hands over).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Bpm,
    Metronome,
    ClickVolume,
    MasterVolume,
    MasterMute,
    /// An input send's param, by `InputSendParam as usize`; before the sends' on/off, so a send that
    /// comes on in a new engine comes on with its values.
    InputSendParam(usize),
    /// An input send, by `InputSend as usize`.
    InputSend(usize),
    LoopEndStop,
    FadeBars,
    FixedLength,
    FixedBars,
    Retake,
    AutoRecord,
    AutoSensitivity,
    SelectTrack,
    Volume(u8),
    Mute(u8),
    DubFeedback(u8),
    Pan(u8),
    /// A lane's effect, by `FxKind::index`.
    FxBypass(u8, usize),
    /// A lane's FX param, by `FxKind::index * MAX_PARAMS + FxParam::index`.
    FxParam(u8, usize),
    /// A built-in instrument's level, by `Instrument as usize`.
    InstrumentGain(usize),
    Instrument,
    PitchBend,
    Modulation,
    SlotLive(u8),
    SlotGain(u8),
}

impl Key {
    fn lane(self) -> Option<u8> {
        match self {
            Key::Volume(i) | Key::Mute(i) | Key::DubFeedback(i) | Key::Pan(i) | Key::FxBypass(i, _) | Key::FxParam(i, _) => Some(i),
            _ => None,
        }
    }
}

/// The key a setting command sets; `None` for an action (or a lane or slot out of range).
fn key(command: &Command) -> Option<Key> {
    let lane = |i: u8| (usize::from(i) < TRACK_COUNT).then_some(i);
    let slot = |i: u8| (usize::from(i) < SLOT_COUNT).then_some(i);
    Some(match *command {
        Command::SetBpm(_) => Key::Bpm,
        Command::SetMetronome(_) => Key::Metronome,
        Command::SetClickVolume(_) => Key::ClickVolume,
        Command::SetMasterVolume(_) => Key::MasterVolume,
        Command::SetMasterMute(_) => Key::MasterMute,
        Command::SetInputSendParam(param, _) => Key::InputSendParam(param as usize),
        Command::SetInputSend(send, _) => Key::InputSend(send as usize),
        Command::SetLoopEndStop(_) => Key::LoopEndStop,
        Command::SetFadeBars(_) => Key::FadeBars,
        Command::SetFixedLength(_) => Key::FixedLength,
        Command::SetFixedBars(_) => Key::FixedBars,
        Command::SetRetake(_) => Key::Retake,
        Command::SetAutoRecord(_) => Key::AutoRecord,
        Command::SetAutoSensitivity(_) => Key::AutoSensitivity,
        Command::SelectTrack(i) => lane(i).map(|_| Key::SelectTrack)?,
        Command::SetVolume(i, _) => Key::Volume(lane(i)?),
        Command::SetMute(i, _) => Key::Mute(lane(i)?),
        Command::SetDubFeedback(i, _) => Key::DubFeedback(lane(i)?),
        Command::SetPan(i, _) => Key::Pan(lane(i)?),
        Command::SetFxBypass(i, kind, _) => Key::FxBypass(lane(i)?, kind.index()),
        Command::SetFxParam(i, param, _) => Key::FxParam(lane(i)?, param.kind().index() * MAX_PARAMS + param.index()),
        Command::SetInstrumentGain(i, _) => Key::InstrumentGain(i as usize),
        Command::SelectInstrument(_) => Key::Instrument,
        Command::PitchBend(_) => Key::PitchBend,
        Command::Modulation(_) => Key::Modulation,
        Command::SetSlotLive(i, _) => Key::SlotLive(slot(i)?),
        Command::SetSlotGain(i, _) => Key::SlotGain(slot(i)?),
        Command::RecDub(_)
        | Command::PlayStop(_)
        | Command::Stop(_)
        | Command::Undo(_)
        | Command::Reverse(_)
        | Command::Copy(_)
        | Command::Trim(..)
        | Command::Clear(_)
        | Command::PlayAll
        | Command::StopAll
        | Command::ClearAll
        | Command::Action(_)
        | Command::ActionOn(..)
        | Command::Press
        | Command::NoteOn(..)
        | Command::NoteOff(_)
        | Command::AllNotesOff => return None,
    })
}

/// The toggled setting `command` sets (a setter: true) or switches (its toggle: false).
fn toggled_by(command: &Command) -> Option<(Toggle, bool)> {
    Some(match *command {
        Command::SetMetronome(_) => (Toggle::Click, true),
        Command::SetLoopEndStop(_) => (Toggle::EndStop, true),
        Command::SetFixedLength(_) => (Toggle::Fixed, true),
        Command::SetRetake(_) => (Toggle::Retake, true),
        Command::SetAutoRecord(_) => (Toggle::AutoRec, true),
        Command::SetInputSend(send, _) => (Toggle::Send(send), true),
        Command::Action(Action::Toggle(t)) | Command::ActionOn(_, Action::Toggle(t)) => (t, false),
        _ => return None,
    })
}

pub(crate) struct Settings {
    last: BTreeMap<Key, Command>,
    /// Each lane's last `Mix` and its frame; `None` until one arrives (the lane's entries in `last` are
    /// then bootstrap).
    applied: [Option<(Frame, CompactMix)>; TRACK_COUNT],
    /// The engine generation (`Core::engine_gen`) whose `Mix` events the projection takes.
    gen: u64,
    /// Per toggled setting (`Toggle::index`): its commands the followed engine's ring took that it has not
    /// answered yet, and how many of them, counted from the oldest, reach the last setter among them (0:
    /// no setter in flight).
    in_flight: [u32; Toggle::COUNT],
    setter_at: [u32; Toggle::COUNT],
}

impl Default for Settings {
    fn default() -> Self {
        Settings { last: BTreeMap::new(), applied: [None; TRACK_COUNT], gen: 0, in_flight: [0; Toggle::COUNT], setter_at: [0; Toggle::COUNT] }
    }
}

impl Settings {
    /// Whether `command` is a setting; kept unless it sets the mix of a lane the engine has reported.
    pub(crate) fn record(&mut self, command: &Command) -> bool {
        let Some(key) = key(command) else { return false };
        if key.lane().is_none_or(|lane| self.applied[usize::from(lane)].is_none()) {
            self.last.insert(key, *command);
        }
        true
    }

    /// The engine of generation `gen` applied `mix` to lane `lane` (its `Event::Mix`): the lane's mix is
    /// that one from here on. False, and nothing kept, for an engine this memory no longer follows.
    pub(crate) fn mixed(&mut self, gen: u64, frame: Frame, lane: u8, mix: &CompactMix) -> bool {
        if gen != self.gen || usize::from(lane) >= TRACK_COUNT {
            return false;
        }
        self.applied[usize::from(lane)] = Some((frame, *mix));
        self.last.retain(|k, _| k.lane() != Some(lane));
        let fresh = CompactMix::from(&LaneMix::default());
        let wide = mix.widen();
        if mix.volume != fresh.volume {
            self.insert(Command::SetVolume(lane, mix.volume));
        }
        if mix.muted != fresh.muted {
            self.insert(Command::SetMute(lane, mix.muted));
        }
        if mix.dub_feedback != fresh.dub_feedback {
            self.insert(Command::SetDubFeedback(lane, mix.dub_feedback));
        }
        if mix.pan != fresh.pan {
            self.insert(Command::SetPan(lane, mix.pan));
        }
        for kind in FxKind::ALL {
            let (now, was) = (&mix.fx[kind.index()], &fresh.fx[kind.index()]);
            for (i, def) in kind.params().iter().enumerate() {
                if let Some(param) = FxParam::from_key(kind, def.key).filter(|_| now.params[i] != was.params[i]) {
                    self.insert(Command::SetFxParam(lane, param, wide.fx[kind.index()].params[i]));
                }
            }
            if now.bypassed != was.bypassed {
                self.insert(Command::SetFxBypass(lane, kind, now.bypassed));
            }
        }
        true
    }

    /// The followed engine's command ring took `command`: a command of a toggled setting is now in flight
    /// until the engine answers it (`toggled`, `refused`).
    pub(crate) fn pushed(&mut self, command: &Command) {
        let Some((toggle, setter)) = toggled_by(command) else { return };
        let k = toggle.index();
        self.in_flight[k] += 1;
        if setter {
            self.setter_at[k] = self.in_flight[k];
        }
    }

    /// The engine of generation `gen` answered the oldest command of `toggle` in flight with `on` (its
    /// `Event::Toggled`, or an owed one read at a rebuild): kept as the setter that sets it, unless a
    /// setter pushed after that command is still unanswered (its value, kept as sent, is newer). False,
    /// and nothing kept, for an engine this memory no longer follows.
    pub(crate) fn toggled(&mut self, gen: u64, toggle: Toggle, on: bool) -> bool {
        if gen != self.gen {
            return false;
        }
        if self.answered(toggle) {
            self.insert(toggle.setter(on));
        }
        true
    }

    /// The engine of generation `gen` refused a toggle (`Event::Refused`): the answer to its command, which
    /// changed nothing. Any other refusal is no toggle's.
    pub(crate) fn refused(&mut self, gen: u64, reason: Refusal) {
        let toggle = match reason {
            Refusal::FixedCapturing | Refusal::FixedRetake => Toggle::Fixed,
            Refusal::RetakeCapturing => Toggle::Retake,
            Refusal::AutoRecCapturing | Refusal::AutoRecLocked => Toggle::AutoRec,
            _ => return,
        };
        if gen == self.gen {
            self.answered(toggle);
        }
    }

    /// One answer for `toggle`: its oldest command in flight is done. True when no setter is left in flight.
    fn answered(&mut self, toggle: Toggle) -> bool {
        let k = toggle.index();
        self.in_flight[k] = self.in_flight[k].saturating_sub(1);
        self.setter_at[k] = self.setter_at[k].saturating_sub(1);
        self.setter_at[k] == 0
    }

    fn insert(&mut self, command: Command) {
        if let Some(key) = key(&command) {
            self.last.insert(key, command);
        }
    }

    /// A new engine of generation `gen` is to be replayed this memory: only its events count from here
    /// on, each lane keeping the mix it has until the new engine's first `Mix` for it, and no command of a
    /// toggled setting is in flight in it until the replay is `pushed`.
    pub(crate) fn follow(&mut self, gen: u64) {
        self.gen = gen;
        self.in_flight = [0; Toggle::COUNT];
        self.setter_at = [0; Toggle::COUNT];
    }

    /// Every kept setting, in replay order.
    pub(crate) fn replay(&self) -> impl Iterator<Item = Command> + '_ {
        self.last.values().copied()
    }

    /// Each lane's last `Mix`, for a lane that has had one (a reset frame carries them).
    pub(crate) fn mixes(&self) -> impl Iterator<Item = Event> + '_ {
        self.applied.iter().enumerate().filter_map(|(lane, a)| a.map(|(frame, mix)| Event::Mix { frame, lane: lane as u8, mix }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_engine::dsp::fx::{FxKind, FxParam};
    use lf_engine::{InputSend, InputSendParam, Instrument, NoteTarget};

    fn replay(s: &Settings) -> Vec<Command> {
        s.replay().collect()
    }

    #[test]
    fn the_last_value_of_each_setting_is_kept_and_actions_are_not() {
        let mut s = Settings::default();
        assert!(s.record(&Command::SetBpm(90.0)));
        assert!(s.record(&Command::SetBpm(100.0)));
        assert!(!s.record(&Command::RecDub(0)));
        assert!(!s.record(&Command::NoteOn(60, 1.0)));
        assert!(s.record(&Command::PitchBend(1.0)));
        assert!(s.record(&Command::SelectInstrument(NoteTarget::Builtin(Instrument::Pad))));
        assert!(!s.record(&Command::SetVolume(9, 0.5)), "a lane out of range is not a setting");
        assert!(!s.record(&Command::SetSlotLive(2, true)), "a slot out of range is not a setting");
        assert!(s.record(&Command::SetInputSend(InputSend::Echo, true)));
        assert!(s.record(&Command::SetInputSendParam(InputSendParam::EchoLevel, 0.2)));
        assert!(s.record(&Command::SetInputSendParam(InputSendParam::EchoLevel, 0.6)));
        assert!(s.record(&Command::SetFadeBars(4)));
        assert!(s.record(&Command::SetInstrumentGain(Instrument::Bass, 0.3)));
        assert!(s.record(&Command::SetInstrumentGain(Instrument::Lead, 0.5)));
        assert!(s.record(&Command::SetInstrumentGain(Instrument::Bass, 0.4)));
        assert_eq!(
            replay(&s),
            [
                Command::SetBpm(100.0),
                Command::SetInputSendParam(InputSendParam::EchoLevel, 0.6),
                Command::SetInputSend(InputSend::Echo, true),
                Command::SetFadeBars(4),
                Command::SetInstrumentGain(Instrument::Lead, 0.5),
                Command::SetInstrumentGain(Instrument::Bass, 0.4),
                Command::SelectInstrument(NoteTarget::Builtin(Instrument::Pad)),
                Command::PitchBend(1.0)
            ],
            "the note target is replayed before the wheels it hands over, a send's values before the send"
        );
    }

    /// Lane `lane`'s mix as an `Event::Mix` carries it, from a fresh lane's with `edit` applied.
    fn mix(edit: impl FnOnce(&mut LaneMix)) -> CompactMix {
        let mut m = LaneMix::default();
        edit(&mut m);
        CompactMix::from(&m)
    }

    #[test]
    fn a_lanes_mix_is_the_bootstrap_until_the_engine_reports_it_then_what_the_engine_applied() {
        let mut s = Settings::default();
        s.record(&Command::SetVolume(0, 0.5));
        s.record(&Command::SetMute(1, true));
        s.record(&Command::SetMasterVolume(0.7));
        assert_eq!(replay(&s), [Command::SetMasterVolume(0.7), Command::SetVolume(0, 0.5), Command::SetMute(1, true)], "kept as sent before any engine");
        assert!(s.mixed(0, 100, 0, &mix(|m| m.volume = 0.25)));
        assert!(s.record(&Command::SetVolume(0, 0.9)), "still a setting");
        assert!(s.record(&Command::SetFxBypass(0, FxKind::Delay, false)));
        assert_eq!(
            replay(&s),
            [Command::SetMasterVolume(0.7), Command::SetVolume(0, 0.25), Command::SetMute(1, true)],
            "lane 0 is what the engine applied, not what was sent since; lane 1 waits for its first Mix"
        );
        // A COPY, CLEAR or pedal MUTE reaches the memory as the lane's next Mix: a fresh lane's keeps nothing.
        assert!(s.mixed(0, 200, 1, &CompactMix::from(&LaneMix::default())));
        assert!(s.mixed(0, 200, 0, &mix(|m| {
            m.muted = true;
            m.dub_feedback = 0.25;
            m.fx[FxKind::Delay.index()].bypassed = false;
        })));
        assert_eq!(
            replay(&s),
            [
                Command::SetMasterVolume(0.7),
                Command::SetMute(0, true),
                Command::SetDubFeedback(0, 0.25),
                Command::SetFxBypass(0, FxKind::Delay, false),
            ],
            "only what differs from a fresh lane"
        );
        let mixes: Vec<Event> = s.mixes().collect();
        assert_eq!(mixes.len(), 2, "the two lanes the engine reported: {mixes:?}");
        assert!(matches!(mixes[0], Event::Mix { frame: 200, lane: 0, mix } if mix.muted));
    }

    #[test]
    fn a_toggled_setting_is_kept_as_its_setter_at_the_value_the_engine_applied() {
        let mut s = Settings::default();
        s.follow(1);
        assert!(!s.record(&Command::Action(lf_engine::Action::Toggle(Toggle::Click))), "a toggle is an action, kept nowhere");
        assert!(s.toggled(1, Toggle::Click, true));
        assert!(s.toggled(1, Toggle::Send(InputSend::Reverb), true));
        assert!(s.toggled(1, Toggle::Send(InputSend::Reverb), false));
        assert_eq!(replay(&s), [Command::SetMetronome(true), Command::SetInputSend(InputSend::Reverb, false)], "an off kept too: the UI pushes a send the memory lacks");
        assert!(s.record(&Command::SetMetronome(false)), "a setter is still kept as sent");
        assert_eq!(replay(&s)[0], Command::SetMetronome(false));
        assert!(s.toggled(1, Toggle::Click, true), "then the engine's next value");
        assert_eq!(replay(&s)[0], Command::SetMetronome(true));
        s.follow(2);
        assert!(!s.toggled(1, Toggle::Retake, true), "a replaced engine's late Toggled");
        assert!(!replay(&s).contains(&Command::SetRetake(true)));
        for t in Toggle::ALL {
            assert_eq!(key(&t.setter(true)), key(&t.setter(false)), "{t:?}: one key per setting");
            assert!(key(&t.setter(true)).is_some());
        }
    }

    #[test]
    fn an_answer_older_than_a_setter_in_flight_never_overwrites_it() {
        let mut s = Settings::default();
        s.follow(1);
        let toggle = Command::Action(lf_engine::Action::Toggle(Toggle::Click));
        s.pushed(&toggle);
        assert!(s.record(&Command::SetMetronome(false)));
        s.pushed(&Command::SetMetronome(false));
        s.pushed(&toggle);
        assert!(s.toggled(1, Toggle::Click, true), "the first toggle's answer");
        assert_eq!(replay(&s), [Command::SetMetronome(false)], "older than the setter in flight: the setter stays");
        assert!(s.toggled(1, Toggle::Click, false), "the setter's own answer");
        assert_eq!(replay(&s), [Command::SetMetronome(false)]);
        assert!(s.toggled(1, Toggle::Click, true), "the last toggle's");
        assert_eq!(replay(&s), [Command::SetMetronome(true)], "no setter in flight: the applied value");
        // A refused toggle answers with its Refused, and a refusal that is no toggle's answers nothing.
        s.pushed(&Command::SetRetake(true));
        s.record(&Command::SetRetake(true));
        s.pushed(&Command::Action(lf_engine::Action::Toggle(Toggle::Fixed)));
        s.pushed(&Command::SetFixedLength(false));
        s.record(&Command::SetFixedLength(false));
        s.refused(1, Refusal::NoFade);
        s.refused(1, Refusal::FixedRetake);
        assert!(s.toggled(1, Toggle::Fixed, false), "the setter's answer, its refused toggle's counted");
        assert!(replay(&s).contains(&Command::SetFixedLength(false)));
        assert!(s.toggled(1, Toggle::Retake, true));
        s.follow(2);
        s.pushed(&toggle);
        assert!(s.toggled(2, Toggle::Click, false), "a new engine counts from nothing");
        assert!(replay(&s).contains(&Command::SetMetronome(false)));
    }

    #[test]
    fn a_mix_from_an_engine_the_memory_no_longer_follows_is_refused() {
        let mut s = Settings::default();
        s.follow(1);
        assert!(s.mixed(1, 0, 2, &mix(|m| m.volume = 0.5)));
        s.follow(2);
        assert!(!s.mixed(1, 10, 2, &mix(|m| m.volume = 0.1)), "the replaced engine's late Mix");
        assert_eq!(replay(&s), [Command::SetVolume(2, 0.5)], "the new engine's replay stands");
        assert!(s.mixed(2, 20, 2, &mix(|m| m.volume = 0.75)));
        assert_eq!(replay(&s), [Command::SetVolume(2, 0.75)]);
    }

    #[test]
    fn a_lanes_pan_is_a_lane_setting_kept_off_the_centre() {
        let mut s = Settings::default();
        assert!(s.record(&Command::SetPan(1, 0.5)), "a setting");
        assert!(!s.record(&Command::SetPan(5, 0.5)), "a lane out of range is not");
        s.record(&Command::SetFxBypass(1, FxKind::Delay, false));
        s.record(&Command::SetVolume(1, 0.25));
        assert_eq!(
            replay(&s),
            [Command::SetVolume(1, 0.25), Command::SetPan(1, 0.5), Command::SetFxBypass(1, FxKind::Delay, false)],
            "bootstrap, with the lane's other settings"
        );
        assert!(s.mixed(0, 10, 1, &mix(|m| m.pan = -0.25)));
        assert_eq!(replay(&s), [Command::SetPan(1, -0.25)], "what the engine applied");
        assert!(s.mixed(0, 20, 1, &mix(|_| {})));
        assert!(replay(&s).is_empty(), "a centred lane keeps no pan");
    }

    #[test]
    fn an_fx_param_with_up_to_seven_digits_comes_back_as_it_was_sent() {
        let mut s = Settings::default();
        assert!(s.mixed(0, 0, 3, &mix(|m| {
            m.fx[FxKind::Filter.index()].params = [1234.5, 0.3, 0.0];
            m.fx[FxKind::Delay.index()].params[1] = 0.95;
        })));
        let kept = replay(&s);
        for command in [Command::SetFxParam(3, FxParam::Cutoff, 1234.5), Command::SetFxParam(3, FxParam::Q, 0.3), Command::SetFxParam(3, FxParam::Feedback, 0.95)] {
            assert!(kept.contains(&command), "{command:?} in {kept:?}");
        }
    }
}
