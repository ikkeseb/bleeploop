use lf_engine::{Action, Command, Toggle};
#[cfg(debug_assertions)]
use lf_engine::UnitParam;

/// Where one of the UI's engine commands (`input_send`'s) goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiRoute {
    /// An input command (a looper press, `Press`, `SelectTrack`, a toggle, the tuner's press and its
    /// setter): the one queue, in order with the pedals, under the same no-device rule (but for the
    /// tuner's close: `MidiHost::ui_commands`).
    Queue,
    /// A note, a wheel, the note target, a panic or a synth pick (`SetUnitParam(_, Instrument, _)`,
    /// `docs/unit-contract.md` §2: "sent only by the note router"): the router's alone (one ordered path
    /// into the engine). Refused here: sent past the router it would break note ownership (a pick
    /// releases what sounds in its own batch). The pick's refusal is DEV only, as the router's pick: a
    /// release build routes `SetUnitParam(_, Instrument, _)` to the queue as every unit command, so a
    /// release `input_send` batch runs exactly as before the pick existed.
    Router,
    /// A setting: straight to `EngineHost::send`, which keeps it (a rebuild would discard it queued).
    Direct,
    /// GO LIVE's toggle ([`Toggle::Live`]): straight to `EngineHost::send` as the slot's live setter
    /// goes, so every live operation (a press, the UI's setters, an unload's live-off) reaches the
    /// engine in the order it was made; queued, a press waiting behind a full ring could land after a
    /// later live-off and open the slot again. A press: refused while no device runs, as the queue
    /// refuses one, and when the ring has no room (never retried).
    Live,
}

/// Which way `command` goes when the UI sends it.
pub(crate) fn ui_route(command: &Command) -> UiRoute {
    match command {
        Command::Action(Action::Toggle(Toggle::Live(_))) | Command::ActionOn(_, Action::Toggle(Toggle::Live(_))) => UiRoute::Live,
        #[cfg(debug_assertions)]
        Command::SetUnitParam(_, UnitParam::Instrument, _) => UiRoute::Router,
        Command::RecDub(_)
        | Command::PlayStop(_)
        | Command::Stop(_)
        | Command::Undo(_)
        | Command::Reverse(_)
        | Command::Copy(_)
        | Command::CopyWide(_)
        | Command::Trim(..)
        | Command::Clear(_)
        | Command::PlayAll
        | Command::StopAll
        | Command::ClearAll
        | Command::Action(_)
        | Command::ActionOn(..)
        | Command::Press
        | Command::SelectTrack(_)
        | Command::SetTuner(_)
        | Command::SetUnitBypass(..)
        | Command::SetUnitParam(..) => UiRoute::Queue,
        Command::NoteOn(..)
        | Command::NoteOff(_)
        | Command::PitchBend(_)
        | Command::Modulation(_)
        | Command::SelectInstrument(_)
        | Command::AllNotesOff => UiRoute::Router,
        Command::SetBpm(_)
        | Command::SetMeter(_)
        | Command::SetMetronome(_)
        | Command::SetClickVolume(_)
        | Command::SetMasterVolume(_)
        | Command::SetMasterMute(_)
        | Command::SetLoopEndStop(_)
        | Command::SetFadeBars(_)
        | Command::SetFixedLength(_)
        | Command::SetFixedBars(_)
        | Command::SetRetake(_)
        | Command::SetAutoRecord(_)
        | Command::SetAutoSensitivity(_)
        | Command::SetVolume(..)
        | Command::SetMute(..)
        | Command::SetDubFeedback(..)
        | Command::SetPan(..)
        | Command::SetFxParam(..)
        | Command::SetFxBypass(..)
        | Command::SetSlotLive(..)
        | Command::SetSlotGain(..)
        | Command::SetInstrumentGain(..)
        | Command::SetInputSend(..)
        | Command::SetInputSendParam(..)
        | Command::SetScope(_) => UiRoute::Direct,
    }
}
