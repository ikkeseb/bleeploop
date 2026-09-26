//! OWNS: native MIDI for the engine (`docs/plans/native-engine.md` § Stage 4): the input ports (midir,
//! one connection per port, hot-plug polling), message parsing, the MIDI-learn bindings mirrored
//! from settings (`src/app/midi-actions.ts`), and what a message becomes: a note for the engine
//! (next block start) or a looper action stamped with its press frame (`super::FrameClock`). This doc
//! is the module's briefing.
//!
//! # Module map
//!
//! | Module | Owns | Ported from |
//! |---|---|---|
//! | this file | [`MidiHost`], the state the port callbacks share ([`Core`]), the UI events | `src/audio/midi.ts` (glue) |
//! | `parse` | bytes to `parse::Message`, and what is ignored | `midi.ts` `parseMidiMessage` |
//! | `bindings` | [`Binding`], learn capture, matching, momentary vs latching, consume-first | `src/app/midi-actions.ts`, `src/app/actions.ts` |
//! | `router` | per-owner note ownership, sustain, the wheels | `src/audio/input-router.ts` |
//! | `ports` | midir connections, the hot-plug poll and its diff, the port keys | `midi.ts` `attachInputs` |
//!
//! # Rules
//!
//! - **A message passes MIDI learn first.** A learned or bound message is consumed there and never
//!   reaches the router; only what learn leaves is played.
//! - **Notes go unstamped, actions stamped.** A note, a pedal and a wheel land at the next block start
//!   (`TimedCommand::frame` `None`), as the UI's gestures do; a bound action carries the frame its
//!   arrival maps to ([`FrameClock::press_frame`], the arrival taken on the callback's entry).
//! - **While no device runs, only releases reach the engine.** Its command ring drains only in the
//!   callback, so a bound action or a note-on sent then would fire at the next open (the looper
//!   recording by itself; note-ons piling up until a note-off no longer fits). Both are dropped while
//!   the clock has no stamp; a note-on before it reaches the router, so no note is held. The wheels
//!   wait in the router, and the engine hears where they ended with the first message once a device
//!   runs, so a sweep cannot fill the ring ahead of a note-off. Releases go through, so a note held
//!   before the stop still gets its note-off.
//! - **One lock orders everything.** The router, MIDI learn and the port table sit in one `Mutex` the
//!   port callbacks and the poller take; commands go to the sink under it, so the engine sees them in
//!   the order the router decided them. None of these threads is an audio thread.
//! - **The router follows the note target.** A target switch or a panic goes through
//!   [`MidiHost::select_instrument`] / [`MidiHost::all_notes_off`], which forget the held notes as the
//!   web's `allNotesOff` did. Sent past them, a key held across the switch would keep another port's
//!   strike of that note silent.
//! - **A port that goes away releases what it held**, its pedal and its wheels, as a Web MIDI
//!   disconnect does; the UI hears of it in [`MidiEvent::Ports`].

pub mod bindings;
mod parse;
mod ports;
mod router;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use lf_engine::grid::Frame;
use lf_engine::{Command, NoteTarget, TimedCommand};
use serde::Serialize;

pub use bindings::{ActionId, Binding, Kind, PortKey};

use super::FrameClock;
use bindings::Learn;
use parse::parse;
use router::Router;

/// Where commands go: `EngineHost::send` in the app, a recorder in tests.
pub type Sink = Arc<dyn Fn(TimedCommand) -> Result<(), String> + Send + Sync>;

/// What the UI hears from native MIDI.
#[derive(Clone, Debug, PartialEq)]
pub enum MidiEvent {
    /// A learn captured this binding (the learn is over).
    Learned(Binding),
    /// The bindings changed here (a learn, or a pedal read as momentary): the list to persist.
    Bindings(Vec<Binding>),
    /// A GO LIVE binding fired: the plugin host's to run.
    GoLive,
    /// The port list changed; `gone` names the ports that went away (their held notes were released).
    Ports { ports: Vec<PortInfo>, gone: Vec<String> },
}

/// A present input port.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortInfo {
    pub name: String,
    pub occurrence: u32,
    /// False while it could not be opened (another program holds it); retried every poll.
    pub open: bool,
}

/// Counters for the DEV probe; both stay 0 in a clean run with a device open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MidiDiag {
    /// Commands the sink refused (no device open, or the engine's ring full).
    pub failed_sends: u64,
    /// Panics caught in a port callback (the message is dropped; midir calls it from an
    /// `extern "system"` WinMM callback, where an unwind would abort the process).
    pub panics: u64,
}

/// A present port and its connection (`None` while it is not open).
pub(crate) struct PortEntry {
    pub(crate) key: PortKey,
    pub(crate) conn: Option<u32>,
}

struct State {
    router: Router,
    learn: Learn,
    ports: Vec<PortEntry>,
    /// The port list the UI last heard.
    published: Vec<PortInfo>,
}

/// What the port callbacks, the poller and [`MidiHost`] share.
pub(crate) struct Core {
    state: Mutex<State>,
    events: Mutex<Vec<MidiEvent>>,
    sink: Sink,
    clock: FrameClock,
    failed_sends: AtomicU64,
    panics: AtomicU64,
}

impl Core {
    fn new(sink: Sink, clock: FrameClock) -> Core {
        Core {
            state: Mutex::new(State { router: Router::default(), learn: Learn::default(), ports: Vec::new(), published: Vec::new() }),
            events: Mutex::new(Vec::new()),
            sink,
            clock,
            failed_sends: AtomicU64::new(0),
            panics: AtomicU64::new(0),
        }
    }

    /// A panic on a port callback is caught, so the lock may be poisoned: the state stays usable.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, event: MidiEvent) {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push(event);
    }

    fn send(&self, frame: Option<Frame>, command: Command) {
        if (self.sink)(TimedCommand { frame, command }).is_err() {
            self.failed_sends.fetch_add(1, Relaxed);
        }
    }

    /// A port callback: `bytes` arrived on connection `conn` at `at`.
    pub(crate) fn message(&self, conn: u32, at: Instant, bytes: &[u8]) {
        let Some(message) = parse(bytes) else { return };
        if catch_unwind(AssertUnwindSafe(|| self.route(conn, at, message))).is_err() {
            self.panics.fetch_add(1, Relaxed);
        }
    }

    fn route(&self, conn: u32, at: Instant, message: parse::Message) {
        let mut state = self.lock();
        let State { router, learn, ports, .. } = &mut *state;
        // A connection already released (its port went away) has no owner left.
        let Some(port) = ports.iter().find(|p| p.conn == Some(conn)) else { return };
        let owner = (conn, message.channel());
        // `None` while no device runs: only releases go through (the rules above).
        let stamp = self.clock.press_frame(at);
        let mut out = |command| self.send(None, command);
        router.hold_wheels(stamp.is_none(), &mut out);
        let outcome = learn.consume(&port.key, &message, at);
        if let Some(b) = outcome.learned {
            if b.kind == Kind::Cc {
                router.release_controller(owner, b.number, &mut out);
            }
            self.emit(MidiEvent::Learned(b));
        }
        if outcome.changed {
            self.emit(MidiEvent::Bindings(learn.bindings.clone()));
        }
        match outcome.fire.map(ActionId::engine_action) {
            Some(Some(action)) => {
                if stamp.is_some() {
                    self.send(stamp, Command::Action(action));
                }
            }
            Some(None) => self.emit(MidiEvent::GoLive),
            None => {}
        }
        let starts = matches!(message, parse::Message::NoteOn { .. });
        if !outcome.consumed && (stamp.is_some() || !starts) {
            router.message(owner, message, &mut out);
        }
    }

    /// Connection `conn` is closing: release what it held and stop taking its messages. Returns the
    /// port's name when it was in the table.
    pub(crate) fn port_gone(&self, conn: u32) -> Option<String> {
        let mut state = self.lock();
        let State { router, ports, .. } = &mut *state;
        let mut out = |command| self.send(None, command);
        router.hold_wheels(self.clock.press_frame(Instant::now()).is_none(), &mut out);
        router.release_port(conn, &mut out);
        let entry = ports.iter_mut().find(|p| p.conn == Some(conn))?;
        entry.conn = None;
        Some(entry.key.name.clone())
    }

    pub(crate) fn set_ports(&self, ports: Vec<PortEntry>) {
        self.lock().ports = ports;
    }

    /// Tell the UI when the port list changed, or a port went away.
    pub(crate) fn publish_ports(&self, gone: Vec<String>) {
        let mut state = self.lock();
        let now = port_infos(&state.ports);
        if now != state.published || !gone.is_empty() {
            state.published = now.clone();
            self.emit(MidiEvent::Ports { ports: now, gone });
        }
    }
}

fn port_infos(ports: &[PortEntry]) -> Vec<PortInfo> {
    ports.iter().map(|p| PortInfo { name: p.key.name.clone(), occurrence: p.key.occurrence, open: p.conn.is_some() }).collect()
}

/// Native MIDI input for the engine. Dropping it closes every port (their notes released) and joins the
/// poller.
pub struct MidiHost {
    core: Arc<Core>,
    /// Hanging up stops the poller.
    stop: Option<Sender<()>>,
    poller: Option<JoinHandle<()>>,
}

impl MidiHost {
    /// Start listening: every present input port opens now, and ports that come and go are followed
    /// every second (`ports::POLL`). Commands go to `sink`; actions are stamped on `clock`.
    pub fn start(sink: Sink, clock: FrameClock) -> MidiHost {
        let core = Arc::new(Core::new(sink, clock));
        let (stop, stopped) = mpsc::channel();
        let poller = {
            let core = core.clone();
            std::thread::Builder::new()
                .name("lf-midi-ports".into())
                .spawn(move || ports::run(core, stopped))
                .map_err(|e| log::error!("[midi] could not start the port poller: {e}"))
                .ok()
        };
        MidiHost { core, stop: Some(stop), poller }
    }

    /// Replace the bindings (the settings handed over, or one forgotten).
    pub fn set_bindings(&self, bindings: Vec<Binding>) {
        self.core.lock().learn.set_bindings(bindings);
    }

    pub fn bindings(&self) -> Vec<Binding> {
        self.core.lock().learn.bindings.clone()
    }

    /// Learn the next CC or note-on, from any port, onto `action` (answered by [`MidiEvent::Learned`]).
    pub fn learn(&self, action: ActionId) {
        self.core.lock().learn.learning = Some(action);
    }

    /// Stop listening. True when a learn was pending.
    pub fn cancel_learn(&self) -> bool {
        self.core.lock().learn.learning.take().is_some()
    }

    pub fn learning(&self) -> Option<ActionId> {
        self.core.lock().learn.learning
    }

    /// The present input ports, in the system's order.
    pub fn ports(&self) -> Vec<PortInfo> {
        port_infos(&self.core.lock().ports)
    }

    /// Move the notes to `target` (`Command::SelectInstrument`); the router forgets the notes held on the
    /// target it leaves, which the engine releases.
    pub fn select_instrument(&self, target: NoteTarget) -> Result<(), String> {
        let mut state = self.core.lock();
        state.router.all_notes_off();
        (self.core.sink)(TimedCommand { frame: None, command: Command::SelectInstrument(target) })
    }

    /// Panic: every note off (`Command::AllNotesOff`), and the router forgets them.
    pub fn all_notes_off(&self) -> Result<(), String> {
        let mut state = self.core.lock();
        state.router.all_notes_off();
        (self.core.sink)(TimedCommand { frame: None, command: Command::AllNotesOff })
    }

    /// Move the pending UI events into `out`.
    pub fn drain_events(&self, out: &mut Vec<MidiEvent>) {
        out.append(&mut self.core.events.lock().unwrap_or_else(|e| e.into_inner()));
    }

    pub fn diag(&self) -> MidiDiag {
        MidiDiag { failed_sends: self.core.failed_sends.load(Relaxed), panics: self.core.panics.load(Relaxed) }
    }
}

impl Drop for MidiHost {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(poller) = self.poller.take() {
            let _ = poller.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use lf_engine::{Action, Instrument};

    use super::bindings::RELEASE;

    const A: u32 = 1;
    const B: u32 = 2;

    /// A host with no poller and two ports, "Probe a" and "Probe b" on connections [`A`] and [`B`], as
    /// the browser probes' two virtual Web MIDI inputs, and a device running (its clock stamped).
    struct Rig {
        host: MidiHost,
        sent: Arc<Mutex<Vec<TimedCommand>>>,
        clock: FrameClock,
        t: Instant,
    }

    impl Rig {
        fn new() -> Rig {
            let r = Rig::stopped();
            r.clock.publish(r.t, 0, 256, 48_000);
            r
        }

        /// No device runs: the clock has no stamp.
        fn stopped() -> Rig {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let sink: Sink = {
                let sent = sent.clone();
                Arc::new(move |c| {
                    sent.lock().unwrap().push(c);
                    Ok(())
                })
            };
            let clock = FrameClock::new();
            let core = Arc::new(Core::new(sink, clock.clone()));
            core.set_ports(vec![
                PortEntry { key: PortKey { name: "Probe a".into(), occurrence: 0 }, conn: Some(A) },
                PortEntry { key: PortKey { name: "Probe b".into(), occurrence: 0 }, conn: Some(B) },
            ]);
            Rig { host: MidiHost { core, stop: None, poller: None }, sent, clock, t: Instant::now() }
        }

        /// Messages arriving in one burst, at the rig's current time.
        fn send(&self, conn: u32, messages: &[[u8; 3]]) {
            for m in messages {
                self.host.core.message(conn, self.t, m);
            }
        }

        fn wait(&mut self, d: Duration) {
            self.t += d;
        }

        fn take(&self) -> Vec<Command> {
            self.sent.lock().unwrap().drain(..).map(|c| c.command).collect()
        }

        /// The actions sent since the last take (everything else sent is dropped).
        fn actions(&self) -> Vec<Action> {
            self.take().into_iter().filter_map(|c| if let Command::Action(a) = c { Some(a) } else { None }).collect()
        }

        fn events(&self) -> Vec<MidiEvent> {
            let mut out = Vec::new();
            self.host.drain_events(&mut out);
            out
        }

        /// Learn `messages` (one burst from `conn`) onto `action`.
        fn learn(&self, action: ActionId, conn: u32, messages: &[[u8; 3]]) {
            self.host.learn(action);
            self.send(conn, messages);
        }
    }

    fn on(note: u8, velocity: u8) -> Command {
        Command::NoteOn(note, f32::from(velocity) / 127.0)
    }

    // probe midi-learn "a learning tap binds the CC as momentary, ends the learn and runs nothing";
    // midi-actions.ts consume (learn capture, then the tail inside RELEASE_MS).
    #[test]
    fn a_learning_tap_binds_the_cc_as_momentary_ends_the_learn_and_runs_nothing() {
        let r = Rig::new();
        r.learn(ActionId::RecDub, A, &[[0xb0, 20, 127], [0xb0, 20, 0]]);
        assert_eq!(r.take(), []);
        assert_eq!(r.host.learning(), None);
        let b = Binding {
            port_name: "Probe a".into(),
            occurrence: 0,
            channel: 0,
            kind: Kind::Cc,
            number: 20,
            action: ActionId::RecDub,
            press_high: true,
            momentary: true,
        };
        assert_eq!(r.host.bindings(), [b.clone()]);
        let learned = Binding { momentary: false, ..b.clone() };
        assert_eq!(r.events(), [MidiEvent::Learned(learned.clone()), MidiEvent::Bindings(vec![learned]), MidiEvent::Bindings(vec![b])]);
    }

    // probe midi-learn "after a reload the learned CC records the selected track, and its release runs
    // nothing": a bound press is Command::Action on the selected lane, stamped with the frame its arrival
    // maps to (frame_clock.rs press_frame).
    #[test]
    fn a_bound_press_sends_its_action_stamped_with_the_press_frame_and_its_release_nothing() {
        let mut r = Rig::new();
        r.learn(ActionId::RecDub, A, &[[0xb0, 20, 127], [0xb0, 20, 0]]);
        r.wait(RELEASE * 2);
        r.clock.publish(r.t, 48_000, 256, 48_000);
        r.wait(Duration::from_millis(1));
        r.send(A, &[[0xb0, 20, 127]]);
        let frame = r.clock.press_frame(r.t);
        assert_eq!(frame, Some(48_000 + 48 + 256));
        assert_eq!(*r.sent.lock().unwrap(), [TimedCommand { frame, command: Command::Action(Action::RecDub) }]);
        r.take();
        r.send(A, &[[0xb0, 20, 0]]);
        assert_eq!(r.take(), []);
    }

    // While no device runs nothing drains the engine's ring: a bound press and a note-on sent then would
    // fire at the next open. Both are dropped (the note-on never held); a note held from before the
    // stop still gets its note-off, and learn still captures.
    #[test]
    fn with_no_device_running_actions_and_note_ons_are_dropped_and_note_offs_pass() {
        let mut r = Rig::new();
        r.learn(ActionId::RecDub, A, &[[0xb0, 20, 127], [0xb0, 20, 0]]);
        r.wait(RELEASE * 2);
        r.send(A, &[[0x90, 60, 100], [0xb0, 64, 127], [0x90, 62, 100], [0x80, 62, 0]]);
        assert_eq!(r.take(), [on(60, 100), on(62, 100)], "the pedal holds 62");

        r.clock.clear();
        r.send(A, &[[0xb0, 20, 127], [0xb0, 20, 0], [0x90, 64, 100], [0x90, 60, 90]]);
        assert_eq!(r.take(), [], "the bound press and the note-ons are dropped");
        assert_eq!(r.host.core.lock().router.held(), [60], "the dropped note-ons hold nothing");
        r.send(A, &[[0x80, 64, 0], [0x80, 60, 0], [0xb0, 64, 0]]);
        assert_eq!(r.take(), [Command::NoteOff(62), Command::NoteOff(60)], "the held and sustained notes let go");
        r.host.learn(ActionId::Undo);
        r.send(A, &[[0xb0, 30, 127]]);
        assert_eq!(r.host.learning(), None, "learn captured");
        assert!(r.host.bindings().iter().any(|b| b.number == 30));

        // A device again: as before.
        r.clock.publish(r.t, 0, 256, 48_000);
        r.wait(RELEASE * 2);
        r.send(A, &[[0xb0, 20, 127], [0x90, 64, 100]]);
        assert_eq!(r.take(), [Command::Action(Action::RecDub), on(64, 100)]);

        // A rig that never had a device: nothing sent, nothing held.
        let r = Rig::stopped();
        r.send(A, &[[0x90, 60, 100], [0x80, 60, 0]]);
        assert_eq!(r.take(), []);
        assert!(r.host.core.lock().router.held().is_empty());
    }

    // probe midi-learn "Esc cancels a learn", "a CC after a cancelled learn binds nothing"
    // (midi-actions.ts cancelLearn: true when a learn was pending).
    #[test]
    fn a_cancelled_learn_binds_nothing() {
        let r = Rig::new();
        r.host.learn(ActionId::Undo);
        assert!(r.host.cancel_learn());
        assert!(!r.host.cancel_learn(), "nothing pending the second time");
        r.send(A, &[[0xb0, 30, 127], [0xb0, 30, 0]]);
        assert!(r.host.bindings().is_empty());
        assert_eq!(r.events(), []);
    }

    // probe midi-learn "each momentary press fires once, on the press; the learning tap none", "each
    // latching press fires once, the learning press none", "a reversed-polarity pedal fires once per
    // press, on the press (0)" (midi-actions.ts consume: the footswitch rule).
    #[test]
    fn momentary_latching_and_reversed_footswitches_each_fire_once_per_press() {
        let mut r = Rig::new();
        // Momentary: the learning tap sends both sides.
        r.learn(ActionId::NextTrack, A, &[[0xb0, 21, 127], [0xb0, 21, 0]]);
        let mut fired = Vec::new();
        for m in [[0xb0, 21, 127], [0xb0, 21, 0], [0xb0, 21, 127], [0xb0, 21, 0], [0xb0, 21, 127], [0xb0, 21, 0]] {
            r.send(A, &[m]);
            fired.push(r.actions().len());
        }
        assert_eq!(fired, [1, 0, 1, 0, 1, 0]);

        // Latching: the learning press sends 127 and nothing follows it inside the window.
        r.learn(ActionId::NextTrack, A, &[[0xb0, 22, 127]]);
        r.wait(Duration::from_millis(1300));
        let mut fired = Vec::new();
        for m in [[0xb0, 22, 0], [0xb0, 22, 127], [0xb0, 22, 0], [0xb0, 22, 127]] {
            r.send(A, &[m]);
            fired.push(r.actions().len());
        }
        assert_eq!(fired, [1, 1, 1, 1]);
        assert!(!r.host.bindings().iter().find(|b| b.number == 22).unwrap().momentary);

        // Reversed polarity (0 on press, 127 on release), on port b, channel 3.
        r.learn(ActionId::NextTrack, B, &[[0xb2, 23, 0], [0xb2, 23, 127]]);
        let mut fired = Vec::new();
        for m in [[0xb2, 23, 0], [0xb2, 23, 127], [0xb2, 23, 0], [0xb2, 23, 127]] {
            r.send(B, &[m]);
            fired.push(r.actions().len());
        }
        assert_eq!(fired, [1, 0, 1, 0]);
    }

    // midi-actions.ts consume: the release must come inside RELEASE_MS of the learning press; later, the
    // pedal reads as latching and its release fires too.
    #[test]
    fn a_release_after_the_learn_window_reads_as_latching() {
        let mut r = Rig::new();
        r.learn(ActionId::PlayAll, A, &[[0xb0, 25, 127]]);
        r.wait(RELEASE);
        r.send(A, &[[0xb0, 25, 0]]);
        assert_eq!(r.actions(), [Action::PlayAll]);
        assert!(!r.host.bindings()[0].momentary);
    }

    // probe midi-learn "unlearned CC64/1/123 reach the router as before" (midi.ts parseMidiMessage).
    #[test]
    fn unlearned_pedal_wheel_and_all_notes_off_reach_the_router() {
        let r = Rig::new();
        r.send(A, &[[0x90, 60, 100], [0xb0, 64, 127], [0x80, 60, 0], [0xb0, 1, 64], [0xb0, 123, 0], [0xb0, 64, 0]]);
        assert_eq!(r.take(), [on(60, 100), Command::Modulation(64.0 / 127.0), Command::NoteOff(60)]);
    }

    // probe midi-learn "CC 120/123 are never learned: they reach the router and the learn keeps
    // listening" and "a note-off (0x80 or velocity 0) is never learned: it reaches the router and the
    // learn keeps listening" (midi-actions.ts consume's learn guard).
    #[test]
    fn channel_mode_ccs_and_note_offs_never_start_a_learn() {
        let r = Rig::new();
        r.send(A, &[[0x90, 61, 100], [0x90, 62, 100], [0x90, 65, 100]]);
        r.take();
        r.host.learn(ActionId::StopAll);
        r.send(A, &[[0xb0, 120, 0], [0x80, 61, 0], [0x90, 62, 0], [0xb0, 123, 0]]);
        assert_eq!(r.take(), [Command::NoteOff(61), Command::NoteOff(62), Command::NoteOff(65)]);
        assert_eq!(r.host.learning(), Some(ActionId::StopAll), "the learn is still listening");
        assert!(r.host.bindings().is_empty());
    }

    // probe midi-learn "learning CC64 lets go of the pedal held down on its port", "a CC learned onto 64
    // must not sustain", "the learned CC64 press ran its action once, on the press (0)", "the other port's
    // unlearned CC64 still sustains" (midi.ts releaseController).
    #[test]
    fn a_pedal_learned_while_down_lets_go_and_never_sustains_again() {
        let r = Rig::new();
        r.send(B, &[[0xb0, 64, 127], [0x90, 50, 100], [0x80, 50, 0]]);
        assert_eq!(r.take(), [on(50, 100)], "port b's pedal holds its note");
        r.learn(ActionId::NextTrack, B, &[[0xb0, 64, 0], [0xb0, 64, 127]]);
        assert_eq!(r.take(), [Command::NoteOff(50)], "the learn lets the held pedal go");

        r.send(B, &[[0x90, 67, 100], [0x80, 67, 0]]);
        assert_eq!(r.take(), [on(67, 100), Command::NoteOff(67)], "a learned CC64 never sustains");
        r.send(B, &[[0xb0, 64, 0]]);
        assert_eq!(r.actions(), [Action::NextTrack]);
        r.send(B, &[[0xb0, 64, 127]]);
        assert_eq!(r.actions(), [], "it reads as reversed: its release fires nothing");

        r.send(A, &[[0xb0, 64, 127], [0x90, 67, 100], [0x80, 67, 0]]);
        assert_eq!(r.take(), [on(67, 100)], "port a's unlearned CC64 still sustains");
    }

    // probe midi-learn "learning CC1 hands the vibrato back to the wheel moved before it".
    #[test]
    fn a_learned_mod_wheel_hands_the_vibrato_back() {
        let r = Rig::new();
        r.send(B, &[[0xb0, 1, 50]]);
        r.send(A, &[[0xb5, 1, 100]]);
        r.take();
        r.learn(ActionId::StopAll, A, &[[0xb5, 1, 90]]);
        assert_eq!(r.take(), [Command::Modulation(50.0 / 127.0)]);
    }

    // probe midi-learn "a learned note must not sound", "a learned note is never held", "neither the
    // learned note-on nor its note-off reaches the router", "two presses of the learned note stepped back
    // twice, each on its note-on", "an unlearned note on the same channel sounds".
    #[test]
    fn a_learned_note_runs_its_action_and_never_sounds() {
        let mut r = Rig::new();
        r.learn(ActionId::PrevTrack, B, &[[0x91, 60, 100], [0x81, 60, 0]]);
        assert_eq!(r.take(), []);
        r.wait(RELEASE * 2);
        r.send(B, &[[0x91, 60, 100]]);
        assert_eq!(r.take(), [Command::Action(Action::PrevTrack)]);
        r.send(B, &[[0x81, 60, 0], [0x91, 60, 100], [0x91, 60, 0]]);
        assert_eq!(r.take(), [Command::Action(Action::PrevTrack)]);
        assert!(r.host.core.lock().router.held().is_empty());
        r.send(B, &[[0x91, 62, 100]]);
        assert_eq!(r.take(), [on(62, 100)]);
    }

    // probe midi-learn "a forgotten CC64 leaves the list and sustains again" (midi-actions.ts forget).
    #[test]
    fn a_forgotten_binding_hands_its_message_back_to_the_play_path() {
        let r = Rig::new();
        r.learn(ActionId::NextTrack, B, &[[0xb0, 64, 127]]);
        r.host.set_bindings(Vec::new());
        r.send(B, &[[0xb0, 64, 127], [0x90, 60, 100], [0x80, 60, 0]]);
        assert_eq!(r.take(), [on(60, 100)], "sustained");
        r.send(B, &[[0xb0, 64, 0]]);
        assert_eq!(r.take(), [Command::NoteOff(60)]);
    }

    // actions.ts goLive: GO LIVE stays with the plugin host, so its binding reaches the UI, not the engine.
    #[test]
    fn a_go_live_binding_reaches_the_ui_not_the_engine() {
        let mut r = Rig::new();
        r.learn(ActionId::GoLive, A, &[[0x90, 36, 100]]);
        r.events();
        r.wait(RELEASE * 2);
        r.send(A, &[[0x90, 36, 100]]);
        assert_eq!(r.take(), []);
        assert_eq!(r.events(), [MidiEvent::GoLive]);
    }

    // probe midi-note-ownership "unplugging one port must preserve the other port"; midi.ts attachInputs
    // releases a vanished port's notes and pedal, and a closed port's late message finds no owner.
    #[test]
    fn a_port_that_goes_away_releases_its_notes_and_its_late_messages_do_nothing() {
        let r = Rig::new();
        r.send(A, &[[0xb0, 64, 127], [0x90, 67, 100], [0x80, 67, 0], [0x90, 60, 100]]);
        r.send(B, &[[0x90, 62, 100]]);
        r.take();
        assert_eq!(r.host.core.port_gone(A), Some("Probe a".into()));
        assert_eq!(r.take(), [Command::NoteOff(67), Command::NoteOff(60)]);
        r.send(A, &[[0x90, 61, 100]]);
        assert_eq!(r.take(), []);
        assert_eq!(r.host.core.lock().router.held(), [62]);
        assert_eq!(r.host.ports()[0], PortInfo { name: "Probe a".into(), occurrence: 0, open: false });
    }

    // midi.ts attachInputs names each vanished port (its toast); natively the UI hears Ports with `gone`.
    #[test]
    fn the_ui_hears_a_port_list_change_once_and_every_departure() {
        let r = Rig::new();
        r.host.core.publish_ports(Vec::new());
        r.host.core.publish_ports(Vec::new());
        let ports = vec![PortInfo { name: "Probe a".into(), occurrence: 0, open: true }, PortInfo { name: "Probe b".into(), occurrence: 0, open: true }];
        assert_eq!(r.events(), [MidiEvent::Ports { ports: ports.clone(), gone: vec![] }]);
        r.host.core.port_gone(B);
        r.host.core.set_ports(vec![PortEntry { key: PortKey { name: "Probe a".into(), occurrence: 0 }, conn: Some(A) }]);
        r.host.core.publish_ports(vec!["Probe b".into()]);
        assert_eq!(r.events(), [MidiEvent::Ports { ports: ports[..1].to_vec(), gone: vec!["Probe b".into()] }]);
    }

    // probe instrument-routing "switching slots releases the sustained note exactly once": the engine
    // releases it on SelectInstrument (input-router.ts allNotesOff on the swap), so pedal-up adds nothing.
    #[test]
    fn a_target_switch_through_the_host_is_sent_and_the_router_forgets_its_notes() {
        let r = Rig::new();
        r.send(A, &[[0xb0, 64, 127], [0x90, 64, 100], [0x80, 64, 0]]);
        r.take();
        r.host.select_instrument(NoteTarget::Builtin(Instrument::Pad)).unwrap();
        assert_eq!(r.take(), [Command::SelectInstrument(NoteTarget::Builtin(Instrument::Pad))]);
        r.send(A, &[[0xb0, 64, 0]]);
        assert_eq!(r.take(), []);
        r.host.all_notes_off().unwrap();
        assert_eq!(r.take(), [Command::AllNotesOff]);
    }

    // A wheel sweep while no device runs would fill the engine's ring ahead of the note-off: only the
    // release goes out, and the wheels catch up with the first message once a device runs.
    #[test]
    fn with_no_device_a_wheel_sweep_holds_nothing_up_and_the_wheels_catch_up_after() {
        let r = Rig::new();
        r.send(A, &[[0x90, 60, 100]]);
        assert_eq!(r.take(), [on(60, 100)]);
        r.clock.clear();
        for v in 0..=127u8 {
            r.send(A, &[[0xb0, 1, v], [0xe0, 0, v]]);
        }
        r.send(A, &[[0xe0, 0x00, 0x50], [0xb0, 1, 90], [0x80, 60, 0]]);
        assert_eq!(r.take(), [Command::NoteOff(60)], "only the release");
        r.clock.publish(r.t, 0, 256, 48_000);
        r.send(A, &[[0x90, 62, 100]]);
        let bend = (f64::from(0x50u16 << 7) - 8192.0) / 8192.0 * 2.0;
        assert_eq!(r.take(), [Command::PitchBend(bend), Command::Modulation(90.0 / 127.0), on(62, 100)]);
    }

    // The sink refuses (no device open): the router carries on and each refusal is counted.
    #[test]
    fn a_refused_command_is_counted() {
        let sink: Sink = Arc::new(|_| Err("no audio device is open".into()));
        let clock = FrameClock::new();
        clock.publish(Instant::now(), 0, 256, 48_000);
        let core = Arc::new(Core::new(sink, clock));
        core.set_ports(vec![PortEntry { key: PortKey { name: "Probe a".into(), occurrence: 0 }, conn: Some(A) }]);
        let host = MidiHost { core, stop: None, poller: None };
        host.core.message(A, Instant::now(), &[0x90, 60, 100]);
        host.core.message(A, Instant::now(), &[0x80, 60, 0]);
        assert_eq!(host.diag(), MidiDiag { failed_sends: 2, panics: 0 });
    }
}
