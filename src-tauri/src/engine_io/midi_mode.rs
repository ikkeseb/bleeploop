//! OWNS: native MIDI's Tauri commands: the UI's one ordered input path (`input_send`: its engine
//! commands and its note-source events, in the order the outbox queued them), the document's
//! subscription and input epoch (`midi_subscribe`), and the learn UI's calls (`midi_*`). The host is
//! [`MidiHost`] (briefing: `midi/mod.rs`), which engine mode starts with the engine host and drops first
//! at shutdown (`mode.rs`); without it every command here answers an error.
//!
//! `input_send` is the UI's only way to the engine's commands and to the note router: the outbox
//! (`src/platform/index.ts`) sends one batch at a time, the next once this one settled, so the order it
//! queued a note, a slot pick and a looper press in is the order they run in here. An error means none
//! of the batch ran (the outbox may send it again); what native MIDI dropped of a batch that ran is the
//! answer ([`Dropped`]). `midi_subscribe` is synchronous too, so a document's subscribe runs in the order
//! the documents made them. The other `midi_*` commands are async (on the runtime's pool); the UI sends
//! them one at a time, in call order, and each only takes the MIDI lock briefly (the bindings are
//! written on the port thread). Every argument is the least the call needs: an epoch and items, an
//! action id and a track, a list revision and an index, a present port's id, the legacy list's text.

use tauri::ipc::Channel;

use super::midi::{router_command, ActionId, Dropped, ImportReport, MidiEvent, MidiHost};
use super::mode::engine;
use super::wire::{InputEvent, InputItem, WireCommand};

/// Run `f` on this launch's native MIDI; an error when it does not run.
fn midi<R>(f: impl FnOnce(&MidiHost) -> R) -> Result<R, String> {
    engine()?.with_midi(f)
}

/// `input_send`'s batch, in order, for the document of `epoch`: a run of engine commands through
/// [`MidiHost::ui_commands`] (input commands join the one ordered queue, settings go straight to the
/// engine), each input event through the router's own call. A note, a wheel, the note target or a panic
/// sent as an engine command refuses the whole batch before any of it runs. An input event of a
/// replaced document is refused there; its engine commands run, as they always did. The answer is the
/// first reason anything was dropped.
fn input_to(midi: &MidiHost, epoch: u64, items: Vec<InputItem>) -> Result<Option<Dropped>, String> {
    if let Some(command) = items.iter().find_map(|i| match i {
        InputItem::Engine(WireCommand(c)) if router_command(c) => Some(c),
        _ => None,
    }) {
        return Err(format!("{command:?} goes through the note router, not as an engine command"));
    }
    let mut dropped = None;
    let mut run = Vec::new();
    for item in items {
        let event = match item {
            InputItem::Engine(WireCommand(command)) => {
                run.push(command);
                continue;
            }
            InputItem::Input(event) => event,
        };
        let ran = midi.ui_commands(std::mem::take(&mut run))?;
        let input = match event {
            InputEvent::Note { owner, note, velocity, on } => midi.ui_note(epoch, owner, note, velocity, on),
            InputEvent::Blur => midi.ui_blur(epoch),
            InputEvent::SelectTarget { slot, target } => midi.select_target(epoch, slot, target),
            InputEvent::AllNotesOff => midi.all_notes_off(epoch),
        };
        dropped = dropped.or(ran).or(input);
    }
    let ran = midi.ui_commands(run)?;
    Ok(dropped.or(ran))
}

/// The UI's outbox batch (`InputItem`s: engine commands and note-source events, in order) of the
/// document `epoch` (its subscribe's). Synchronous: it runs on the main thread. Answers what was
/// dropped (`null`: nothing); an error means none of it ran.
#[tauri::command]
pub fn input_send(epoch: u64, items: Vec<InputItem>) -> Result<Option<Dropped>, String> {
    midi(|midi| input_to(midi, epoch, items))?
}

/// A WebView document subscribes `channel` to native MIDI's events, first thing in its boot, and gets
/// its input epoch. In the same step the older documents' holds are released, a pending learn is
/// cancelled, and `channel` replaces the last subscriber, unless a newer document subscribed already.
/// The first events are what a new listener draws from: the ports, the bindings, the learn's state, the
/// store's last problem, the held notes. Synchronous, so the documents' subscribes run in the order they
/// reached the app.
#[tauri::command]
pub fn midi_subscribe(channel: Channel<MidiEvent>) -> Result<u64, String> {
    midi(|midi| {
        midi.subscribe(Box::new(move |event| {
            // A closed page's channel fails until the next subscriber replaces it.
            let _ = channel.send(event);
        }))
    })
}

/// Learn the next CC or note-on onto `action` (on track `target` for a lane action; `null`: the
/// selected track). Answered by a `learning` event, then a `learned` one.
#[tauri::command]
pub async fn midi_learn(action: ActionId, target: Option<u8>) -> Result<(), String> {
    midi(|midi| midi.learn(action, target))
}

/// Stop listening; true when a learn was pending.
#[tauri::command]
pub async fn midi_cancel_learn() -> Result<bool, String> {
    midi(|midi| midi.cancel_learn())
}

/// Drop listed binding `index` of the list at `revision` (a `bindings` event's). False: the list changed
/// since, nothing was done (the same for every edit below).
#[tauri::command]
pub async fn midi_forget(revision: u64, index: usize) -> Result<bool, String> {
    midi(|midi| midi.forget(revision, index))?
}

/// Read listed binding `index`'s pedal as momentary (`on`) or latching.
#[tauri::command]
pub async fn midi_set_momentary(revision: u64, index: usize, on: bool) -> Result<bool, String> {
    midi(|midi| midi.set_momentary(revision, index, on))?
}

/// HOLD on or off for listed binding `index` (a momentary REC/DUB pedal only).
#[tauri::command]
pub async fn midi_set_hold(revision: u64, index: usize, on: bool) -> Result<bool, String> {
    midi(|midi| midi.set_hold(revision, index, on))?
}

/// The player assigns listed binding `index` to the present port `port_id` (a `ports` event's id).
#[tauri::command]
pub async fn midi_assign(revision: u64, index: usize, port_id: String) -> Result<bool, String> {
    midi(|midi| midi.assign(revision, index, &port_id))?
}

/// Import the web's list once (`lf.midiLearn` as stored; `"[]"` when it has none).
#[tauri::command]
pub async fn midi_import_legacy(json: String) -> Result<ImportReport, String> {
    midi(|midi| midi.import_legacy(&json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use lf_engine::{Command, Instrument, NoteTarget, TimedCommand};
    use serde_json::json;

    use super::super::midi::EngineSide;
    use super::super::RebuildHook;

    /// An engine host that records what it is sent, with a device running unless told otherwise.
    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<Command>>,
        stopped: std::sync::atomic::AtomicBool,
    }

    impl EngineSide for Recorder {
        fn send(&self, command: TimedCommand) -> Result<(), String> {
            self.sent.lock().unwrap().push(command.command);
            Ok(())
        }

        fn running(&self) -> bool {
            !self.stopped.load(std::sync::atomic::Ordering::Relaxed)
        }

        fn set_rebuild_hook(&self, _: Option<Arc<dyn RebuildHook>>) {}
    }

    /// A host and its document's epoch.
    fn rig() -> (MidiHost, Arc<Recorder>, u64) {
        let engine = Arc::new(Recorder::default());
        let midi = MidiHost::detached(engine.clone());
        let epoch = midi.subscribe(Box::new(|_| {}));
        (midi, engine, epoch)
    }

    fn take(engine: &Recorder) -> Vec<Command> {
        std::mem::take(&mut *engine.sent.lock().unwrap())
    }

    /// A batch as the IPC hands it to `input_send`.
    fn items(json: serde_json::Value) -> Vec<InputItem> {
        serde_json::from_value(json).unwrap()
    }

    // The outbox's one path: engine commands and input events run in the order they were queued, across
    // the two kinds; a replaced document's input events are dropped, its engine commands run.
    #[test]
    fn input_send_runs_engine_commands_and_input_events_in_order() {
        let (midi, engine, epoch) = rig();
        let batch = items(json!([
            { "input": { "note": { "owner": "pointer:1", "note": 60, "velocity": 127, "on": true } } },
            { "engine": { "SetBpm": 90 } },
            { "input": { "selectTarget": { "slot": null, "target": { "Builtin": "pad" } } } },
            { "engine": "Press" },
            { "engine": { "RecDub": 0 } },
            { "input": { "note": { "owner": "key:KeyA", "note": 62, "velocity": 127, "on": true } } },
            { "input": { "blur": null } },
            { "input": { "note": { "owner": "key:KeyS", "note": 64, "velocity": 127, "on": true } } },
            { "input": "allNotesOff" },
        ]));
        assert_eq!(input_to(&midi, epoch, batch), Ok(None));
        assert_eq!(
            take(&engine),
            [
                Command::NoteOn(60, 1.0),
                Command::SetBpm(90.0),
                Command::NoteOff(60),
                Command::SelectInstrument(NoteTarget::Builtin(Instrument::Pad)),
                Command::Press,
                Command::RecDub(0),
                Command::NoteOn(62, 1.0),
                Command::NoteOff(62),
                Command::NoteOn(64, 1.0),
                Command::NoteOff(64),
                Command::AllNotesOff,
            ]
        );
        let newer = midi.subscribe(Box::new(|_| {}));
        let late = items(json!([
            { "input": { "note": { "owner": "key:KeyA", "note": 62, "velocity": 127, "on": true } } },
            { "input": { "selectTarget": { "slot": 1, "target": { "Slot": 1 } } } },
            { "input": "allNotesOff" },
            { "engine": { "SetBpm": 100 } },
        ]));
        assert_eq!(input_to(&midi, epoch, late), Ok(None));
        assert_eq!(take(&engine), [Command::SetBpm(100.0)], "a replaced document's input events are dropped");
        assert!(newer > epoch);
    }

    // A note, a wheel, the note target or a panic sent as an engine command refuses the batch before any
    // of it runs, so the outbox may send it again without running anything twice.
    #[test]
    fn a_batch_with_a_router_command_runs_nothing() {
        let (midi, engine, epoch) = rig();
        for refused in [json!({ "NoteOn": [60, 0.5] }), json!({ "SelectInstrument": { "Slot": 0 } }), json!("AllNotesOff"), json!({ "PitchBend": 1.0 })] {
            let batch = items(json!([
                { "engine": "Press" },
                { "input": { "note": { "owner": "pointer:1", "note": 60, "velocity": 100, "on": true } } },
                { "engine": refused },
            ]));
            assert!(input_to(&midi, epoch, batch).is_err());
            assert_eq!(take(&engine), [], "nothing of a refused batch reached the engine");
        }
    }

    // A fresh press or note-on with no device is dropped and the batch says so; a release in it passes.
    #[test]
    fn input_send_answers_what_it_dropped() {
        let (midi, engine, epoch) = rig();
        let on = items(json!([{ "input": { "note": { "owner": "pointer:1", "note": 60, "velocity": 100, "on": true } } }]));
        assert_eq!(input_to(&midi, epoch, on), Ok(None));
        engine.stopped.store(true, std::sync::atomic::Ordering::Relaxed);
        let batch = items(json!([
            { "engine": "Press" },
            { "input": { "note": { "owner": "pointer:1", "note": 60, "velocity": 0, "on": false } } },
            { "engine": { "SetBpm": 80 } },
        ]));
        assert_eq!(input_to(&midi, epoch, batch), Ok(Some(Dropped::NoDevice)));
        assert_eq!(take(&engine), [Command::NoteOn(60, 100.0 / 127.0), Command::SetBpm(80.0)], "the setting went; the release waits in the queue for a device");
    }
}
