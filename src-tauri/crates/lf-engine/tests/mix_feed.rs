//! What the feed hears of a lane's mix (D21): `Event::Mix`, the mix as the engine applies it, sent from
//! `Looper::publish` when it differs from the last one the event ring took, and the delivery rule it
//! shares with a lane's info and the transport: marked delivered only once the ring takes it, so a full
//! ring delays them, never loses them. An engine sends no lane's mix until the commands queued ahead of
//! its first block are all taken, and the host reads the applied mixes from it (`Engine::applied_mixes`). No Web Audio guard covered the feed's delivery.

mod common;

use common::{code, Opts, Rig};
use lf_engine::dsp::fx::{FxKind, FxParam};
use lf_engine::{Action, Command, CompactMix, Event, LaneMix, LaneState, SessionJob, Snapshot};

/// The mixes the feed heard for `lane` since event `mark`.
fn mixes(rig: &Rig, mark: usize, lane: u8) -> Vec<CompactMix> {
    rig.events[mark..]
        .iter()
        .filter_map(|e| match *e {
            Event::Mix { lane: l, mix, .. } if l == lane => Some(mix),
            _ => None,
        })
        .collect()
}

/// The mix the engine applies to `lane` now, as the feed carries it.
fn applied(rig: &Rig, lane: usize) -> CompactMix {
    CompactMix::from(&rig.engine.looper().mix(lane, rig.engine.fx()))
}

#[test]
fn a_new_engine_sends_every_lanes_mix_once() {
    let mut rig = Rig::new();
    rig.advance(4800);
    let unity = CompactMix::from(&LaneMix::default());
    for lane in 0..5 {
        assert_eq!(mixes(&rig, 0, lane), [unity], "lane {lane}");
    }
}

#[test]
fn a_mix_goes_out_once_per_change_not_once_per_publish() {
    let mut rig = Rig::new();
    // The click splits every block on its beats, and each split publishes.
    rig.set(Command::SetMetronome(true));
    rig.advance(4800);
    let mark = rig.events.len();
    rig.press(Command::SetVolume(0, 0.5));
    rig.advance(48_000);
    let heard = mixes(&rig, mark, 0);
    assert_eq!(heard.len(), 1, "one change, one Mix over a second of publishes: {heard:?}");
    assert_eq!(heard[0].volume, 0.5);
    assert!((1..5).all(|lane| mixes(&rig, mark, lane).is_empty()), "the other lanes did not change");
    // The same value again is no change.
    let mark = rig.events.len();
    rig.press(Command::SetVolume(0, 0.5));
    rig.advance(4800);
    assert!(mixes(&rig, mark, 0).is_empty(), "no change, no Mix");
    // Two settings in one block are one change.
    let mark = rig.events.len();
    rig.send_at(rig.frame, Command::SetFxParam(0, FxParam::Cutoff, 800.0));
    rig.send_at(rig.frame, Command::SetFxBypass(0, FxKind::Filter, false));
    rig.advance(4800);
    let heard = mixes(&rig, mark, 0);
    assert_eq!(heard, [applied(&rig, 0)], "both in one Mix");
    let filter = heard[0].fx[FxKind::Filter.index()];
    assert!(!filter.bypassed && filter.params[0] == 800.0, "{filter:?}");
}

#[test]
fn copy_clear_and_a_pedal_mute_send_the_mix_the_engine_gave_the_lane() {
    let mut rig = Rig::new();
    rig.set_input(code);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.press(Command::SetVolume(0, 0.5));
    rig.press(Command::SetDubFeedback(0, 0.25));
    let mark = rig.events.len();
    rig.press(Command::Copy(0));
    rig.idle();
    assert_eq!(rig.state(1), LaneState::Playing);
    let copied = mixes(&rig, mark, 1);
    assert_eq!(copied, [applied(&rig, 1)], "one Mix for the copy");
    assert_eq!((copied[0].volume, copied[0].dub_feedback), (0.5, 0.25), "the source's mix: {copied:?}");
    let mark = rig.events.len();
    rig.press(Command::ActionOn(1, Action::Mute));
    rig.advance(256);
    assert_eq!(mixes(&rig, mark, 1), [CompactMix { muted: true, ..copied[0] }], "a pedal's MUTE");
    let mark = rig.events.len();
    rig.press(Command::Clear(0));
    rig.advance(256);
    assert_eq!(rig.state(0), LaneState::Empty);
    assert_eq!(mixes(&rig, mark, 0), [CompactMix::from(&LaneMix::default())], "a CLEAR's defaults");
}

#[test]
fn a_mix_the_full_ring_refused_goes_out_later_though_nothing_changes_again() {
    // A ring of four: the new engine's first publish (five lanes' info and mix, the transport) fills it.
    let mut rig = Rig::with_event_capacity(Opts::default(), 4);
    rig.hold_events = true;
    rig.advance(128);
    rig.queue(Command::SetVolume(2, 0.5));
    rig.advance(128);
    assert_eq!(rig.engine.looper().volume(2).0, 0.5, "applied while the ring is full");
    // The feed reads again; nothing changes from here on.
    for _ in 0..16 {
        rig.read_events();
        rig.advance(128);
    }
    rig.read_events();
    assert_eq!(mixes(&rig, 0, 2).iter().map(|m| m.volume).collect::<Vec<_>>(), [0.5], "the refused Mix arrives, once, at its new value");
    assert!(rig.engine.diag().events_dropped > 0, "the full ring counted what it refused");
    for lane in 0..5 {
        assert!(rig.events.iter().any(|e| matches!(e, Event::Lane { lane: l, .. } if *l == lane)), "lane {lane}'s info arrives");
        assert_eq!(mixes(&rig, 0, lane).len(), 1, "lane {lane}'s mix arrives once");
    }
    assert!(rig.events.iter().any(|e| matches!(e, Event::Transport { .. })), "the transport arrives");
}

#[test]
fn a_lanes_info_the_full_ring_refused_goes_out_later_though_nothing_changes_again() {
    let mut rig = Rig::with_event_capacity(Opts::default(), 4);
    rig.hold_events = true;
    for _ in 0..16 {
        rig.advance(128);
        rig.read_events();
    }
    // Five mixes in one block fill the ring of four; the press's info finds it full.
    for lane in 0..5 {
        rig.send_at(rig.frame, Command::SetVolume(lane, 0.5));
    }
    rig.advance(128);
    let mark = rig.events.len();
    rig.press(Command::RecDub(1));
    let info = rig.lane(1);
    assert_eq!(info.state, LaneState::Recording);
    for _ in 0..8 {
        rig.read_events();
        rig.advance(128);
    }
    rig.read_events();
    assert_eq!(rig.lane(1), info, "lane 1's info has not changed again since the press");
    assert!(
        rig.events[mark..].iter().any(|e| matches!(e, Event::Lane { lane: 1, info: i, .. } if *i == info)),
        "lane 1's new info arrives once the ring has room"
    );
    assert!((0..5).all(|lane| mixes(&rig, mark, lane).iter().map(|m| m.volume).eq([0.5])), "and every lane's new mix, once");
}

#[test]
fn an_engine_sends_no_mix_before_its_first_block_then_each_lanes_once_with_its_queued_commands() {
    let mut rig = Rig::new();
    // A new engine's settings replay waits in the command ring for its first block.
    rig.queue(Command::SetVolume(1, 0.5));
    rig.queue(Command::SetMute(3, true));
    // No device runs yet: the host serves a snapshot on the idle engine.
    assert!(rig.session().send(Box::new(SessionJob::Snapshot(Snapshot::new(Vec::new())))).is_ok());
    rig.engine.service_session_idle();
    assert!(rig.session().returned().is_some(), "the idle engine served the snapshot");
    rig.read_events();
    assert!(!rig.events.iter().any(|e| matches!(e, Event::Mix { .. })), "no mix before the first block: {:?}", rig.events);
    for lane in 0..5 {
        assert!(rig.events.iter().any(|e| matches!(e, Event::Lane { lane: l, .. } if *l == lane)), "lane {lane}'s info goes out");
    }
    rig.advance(4800);
    let unity = CompactMix::from(&LaneMix::default());
    assert_eq!(mixes(&rig, 0, 1), [CompactMix { volume: 0.5, ..unity }], "the queued volume, applied");
    assert_eq!(mixes(&rig, 0, 3), [CompactMix { muted: true, ..unity }], "the queued mute, applied");
    for lane in [0, 2, 4] {
        assert_eq!(mixes(&rig, 0, lane), [unity], "lane {lane}");
    }
}

#[test]
fn the_applied_mixes_read_from_the_engine_are_none_before_its_first_block_then_the_last_mix_sent() {
    let mut rig = Rig::new();
    rig.queue(Command::SetVolume(0, 0.25));
    assert!(rig.engine.applied_mixes().is_none(), "the queued commands are not applied yet");
    rig.advance(4800);
    rig.press(Command::SetDubFeedback(4, 0.5));
    rig.press(Command::SetFxParam(2, FxParam::Cutoff, 1234.5));
    rig.advance(256);
    let (frame, applied) = rig.engine.applied_mixes().expect("the engine has run");
    assert_eq!(frame, rig.frame, "stamped at the frame the next block starts");
    for lane in 0..5 {
        assert_eq!(mixes(&rig, 0, lane as u8).last(), Some(&applied[lane]), "lane {lane}");
    }
    assert_eq!((applied[0].volume, applied[4].dub_feedback), (0.25, 0.5));
}

/// Seventeen settings for `lane`: every field of its mix away from a fresh lane's.
fn settings(lane: u8) -> Vec<Command> {
    let x = lane as f64 * 0.01;
    let mut out = vec![
        Command::SetVolume(lane, 0.5 + x as f32),
        Command::SetMute(lane, true),
        Command::SetDubFeedback(lane, 0.25 + x as f32),
        Command::SetPan(lane, -0.5 + x as f32),
    ];
    out.extend(FxKind::ALL.map(|kind| Command::SetFxBypass(lane, kind, false)));
    for (param, value) in [
        (FxParam::Cutoff, 1234.0),
        (FxParam::Q, 2.0),
        (FxParam::Semitones, 3.0),
        (FxParam::Rate, 4.0),
        (FxParam::Time, 0.25),
        (FxParam::Feedback, 0.4),
        (FxParam::Mix, 0.3),
        (FxParam::Amount, 0.6),
    ] {
        out.push(Command::SetFxParam(lane, param, value + x));
    }
    out
}

#[test]
fn a_replay_longer_than_one_blocks_take_sends_no_mix_until_it_is_all_taken() {
    // Each lane's mix once its seventeen settings are applied, a lane a block on a reference engine.
    let mut reference = Rig::new();
    reference.advance(128);
    let expected: Vec<CompactMix> = (0..5)
        .map(|lane| {
            for command in settings(lane) {
                reference.send_at(reference.frame, command);
            }
            reference.advance(128);
            applied(&reference, lane as usize)
        })
        .collect();
    // Eighty-five settings (the rig's own first command ahead of them) wait for the first block, which
    // takes 64.
    let mut rig = Rig::new();
    for lane in 0..5 {
        for command in settings(lane) {
            rig.queue(command);
        }
    }
    rig.advance(128);
    assert_ne!(applied(&rig, 4), expected[4], "lane 4's settings wait in the ring");
    assert!(!rig.events.iter().any(|e| matches!(e, Event::Mix { .. })), "no partial mix: {:?}", rig.events);
    assert!(rig.engine.applied_mixes().is_none(), "nor one read from the engine");
    rig.advance(128);
    let (_, read) = rig.engine.applied_mixes().expect("the replay is all taken");
    for lane in 0..5 {
        assert_ne!(expected[lane], CompactMix::from(&LaneMix::default()), "lane {lane}'s settings change its mix");
        assert_eq!(mixes(&rig, 0, lane as u8), [expected[lane]], "lane {lane}'s mix, once, with all its settings");
        assert_eq!(read[lane], expected[lane], "lane {lane}, read from the engine");
    }
}

#[test]
fn an_event_with_a_mix_stays_under_twice_the_size_of_one_without() {
    // 56 bytes before `Event::Mix` (the largest variant was `Lane`); a whole `LaneMix` would make it 192.
    assert!(std::mem::size_of::<Event>() <= 112, "{} bytes", std::mem::size_of::<Event>());
}
