//! OWNS: native MIDI's Tauri commands: the UI's note-source input (`input_send`), the routing
//! `engine_send` takes through it ([`engine_send_to`]), the frontend epoch's hand-over (`host_init`:
//! [`ui_epoch`]), and the learn UI's calls and event channel (`midi_*`). The host is
//! [`MidiHost`] (briefing: `midi/mod.rs`), which engine mode starts with the engine host and drops
//! first at shutdown (`mode.rs`); without it every command here answers an error.
//!
//! `input_send` is synchronous, as `engine_send` is: both run on the main thread, where the IPC hands
//! requests over in order, so the UI's one outbox keeps a note, a slot pick and a looper press in the
//! order it sent them across the two. The `midi_*` commands are async (on the runtime's pool): none is
//! ordered against input, and each only takes the MIDI lock briefly (the bindings are written on the
//! port thread). Every argument is the least the call needs: an epoch and events, an action id and a
//! track, a listed index, a present port's id, the legacy list's text.

use tauri::ipc::Channel;

use super::midi::{ActionId, ImportReport, MidiEvent, MidiHost};
use super::mode::engine;
use super::wire::{InputEvent, WireCommand};

/// Run `f` on this launch's native MIDI; an error when it does not run.
fn midi<R>(f: impl FnOnce(&MidiHost) -> R) -> Result<R, String> {
    engine()?.with_midi(f)
}

/// `engine_send`'s batch through native MIDI ([`MidiHost::ui_commands`]): input commands join the one
/// ordered queue, settings go straight to the engine, and a note, a wheel, the note target or a panic
/// is refused (the rest of the batch with it), since those go through `input_send`.
pub(super) fn engine_send_to(midi: &MidiHost, commands: Vec<WireCommand>) -> Result<(), String> {
    midi.ui_commands(commands.into_iter().map(|c| c.0).collect())
}

/// `input_send`'s events, in order, for the document of `epoch`.
fn input_to(midi: &MidiHost, epoch: u64, events: Vec<InputEvent>) {
    for event in events {
        match event {
            InputEvent::Note { owner, note, velocity, on } => midi.ui_note(epoch, owner, note, velocity, on),
            InputEvent::Blur => midi.ui_blur(epoch),
            InputEvent::SelectTarget { slot, target } => midi.select_target(slot, target),
            InputEvent::AllNotesOff => midi.all_notes_off(),
        }
    }
}

/// A new WebView document (`host_init`, before it answers the epoch): the older documents' holds are
/// released and their late events refused. Nothing when native MIDI does not run.
pub fn ui_epoch(epoch: u32) {
    let _ = midi(|midi| midi.ui_epoch(u64::from(epoch)));
}

/// The UI's note-source events (pointers, PC keys, the window's blur, the slot pick, the panic) of the
/// document `epoch` (its `frontendEpoch`), in order, through the one note router. An event of a
/// replaced document is dropped there. Synchronous, as `engine_send` (above).
#[tauri::command]
pub fn input_send(epoch: u64, events: Vec<InputEvent>) -> Result<(), String> {
    midi(|midi| input_to(midi, epoch, events))
}

/// Subscribe `channel` to native MIDI's events (replacing the last subscriber: a reload subscribes
/// again). The first events are what a new listener draws from: the ports, the bindings, the learn's
/// state, the store's last problem, the held notes.
#[tauri::command]
pub async fn midi_subscribe(channel: Channel<MidiEvent>) -> Result<(), String> {
    midi(|midi| {
        midi.set_events(Box::new(move |event| {
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

/// Drop listed binding `index`.
#[tauri::command]
pub async fn midi_forget(index: usize) -> Result<(), String> {
    midi(|midi| midi.forget(index))?
}

/// Read listed binding `index`'s pedal as momentary (`on`) or latching.
#[tauri::command]
pub async fn midi_set_momentary(index: usize, on: bool) -> Result<(), String> {
    midi(|midi| midi.set_momentary(index, on))?
}

/// HOLD on or off for listed binding `index` (a momentary REC/DUB pedal only).
#[tauri::command]
pub async fn midi_set_hold(index: usize, on: bool) -> Result<(), String> {
    midi(|midi| midi.set_hold(index, on))?
}

/// The player assigns listed binding `index` to the present port `port_id` (a `ports` event's id).
#[tauri::command]
pub async fn midi_assign(index: usize, port_id: String) -> Result<(), String> {
    midi(|midi| midi.assign(index, &port_id))?
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

    /// An engine host with a device running that records what it is sent.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Command>>);

    impl EngineSide for Recorder {
        fn send(&self, command: TimedCommand) -> Result<(), String> {
            self.0.lock().unwrap().push(command.command);
            Ok(())
        }

        fn running(&self) -> bool {
            true
        }

        fn set_rebuild_hook(&self, _: Option<Arc<dyn RebuildHook>>) {}
    }

    fn rig() -> (MidiHost, Arc<Recorder>) {
        let engine = Arc::new(Recorder::default());
        (MidiHost::detached(engine.clone()), engine)
    }

    fn take(engine: &Recorder) -> Vec<Command> {
        std::mem::take(&mut *engine.0.lock().unwrap())
    }

    /// A batch as the IPC hands it to `engine_send`.
    fn batch(json: serde_json::Value) -> Vec<WireCommand> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn engine_send_refuses_a_note_and_passes_a_setting_and_a_looper_press() {
        let (midi, engine) = rig();
        assert!(engine_send_to(&midi, batch(json!([{ "NoteOn": [60, 0.5] }]))).is_err(), "a note goes through input_send");
        assert!(engine_send_to(&midi, batch(json!([{ "SelectInstrument": { "Slot": 0 } }]))).is_err(), "so does the note target");
        assert_eq!(take(&engine), [], "nothing of a refused batch reached the engine");
        engine_send_to(&midi, batch(json!([{ "SetBpm": 90 }, { "RecDub": 0 }]))).unwrap();
        assert_eq!(take(&engine), [Command::SetBpm(90.0), Command::RecDub(0)]);
    }

    #[test]
    fn input_events_reach_the_router_in_order_and_a_replaced_documents_are_dropped() {
        let (midi, engine) = rig();
        midi.ui_epoch(2);
        let events: Vec<InputEvent> = serde_json::from_value(json!([
            { "note": { "owner": "pointer:1", "note": 60, "velocity": 127, "on": true } },
            { "selectTarget": { "slot": null, "target": { "Builtin": "pad" } } },
            { "note": { "owner": "key:KeyA", "note": 62, "velocity": 127, "on": true } },
            { "blur": null },
            { "note": { "owner": "key:KeyS", "note": 64, "velocity": 127, "on": true } },
            "allNotesOff",
        ]))
        .unwrap();
        input_to(&midi, 2, events);
        assert_eq!(
            take(&engine),
            [
                Command::NoteOn(60, 1.0),
                Command::NoteOff(60),
                Command::SelectInstrument(NoteTarget::Builtin(Instrument::Pad)),
                Command::NoteOn(62, 1.0),
                Command::NoteOff(62),
                Command::NoteOn(64, 1.0),
                Command::NoteOff(64),
                Command::AllNotesOff,
            ]
        );
        input_to(&midi, 1, vec![InputEvent::Note { owner: "key:KeyA".into(), note: 62, velocity: 127, on: true }]);
        assert_eq!(take(&engine), [], "an event of a replaced document is dropped");
    }
}
