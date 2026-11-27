//! Page input uses the same ownership and bounded command queue as native MIDI on Windows.
use lf_engine::{Command, TimedCommand};
use serde_json::{json, Value};
use tauri::ipc::Channel;

use super::wire::{InputEvent, InputItem, WireCommand};
use super::EngineHost;
use super::{
    queue::{Out, Queue, Refused},
    router::{Owner, Router},
    ui_route::{ui_route, UiRoute},
};

#[derive(Default)]
pub(super) struct Input {
    router: Router,
    queue: Queue,
    channel: Option<Channel<Value>>,
    origin: Option<f64>,
    epochs: u64,
    heard: Option<u64>,
}

impl Input {
    pub(super) fn subscribe(
        &mut self,
        host: &EngineHost,
        origin: f64,
        channel: Channel<Value>,
        running: bool,
    ) -> u64 {
        self.epochs += 1;
        if self.origin.is_none_or(|o| origin >= o) && origin.is_finite() {
            let mut releases = Vec::new();
            self.router.ui_epoch(self.epochs, &mut releases);
            self.admit(host, &releases, running);
            self.channel = Some(channel);
            self.origin = Some(origin);
            self.heard = None;
            self.publish_held();
        }
        self.epochs
    }

    fn publish_held(&mut self) {
        let held = self.router.held().read();
        if self.heard != Some(held.changes) {
            if let Some(channel) = &self.channel {
                let notes: Vec<_> = (0..128).filter(|&n| held.contains(n)).collect();
                let _ = channel.send(json!({"held":{"changes":held.changes,"notes":notes}}));
            }
            self.heard = Some(held.changes);
        }
    }

    pub(super) fn rebuilt(&mut self, host: &EngineHost) {
        self.router.engine_rebuilt();
        let (fold, _) = self.queue.rebuild();
        for command in fold.commands() {
            let _ = host.send(TimedCommand {
                frame: None,
                command,
            });
        }
    }

    pub(super) fn drain(&mut self, host: &EngineHost) {
        self.queue.drain(&mut |command| host.send(command));
    }

    fn admit(&mut self, host: &EngineHost, out: &[Out], running: bool) -> Option<&'static str> {
        self.drain(host);
        if let Err(reason) = self.queue.admit(out, running) {
            self.router.attack_refused(out);
            return Some(match reason {
                Refused::NoDevice => "noDevice",
                Refused::Full => "full",
                Refused::Paused => "rebuilding",
            });
        }
        self.drain(host);
        None
    }

    pub(super) fn send(
        &mut self,
        host: &EngineHost,
        epoch: u64,
        items: Vec<InputItem>,
        running: bool,
    ) -> Result<Option<&'static str>, String> {
        if self.router.epoch() != epoch || epoch == 0 {
            return Ok(Some("stale"));
        }
        // The outbox may retry a rejected call, so refuse the whole batch before touching the router.
        for item in &items {
            match item {
                InputItem::Engine(WireCommand(c)) if ui_route(c) == UiRoute::Router => {
                    return Err(format!("{c:?} goes through the note router"))
                }
                InputItem::Engine(WireCommand(c))
                    if c.unit().is_some() || c.kind_toggle().is_some() =>
                {
                    return Err("plug-in chains are unavailable on JACK".into())
                }
                #[cfg(debug_assertions)]
                InputItem::Input(InputEvent::PickSynth { .. }) => {
                    return Err("plug-in chains are unavailable on JACK".into())
                }
                _ => {}
            }
        }
        let mut dropped = None;
        let mut out = Vec::new();
        for item in items {
            let result = match item {
                InputItem::Engine(WireCommand(command)) => match ui_route(&command) {
                    UiRoute::Router => unreachable!(),
                    UiRoute::Queue if command == Command::SetTuner(false) && !running => None,
                    UiRoute::Queue => {
                        out.push(Out::new(command));
                        continue;
                    }
                    route => {
                        let queued = self.admit(host, &out, running);
                        out.clear();
                        dropped = dropped.or(queued);
                        if route == UiRoute::Live && !running {
                            Some("noDevice")
                        } else {
                            host.send(TimedCommand {
                                frame: None,
                                command,
                            })
                            .err()
                            .map(|_| "full")
                        }
                    }
                },
                InputItem::Input(event) => {
                    let queued = self.admit(host, &out, running);
                    dropped = dropped.or(queued);
                    out.clear();
                    match event {
                        InputEvent::Blur => self.router.ui_blur(epoch, &mut out),
                        InputEvent::SelectTarget { slot, target } => {
                            self.router.select_target(slot, target, &mut out)
                        }
                        InputEvent::Note {
                            owner,
                            note,
                            velocity,
                            on,
                        } => {
                            let owner = Owner::Ui { epoch, id: owner };
                            if on {
                                self.router.note_on(&owner, note, velocity, &mut out);
                            } else {
                                self.router.note_off(&owner, note, &mut out);
                            }
                        }
                        InputEvent::AllNotesOff => {
                            self.router.release_all(&mut out);
                            out.push(Out::new(Command::AllNotesOff));
                        }
                        #[cfg(debug_assertions)]
                        InputEvent::PickSynth { .. } => unreachable!(),
                    }
                    self.admit(host, &out, running)
                }
            };
            dropped = dropped.or(result);
            out.clear();
        }
        let queued = self.admit(host, &out, running);
        self.publish_held();
        Ok(dropped.or(queued))
    }
}
