//! OWNS: what a MIDI-learn binding runs ([`Fire`]) as engine commands and UI events, ported from
//! `src/app/actions.ts` (`runAction`, `pressHold`, `releaseHold`), and which of the UI's own commands
//! join the input queue ([`ui_route`]). Pure.
//!
//! - **A lane action is the engine's `Action`:** on the lane the engine has selected when it lands
//!   (`Command::Action`, never the UI's copy of the selection), or `Command::ActionOn` a named lane. A
//!   named REC/DUB selects its lane first (`SelectTrack`, as `looper.selectTrack(target)` does), so the
//!   transport keys follow the take; another named action leaves the selection alone. HALVE is `Halve`.
//! - **NEXT/PREV TRACK, PLAY ALL, STOP ALL, FADE and the eight toggles are the engine's own actions.**
//!   A toggle is a looper press itself (it disarms a pending CLEAR), so it goes with no `Press`.
//! - **GO LIVE and TAP stay the UI's:** a `Press` tells the engine a looper press came, and the UI runs
//!   the action on its event. The stage view and its next look are no looper presses: an event only,
//!   nothing for the engine, so a pending CLEAR and a lane cue outlive them.
//! - **HOLD's press is REC/DUB's `Hold` by its control**, a named lane selected first; its release is
//!   `Release` by the same control and no looper press (`releaseHold`).
//! - **Every looper press is also a [`MidiEvent::Pressed`]** (`onPress`: the UI takes its lane cue
//!   down), before the action's own event.
//!
//! One `Fire` is one batch for the queue, whole or not at all: a refused one changes nothing in the
//! engine, and the events go to the UI either way, as a click's `onPress` and facade call run whatever
//! the engine does with the press.

use lf_engine::{Action, Command, InputSend, Toggle};

use super::bindings::ActionId;
use super::learn::Fire;
use super::queue::Out;
use super::{MidiEvent, UiAction};

/// What one [`Fire`] asks of the engine and the UI.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Run {
    /// One batch, admitted whole.
    pub(crate) out: Vec<Out>,
    /// For the UI, in order.
    pub(crate) events: Vec<MidiEvent>,
}

/// A lane action's engine action.
fn lane(action: ActionId) -> Option<Action> {
    Some(match action {
        ActionId::RecDub => Action::RecDub,
        ActionId::PlayStop => Action::PlayStop,
        ActionId::Undo => Action::Undo,
        ActionId::Clear => Action::Clear,
        ActionId::Mute => Action::Mute,
        ActionId::Reverse => Action::Reverse,
        ActionId::Copy => Action::Copy,
        ActionId::HalveTrack => Action::Halve,
        _ => return None,
    })
}

/// A global action the engine runs as its own (`ENGINE_GLOBAL`).
fn engine_global(action: ActionId) -> Option<Action> {
    Some(match action {
        ActionId::NextTrack => Action::NextTrack,
        ActionId::PrevTrack => Action::PrevTrack,
        ActionId::PlayAll => Action::PlayAll,
        ActionId::StopAll => Action::StopAll,
        ActionId::FadeAll => Action::FadeAll,
        ActionId::ClickToggle => Action::Toggle(Toggle::Click),
        ActionId::EndStopToggle => Action::Toggle(Toggle::EndStop),
        ActionId::FixedToggle => Action::Toggle(Toggle::Fixed),
        ActionId::RetakeToggle => Action::Toggle(Toggle::Retake),
        ActionId::AutoRecToggle => Action::Toggle(Toggle::AutoRec),
        ActionId::InFxEcho => Action::Toggle(Toggle::Send(InputSend::Echo)),
        ActionId::InFxReverb => Action::Toggle(Toggle::Send(InputSend::Reverb)),
        ActionId::InFxRing => Action::Toggle(Toggle::Send(InputSend::Ring)),
        _ => return None,
    })
}

/// A global action the UI runs (`GLOBAL`'s facade rows) and whether it is a looper press.
fn ui_action(action: ActionId) -> Option<(UiAction, bool)> {
    Some(match action {
        ActionId::GoLive => (UiAction::GoLive, true),
        ActionId::TapTempo => (UiAction::TapTempo, true),
        ActionId::StageView => (UiAction::StageView, false),
        ActionId::StageNextView => (UiAction::StageNextView, false),
        _ => return None,
    })
}

/// `action` on `target` (a named lane), or on the lane the engine has selected.
fn on(target: Option<u8>, action: Action) -> Command {
    match target {
        Some(i) => Command::ActionOn(i, action),
        None => Command::Action(action),
    }
}

/// What `fire` runs: `runAction`, `pressHold` or `releaseHold`.
pub(crate) fn run(fire: Fire) -> Run {
    let commands: Vec<Command>;
    let mut events = Vec::new();
    match fire {
        Fire::Run { action, target } => {
            if let Some(a) = lane(action) {
                events.push(MidiEvent::Pressed);
                commands = match target {
                    Some(i) if action == ActionId::RecDub => vec![Command::SelectTrack(i), on(target, a)],
                    _ => vec![on(target, a)],
                };
            } else if let Some(a) = engine_global(action) {
                events.push(MidiEvent::Pressed);
                commands = vec![Command::Action(a)];
            } else if let Some((ui, press)) = ui_action(action) {
                if press {
                    events.push(MidiEvent::Pressed);
                }
                events.push(MidiEvent::Run { action: ui });
                commands = if press { vec![Command::Press] } else { Vec::new() };
            } else {
                unreachable!("{action:?} has no row");
            }
        }
        Fire::HoldPress { target, control } => {
            events.push(MidiEvent::Pressed);
            let hold = on(target, Action::Hold(control));
            commands = match target {
                Some(i) => vec![Command::SelectTrack(i), hold],
                None => vec![hold],
            };
        }
        Fire::HoldRelease { control } => commands = vec![Command::Action(Action::Release(control))],
    }
    Run { out: commands.into_iter().map(Out::new).collect(), events }
}

/// Where one of the UI's engine commands (`input_send`'s) goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiRoute {
    /// An input command (a looper press, `Press`, `SelectTrack`, a toggle): the one queue, in order
    /// with the pedals, under the same no-device rule.
    Queue,
    /// A note, a wheel, the note target or a panic: the router's alone (one ordered path into the
    /// engine). Refused here: sent past the router it would break note ownership.
    Router,
    /// A setting: straight to `EngineHost::send`, which keeps it (a rebuild would discard it queued).
    Direct,
}

/// Which way `command` goes when the UI sends it.
pub(crate) fn ui_route(command: &Command) -> UiRoute {
    match command {
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
        | Command::SelectTrack(_) => UiRoute::Queue,
        Command::NoteOn(..)
        | Command::NoteOff(_)
        | Command::PitchBend(_)
        | Command::Modulation(_)
        | Command::SelectInstrument(_)
        | Command::AllNotesOff => UiRoute::Router,
        Command::SetBpm(_)
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
        | Command::SetInputSendParam(..) => UiRoute::Direct,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_engine::dsp::fx::FxKind;
    use lf_engine::{Instrument, InputSendParam, NoteTarget};

    use super::super::queue::Class;

    fn commands(run: &Run) -> Vec<Command> {
        run.out.iter().map(|o| o.command).collect()
    }

    // actions.ts runAction, pressHold and releaseHold, row by row: all 25 actions on the selected lane
    // and, for the eight lane actions, on a named one; HOLD's press both ways and its release.
    #[test]
    fn the_fire_table() {
        use ActionId::*;
        use MidiEvent::Pressed;
        let pressed = || vec![Pressed];
        let ui = |action: UiAction, press: bool| if press { vec![Pressed, MidiEvent::Run { action }] } else { vec![MidiEvent::Run { action }] };
        let a = |action: Action| vec![Command::Action(action)];
        let on2 = |action: Action| vec![Command::ActionOn(2, action)];
        let rows: Vec<(Fire, Vec<Command>, Vec<MidiEvent>)> = vec![
            (Fire::Run { action: RecDub, target: None }, a(Action::RecDub), pressed()),
            (Fire::Run { action: RecDub, target: Some(2) }, vec![Command::SelectTrack(2), Command::ActionOn(2, Action::RecDub)], pressed()),
            (Fire::Run { action: PlayStop, target: None }, a(Action::PlayStop), pressed()),
            (Fire::Run { action: PlayStop, target: Some(2) }, on2(Action::PlayStop), pressed()),
            (Fire::Run { action: Undo, target: None }, a(Action::Undo), pressed()),
            (Fire::Run { action: Undo, target: Some(2) }, on2(Action::Undo), pressed()),
            (Fire::Run { action: Clear, target: None }, a(Action::Clear), pressed()),
            (Fire::Run { action: Clear, target: Some(2) }, on2(Action::Clear), pressed()),
            (Fire::Run { action: Mute, target: None }, a(Action::Mute), pressed()),
            (Fire::Run { action: Mute, target: Some(2) }, on2(Action::Mute), pressed()),
            (Fire::Run { action: Reverse, target: None }, a(Action::Reverse), pressed()),
            (Fire::Run { action: Reverse, target: Some(2) }, on2(Action::Reverse), pressed()),
            (Fire::Run { action: Copy, target: None }, a(Action::Copy), pressed()),
            (Fire::Run { action: Copy, target: Some(2) }, on2(Action::Copy), pressed()),
            (Fire::Run { action: HalveTrack, target: None }, a(Action::Halve), pressed()),
            (Fire::Run { action: HalveTrack, target: Some(2) }, on2(Action::Halve), pressed()),
            (Fire::Run { action: NextTrack, target: None }, a(Action::NextTrack), pressed()),
            (Fire::Run { action: PrevTrack, target: None }, a(Action::PrevTrack), pressed()),
            (Fire::Run { action: PlayAll, target: None }, a(Action::PlayAll), pressed()),
            (Fire::Run { action: StopAll, target: None }, a(Action::StopAll), pressed()),
            (Fire::Run { action: FadeAll, target: None }, a(Action::FadeAll), pressed()),
            (Fire::Run { action: GoLive, target: None }, vec![Command::Press], ui(UiAction::GoLive, true)),
            (Fire::Run { action: StageView, target: None }, vec![], ui(UiAction::StageView, false)),
            (Fire::Run { action: StageNextView, target: None }, vec![], ui(UiAction::StageNextView, false)),
            (Fire::Run { action: TapTempo, target: None }, vec![Command::Press], ui(UiAction::TapTempo, true)),
            (Fire::Run { action: ClickToggle, target: None }, a(Action::Toggle(Toggle::Click)), pressed()),
            (Fire::Run { action: EndStopToggle, target: None }, a(Action::Toggle(Toggle::EndStop)), pressed()),
            (Fire::Run { action: FixedToggle, target: None }, a(Action::Toggle(Toggle::Fixed)), pressed()),
            (Fire::Run { action: RetakeToggle, target: None }, a(Action::Toggle(Toggle::Retake)), pressed()),
            (Fire::Run { action: AutoRecToggle, target: None }, a(Action::Toggle(Toggle::AutoRec)), pressed()),
            (Fire::Run { action: InFxEcho, target: None }, a(Action::Toggle(Toggle::Send(InputSend::Echo))), pressed()),
            (Fire::Run { action: InFxReverb, target: None }, a(Action::Toggle(Toggle::Send(InputSend::Reverb))), pressed()),
            (Fire::Run { action: InFxRing, target: None }, a(Action::Toggle(Toggle::Send(InputSend::Ring))), pressed()),
            (Fire::HoldPress { target: None, control: 3 }, a(Action::Hold(3)), pressed()),
            (Fire::HoldPress { target: Some(4), control: 3 }, vec![Command::SelectTrack(4), Command::ActionOn(4, Action::Hold(3))], pressed()),
            (Fire::HoldRelease { control: 3 }, a(Action::Release(3)), vec![]),
        ];
        let mut covered: Vec<ActionId> = Vec::new();
        for (fire, want, events) in rows {
            let got = run(fire);
            assert_eq!((commands(&got), got.events), (want, events), "{fire:?}");
            if let Fire::Run { action, .. } = fire {
                if !covered.contains(&action) {
                    covered.push(action);
                }
            }
        }
        assert_eq!(covered, ActionId::ALL, "every action has its row, in the picker's order");
    }

    // The queue's classes: a toggle is a fresh action, HOLD's press reserves its release, its release
    // is a release; one fire is one batch.
    #[test]
    fn hold_goes_to_the_queue_as_a_press_and_its_release() {
        let classes = |fire: Fire| run(fire).out.iter().map(|o| o.class).collect::<Vec<_>>();
        assert_eq!(classes(Fire::HoldPress { target: Some(1), control: 0 }), [Class::Action, Class::HoldPress]);
        assert_eq!(classes(Fire::HoldRelease { control: 0 }), [Class::HoldRelease]);
        assert_eq!(classes(Fire::Run { action: ActionId::ClickToggle, target: None }), [Class::Action]);
    }

    // The UI's input commands join the queue; the router's commands are refused there; a setting
    // goes straight to the engine.
    #[test]
    fn the_uis_commands_split_into_input_router_and_settings() {
        let input = [
            Command::RecDub(1),
            Command::PlayStop(1),
            Command::Stop(1),
            Command::Undo(1),
            Command::Reverse(1),
            Command::Copy(1),
            Command::Trim(1, 2),
            Command::Clear(1),
            Command::PlayAll,
            Command::StopAll,
            Command::ClearAll,
            Command::Action(Action::Toggle(Toggle::Click)),
            Command::ActionOn(2, Action::RecDub),
            Command::Press,
            Command::SelectTrack(3),
        ];
        let router = [
            Command::NoteOn(60, 0.5),
            Command::NoteOff(60),
            Command::PitchBend(1.0),
            Command::Modulation(0.5),
            Command::SelectInstrument(NoteTarget::Builtin(Instrument::Pad)),
            Command::AllNotesOff,
        ];
        let settings = [
            Command::SetBpm(120.0),
            Command::SetMetronome(true),
            Command::SetMasterVolume(0.5),
            Command::SetVolume(0, 0.5),
            Command::SetFxBypass(0, FxKind::Delay, true),
            Command::SetSlotLive(0, true),
            Command::SetInputSend(InputSend::Echo, true),
            Command::SetInputSendParam(InputSendParam::EchoLevel, 0.2),
            Command::SetInstrumentGain(Instrument::Lead, 0.5),
        ];
        for (commands, route) in [(&input[..], UiRoute::Queue), (&router[..], UiRoute::Router), (&settings[..], UiRoute::Direct)] {
            for c in commands {
                assert_eq!(ui_route(c), route, "{c:?}");
            }
        }
    }
}
