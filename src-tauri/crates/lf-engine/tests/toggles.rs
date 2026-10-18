//! The toggled settings (CLICK, END STOP, FIXED, RETAKE, AUTO REC, the three input sends, each slot's GO
//! LIVE) as engine
//! actions (`Action::Toggle`; `docs/ARCHITECTURE.md` § Decided: native MIDI, every toggle has one
//! owner): the engine switches a setting from the value it has when the press applies, so presses from
//! two producers (a pedal, a click on screen) each switch it once; FIXED, RETAKE and AUTO REC are
//! refused where `src/ui/looper/gates.ts` refuses them, with nothing switched; a toggle is a looper
//! press (it disarms a pending pedal CLEAR, refused or not); an input send's toggle never waits behind
//! a looper command held for a block job, as its setter did, while the others wait in order, as theirs
//! did; GO LIVE's toggle reaches the slots as `SetSlotLive` does (it passes a held command, splits the
//! slot render at its frame, and disarms no CLEAR: its press's `Press` does that); and every applied
//! command of a toggled setting is answered once, `Event::Toggled` at the value it left, whatever the
//! event ring dropped. A new guard, not a port: the web looper flipped its own copy of each setting.

mod common;

use common::{code, Opts, Rig};
use lf_engine::{Action, Command, Event, InputSend, LaneState, Refusal, Toggle};

fn press(t: Toggle) -> Command {
    Command::Action(Action::Toggle(t))
}

/// The `Toggled` events since event `mark`.
fn toggled_since(rig: &Rig, mark: usize) -> Vec<(Toggle, bool)> {
    rig.events[mark..]
        .iter()
        .filter_map(|e| match *e {
            Event::Toggled { toggle, on, .. } => Some((toggle, on)),
            _ => None,
        })
        .collect()
}

fn refusals_since(rig: &Rig, mark: usize) -> Vec<(u8, Refusal)> {
    rig.events[mark..]
        .iter()
        .filter_map(|e| match *e {
            Event::Refused { lane, reason, .. } => Some((lane, reason)),
            _ => None,
        })
        .collect()
}

fn value(rig: &Rig, t: Toggle) -> bool {
    rig.engine.toggles()[t.index()]
}

/// A playing one-bar loop on lane 0: the tempo is locked.
fn looping() -> Rig {
    let mut rig = Rig::new();
    rig.set_input(code);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    rig
}

#[test]
fn alternating_producers_each_switch_a_setting_once() {
    for t in Toggle::ALL {
        let mut rig = Rig::new();
        rig.advance(256);
        let mark = rig.events.len();
        // Off but for the rig's GO LIVE on slot A.
        let was = value(&rig, t);
        // A pedal, the UI, the pedal again: unstamped, each at its own block start, as the host sends them.
        for expected in [!was, was, !was] {
            rig.queue(press(t));
            rig.advance(128);
            assert_eq!(value(&rig, t), expected, "{t:?}");
        }
        assert_eq!(toggled_since(&rig, mark), [(t, !was), (t, was), (t, !was)], "{t:?}: one event per press, with the value it left");
        // Two producers in one block: two switches, back where it was, each reported.
        let mark = rig.events.len();
        rig.queue(press(t));
        rig.queue(press(t));
        rig.advance(128);
        assert_eq!(value(&rig, t), !was, "{t:?}");
        assert_eq!(toggled_since(&rig, mark), [(t, was), (t, !was)], "{t:?}");
    }
}

#[test]
fn a_setter_still_sets_outright_and_every_applied_one_is_answered() {
    let mut rig = Rig::new();
    rig.advance(256);
    let mark = rig.events.len();
    rig.press(Command::SetMetronome(true));
    rig.press(Command::SetMetronome(true));
    rig.press(Command::SetInputSend(InputSend::Ring, true));
    rig.advance(4800);
    assert!(rig.engine.clock().metronome() && rig.engine.input_fx().is_on(InputSend::Ring));
    assert_eq!(
        toggled_since(&rig, mark),
        [(Toggle::Click, true), (Toggle::Click, true), (Toggle::Send(InputSend::Ring), true)],
        "one answer per setter, the one that changed nothing included: the host counts them"
    );
    // A toggle after a setter switches from the setter's value.
    rig.press(press(Toggle::Click));
    assert!(!rig.engine.clock().metronome());
}

#[test]
fn fixed_retake_and_auto_rec_are_refused_while_a_take_records_and_nothing_switches() {
    let mut rig = Rig::new();
    rig.advance(256);
    rig.press(Command::SelectTrack(2));
    rig.press(Command::RecDub(0)); // the first take counts in: lane 0 records, armed
    assert_eq!(rig.state(0), LaneState::Recording);
    let mark = rig.events.len();
    for t in [Toggle::Fixed, Toggle::Retake, Toggle::AutoRec] {
        rig.press(press(t));
        assert!(!value(&rig, t), "{t:?} unchanged");
    }
    assert_eq!(
        refusals_since(&rig, mark),
        [(2, Refusal::FixedCapturing), (2, Refusal::RetakeCapturing), (2, Refusal::AutoRecCapturing)],
        "on the selected lane, with gates.ts's reasons"
    );
    assert!(toggled_since(&rig, mark).is_empty(), "a refused toggle reports no value");
    // The others always switch.
    for t in [Toggle::Click, Toggle::EndStop, Toggle::Send(InputSend::Echo)] {
        rig.press(press(t));
        assert!(value(&rig, t), "{t:?}");
    }
    assert_eq!(refusals_since(&rig, mark).len(), 3);
}

#[test]
fn fixed_is_refused_under_retake_over_a_loop_and_auto_rec_once_the_tempo_is_locked() {
    let mut rig = looping();
    assert!(rig.locked());
    let mark = rig.events.len();
    rig.press(press(Toggle::AutoRec));
    assert!(!value(&rig, Toggle::AutoRec));
    rig.press(press(Toggle::Fixed));
    assert!(value(&rig, Toggle::Fixed), "FIXED switches over a loop without RETAKE");
    rig.press(press(Toggle::Retake));
    rig.press(press(Toggle::Fixed));
    assert!(value(&rig, Toggle::Fixed), "and not under RETAKE");
    assert_eq!(refusals_since(&rig, mark), [(0, Refusal::AutoRecLocked), (0, Refusal::FixedRetake)]);
    assert_eq!(Refusal::FixedRetake.text(), "RETAKE is on, so FIXED is ignored");
    assert_eq!(Refusal::AutoRecLocked.text(), "AUTO REC starts a first take, clear all to use it");
}

#[test]
fn a_toggle_on_a_named_lane_acts_as_one_on_none() {
    let mut rig = Rig::new();
    rig.advance(256);
    rig.press(Command::SelectTrack(1));
    rig.press(Command::RecDub(1));
    let mark = rig.events.len();
    rig.press(Command::ActionOn(4, Action::Toggle(Toggle::Click)));
    assert!(rig.engine.clock().metronome(), "the lane means nothing to a toggle");
    rig.press(Command::ActionOn(9, Action::Toggle(Toggle::EndStop)));
    assert!(rig.engine.looper().loop_end_stop(), "not even a lane out of range");
    rig.press(Command::ActionOn(4, Action::Toggle(Toggle::Retake)));
    assert_eq!(refusals_since(&rig, mark), [(1, Refusal::RetakeCapturing)], "a refusal on the selected lane, not the named one");
}

/// Lanes 0 and 1 playing the same 4-bar loop, lane 0 selected.
fn two_lanes() -> Rig {
    let mut rig = Rig::new();
    rig.set_input(code);
    rig.record_first_take(0, 4, 2400);
    rig.set_level(0.0);
    rig.idle();
    rig.press(Command::Copy(0));
    rig.idle();
    rig.press(Command::SelectTrack(0));
    rig
}

/// A pedal's CLEAR on lane 0, then `between`, then CLEAR again: did it clear?
fn clears_past(between: &[Command]) -> bool {
    let mut rig = two_lanes();
    rig.press(Command::SetRetake(true)); // a setting: FIXED is refused from here on
    rig.press(Command::Action(Action::Clear));
    for &command in between {
        rig.press(command);
    }
    rig.idle();
    rig.press(Command::Action(Action::Clear));
    rig.state(0) == LaneState::Empty
}

#[test]
fn a_toggle_is_a_looper_press_refused_or_not() {
    assert!(clears_past(&[]), "CLEAR, CLEAR clears");
    assert!(!clears_past(&[press(Toggle::Fixed)]), "CLEAR, a refused FIXED, CLEAR asks again");
    assert!(!clears_past(&[press(Toggle::Click)]), "CLEAR, CLICK, CLEAR asks again");
    assert!(!clears_past(&[press(Toggle::Send(InputSend::Reverb))]), "CLEAR, REVERB, CLEAR asks again");
    assert!(!clears_past(&[Command::ActionOn(3, Action::Toggle(Toggle::EndStop))]));
}

#[test]
fn go_lives_toggle_disarms_no_clear_and_its_press_disarms_once() {
    assert!(clears_past(&[press(Toggle::Live(0))]), "a cap click between the two CLEARs: the second confirms");
    assert!(clears_past(&[press(Toggle::Live(1)), press(Toggle::Live(1))]));
    // A pedal's GO LIVE: `Press` first, the UI's toggle later (its round trip through the WebView).
    assert!(!clears_past(&[Command::Press, press(Toggle::Live(0))]), "one physical press disarms once");
}

#[test]
fn go_lives_toggle_flips_the_engines_value_and_the_overview_shows_it() {
    let mut rig = Rig::new();
    rig.advance(128);
    let live = |rig: &Rig| [0, 1].map(|s| rig.engine.looper().overview().slot_live(s));
    assert_eq!(live(&rig), [true, false], "the rig's GO LIVE on slot A");
    let mark = rig.events.len();
    rig.press(press(Toggle::Live(0)));
    rig.press(press(Toggle::Live(1)));
    assert_eq!(live(&rig), [false, true]);
    assert_eq!(toggled_since(&rig, mark), [(Toggle::Live(0), false), (Toggle::Live(1), true)], "answered with the value it left");
    // A setter the feed has not shown yet: a press toggles from the engine's value, the setter's.
    let mark = rig.events.len();
    rig.queue(Command::SetSlotLive(0, true));
    rig.queue(press(Toggle::Live(0)));
    rig.advance(128);
    assert_eq!(live(&rig), [false, true], "on, then the press turns it off");
    assert_eq!(toggled_since(&rig, mark), [(Toggle::Live(0), true), (Toggle::Live(0), false)], "the setter is answered too");
}

#[test]
fn a_slot_past_the_rack_is_ignored_and_answered_by_nothing() {
    let mut rig = Rig::new();
    rig.advance(128);
    let before = rig.engine.toggles();
    let mark = rig.events.len();
    for command in [press(Toggle::Live(2)), press(Toggle::Live(255)), Command::ActionOn(1, Action::Toggle(Toggle::Live(9))), Command::SetSlotLive(2, true), Command::SetSlotLive(255, false)] {
        rig.press(command);
    }
    rig.advance(128);
    assert_eq!(rig.engine.toggles(), before, "nothing switched");
    assert_eq!(toggled_since(&rig, mark), [], "and nothing answered");
    assert_eq!(rig.engine.unsent_toggles().count(), 0, "nor owed");
}

#[test]
fn go_lives_toggle_never_waits_behind_a_held_looper_command() {
    let mut rig = copying();
    rig.press(Command::PlayStop(1));
    assert!(rig.engine.holding(), "PLAY on the copy waits for its job");
    rig.press(press(Toggle::Live(1)));
    assert!(rig.engine.holding(), "the job is still running");
    assert!(value(&rig, Toggle::Live(1)), "slot B is live meanwhile");
    rig.idle();
    assert!(value(&rig, Toggle::Live(1)), "and switched once");
}

#[test]
fn a_stamped_go_live_toggle_renders_bit_for_bit_as_its_setter() {
    for block in [64usize, 480] {
        let render = |command: Command| {
            let mut rig = Rig::with(Opts { block, ..Default::default() });
            rig.set_input(|f| 0.5 * (f as f32 * 0.013).sin());
            rig.advance(1024);
            let at = rig.frame + 3 * block as i64 + 17;
            rig.send_at(at, command);
            rig.keep_output();
            rig.advance(8 * block as i64);
            assert!(!rig.engine.looper().overview().slot_live(0), "block {block}: off at its frame");
            (rig.monitor.clone(), rig.record.clone())
        };
        let (setter, toggle) = (render(Command::SetSlotLive(0, false)), render(press(Toggle::Live(0))));
        assert!(setter.0.iter().zip(&toggle.0).all(|(a, b)| a.to_bits() == b.to_bits()), "block {block}: the monitor, bit for bit");
        assert!(setter.1.iter().zip(&toggle.1).all(|(a, b)| a.to_bits() == b.to_bits()), "block {block}: the record path, bit for bit");
        assert_eq!(setter.0.len(), 8 * block);
    }
}

/// A 16-bar loop (32 s) on lane 0, copied to lane 1: the copy job runs for some 31 ms.
fn copying() -> Rig {
    let mut rig = Rig::with(Opts { loop_seconds: 60.0, ..Default::default() });
    rig.set_level(0.5);
    rig.record_first_take(0, 16, 2400);
    rig.set_level(0.0);
    rig.set(Command::SetMute(0, true));
    rig.advance(4800);
    rig.press(Command::SelectTrack(0));
    rig.press(Command::Copy(0));
    rig
}

#[test]
fn an_input_sends_toggle_never_waits_behind_a_held_looper_command_and_the_others_wait_as_their_setters_did() {
    let mut rig = copying();
    rig.press(Command::PlayStop(1));
    assert!(rig.engine.holding(), "PLAY on the copy waits for its job");
    rig.press(press(Toggle::Send(InputSend::Echo)));
    rig.press(press(Toggle::Click));
    rig.press(press(Toggle::EndStop));
    assert!(rig.engine.holding(), "the job is still running");
    assert!(rig.engine.input_fx().is_on(InputSend::Echo), "the echo is on meanwhile");
    assert!(!rig.engine.clock().metronome() && !rig.engine.looper().loop_end_stop(), "CLICK and END STOP wait in order, as SetMetronome and SetLoopEndStop do");
    rig.idle();
    assert!(rig.engine.clock().metronome() && rig.engine.looper().loop_end_stop(), "then switch");
    assert!(rig.engine.input_fx().is_on(InputSend::Echo), "and the echo switched once");
}

#[test]
fn an_input_sends_toggle_passing_a_held_clear_confirmation_lets_it_confirm() {
    let mut rig = copying();
    rig.press(Command::Action(Action::Clear));
    rig.press(Command::Action(Action::Clear));
    assert!(rig.engine.holding() && rig.state(0) == LaneState::Playing, "the confirmation waits for the copy reading lane 0");
    rig.press(press(Toggle::Send(InputSend::Echo)));
    assert!(rig.engine.input_fx().is_on(InputSend::Echo), "the echo is on at once");
    rig.idle();
    assert_eq!(rig.state(0), LaneState::Empty, "the CLEAR pressed before the echo still confirms");
}

#[test]
fn a_toggle_the_full_ring_refused_reaches_the_feed_later_and_until_then_reads_as_unsent() {
    // A ring of four: the new engine's first publish fills it.
    let mut rig = Rig::with_event_capacity(Opts::default(), 4);
    rig.hold_events = true;
    rig.advance(128);
    rig.queue(press(Toggle::Fixed));
    rig.queue(press(Toggle::Send(InputSend::Ring)));
    rig.advance(128);
    assert!(value(&rig, Toggle::Fixed) && value(&rig, Toggle::Send(InputSend::Ring)), "applied while the ring is full");
    let unsent: Vec<_> = rig.engine.unsent_toggles().collect();
    assert_eq!(unsent, [(Toggle::Fixed, true), (Toggle::Send(InputSend::Ring), true)], "what a rebuild now reads from the engine");
    for _ in 0..16 {
        rig.read_events();
        rig.advance(128);
    }
    rig.read_events();
    assert_eq!(
        toggled_since(&rig, 0),
        [(Toggle::Live(0), true), (Toggle::Fixed, true), (Toggle::Send(InputSend::Ring), true)],
        "each once, at its value (the rig's GO LIVE first)"
    );
    assert_eq!(rig.engine.unsent_toggles().count(), 0, "nothing left unsent");
    assert!(rig.engine.diag().events_dropped > 0);
}

#[test]
fn a_setter_and_a_toggle_in_one_block_under_a_full_ring_still_reach_the_feed_at_the_applied_value() {
    // CLICK off, the ring full: the setter turns it on, the toggle back off in the same block. The value
    // ends where it started, but the feed (which may have heard the setter's value elsewhere: the UI shows
    // it, the host sent it) must still hear the applied one.
    let mut rig = Rig::with_event_capacity(Opts::default(), 4);
    rig.hold_events = true;
    rig.advance(128);
    rig.send_at(rig.frame, Command::SetMetronome(true));
    rig.send_at(rig.frame, press(Toggle::Click));
    rig.advance(128);
    assert!(!rig.engine.clock().metronome());
    assert_eq!(rig.engine.unsent_toggles().collect::<Vec<_>>(), [(Toggle::Click, false), (Toggle::Click, false)], "both answers owed, at the applied value");
    for _ in 0..16 {
        rig.read_events();
        rig.advance(128);
    }
    rig.read_events();
    assert_eq!(toggled_since(&rig, 0), [(Toggle::Live(0), true), (Toggle::Click, false), (Toggle::Click, false)], "both arrive, at the applied value");
    assert_eq!(rig.engine.unsent_toggles().count(), 0);
}
