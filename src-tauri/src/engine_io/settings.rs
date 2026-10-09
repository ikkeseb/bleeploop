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
//! value, so the memory takes its value from what the engine applied: each `Event::Toggled` the feed
//! drains from the engine generation it follows (`toggled`), kept as the setter that sets it. Its setter
//! is still kept as sent too (initialization, a replay, a script): no engine-made change competes with
//! it, and the engine reports the value the setter leaves, so the two converge on the applied value.
//! A rebuild drains the replaced engine's event ring, then reads the toggles it applied whose event the
//! full ring refused (`Engine::unsent_toggles`), so an accepted toggle survives the rebuild whether or not
//! the feed saw it. Not kept: a setter still queued in a replaced engine with no device behind it, when
//! that engine also holds an unsent toggle of the same setting (the unsent value wins); and a toggle
//! still queued there, as every queued action.

use std::collections::BTreeMap;

use lf_engine::dsp::fx::{FxKind, FxParam, MAX_PARAMS};
use lf_engine::grid::Frame;
use lf_engine::{Command, CompactMix, Event, LaneMix, Toggle, SLOT_COUNT, TRACK_COUNT};

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

pub(crate) struct Settings {
    last: BTreeMap<Key, Command>,
    /// Each lane's last `Mix` and its frame; `None` until one arrives (the lane's entries in `last` are
    /// then bootstrap).
    applied: [Option<(Frame, CompactMix)>; TRACK_COUNT],
    /// The engine generation (`Core::engine_gen`) whose `Mix` events the projection takes.
    gen: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { last: BTreeMap::new(), applied: [None; TRACK_COUNT], gen: 0 }
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

    /// The engine of generation `gen` applied toggled setting `toggle` as `on` (its `Event::Toggled`, or
    /// its unsent value read at a rebuild): kept as the setter that sets it. False, and nothing kept, for
    /// an engine this memory no longer follows.
    pub(crate) fn toggled(&mut self, gen: u64, toggle: Toggle, on: bool) -> bool {
        if gen != self.gen {
            return false;
        }
        self.insert(toggle.setter(on));
        true
    }

    fn insert(&mut self, command: Command) {
        if let Some(key) = key(&command) {
            self.last.insert(key, command);
        }
    }

    /// A new engine of generation `gen` was replayed this memory: only its `Mix` events count from here
    /// on, each lane keeping the mix it has until the new engine's first `Mix` for it.
    pub(crate) fn follow(&mut self, gen: u64) {
        self.gen = gen;
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
