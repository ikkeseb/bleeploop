//! Stage 3 wired into the engine (`src/effects.rs`, `src/instruments.rs`): the selected instrument on
//! the bus and in the record tap, on the guitar's grid; each lane through its FX chain into the stereo
//! bus, with a reverb tail; CLEAR and COPY on a lane's FX (`machine.ts` `clear`, `copy`); the rhythmic
//! FX on the looper's grid; and the whole wired sound bit-identical at any block size. The ports
//! themselves are held to Tone in `synth.rs`, `fx_*.rs` and `limiter.rs`.

mod common;

use common::{Delay, Opts, Rig};
use lf_engine::dsp::fx::{default_fx_states, FxKind, FxParam};
use lf_engine::grid::Frame;
use lf_engine::{Command, Instrument, LaneState, NoteTarget};

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, v| m.max(v.abs()))
}

/// Lane 0 plays a constant 0.5 loop of one bar; the input is silent from here on.
fn playing() -> Rig {
    let mut rig = Rig::new();
    rig.set_level(0.5);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.advance(4800);
    rig
}

#[test]
fn the_selected_instrument_sounds_on_the_bus_and_a_switch_releases_it() {
    let mut rig = Rig::new();
    rig.keep_output();
    rig.press(Command::NoteOn(60, 0.8));
    rig.advance(4800);
    assert_eq!(peak(&rig.bus), 0.0, "no instrument selected: a note does nothing");

    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)));
    rig.keep_output();
    rig.press(Command::NoteOn(60, 0.8));
    rig.advance(4800);
    assert!(peak(&rig.bus) > 0.05, "the lead sounds: {}", peak(&rig.bus));
    assert_eq!(rig.bus, rig.bus_right, "a mono synth sits in the middle");
    assert!(rig.monitor.iter().all(|&m| m == 0.0), "an instrument is not the monitored wet signal");
    assert_eq!(rig.heard, rig.heard_right);

    // The switch releases the lead's held note; the pad takes the next one.
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Pad)));
    rig.advance(rig.seconds(3.0));
    rig.keep_output();
    rig.advance(4800);
    assert!(peak(&rig.bus) < 1e-4, "the lead's note was released: {}", peak(&rig.bus));
    rig.press(Command::NoteOn(64, 0.8));
    rig.advance(rig.seconds(1.0));
    assert!(peak(&rig.bus) > 0.01, "the pad sounds: {}", peak(&rig.bus));
}

#[test]
fn the_drum_kit_is_stereo_and_a_pitch_wheel_moves_the_synth() {
    let mut rig = Rig::new();
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Drums)));
    rig.keep_output();
    rig.press(Command::NoteOn(38, 1.0)); // the snare: stereo noise
    rig.advance(4800);
    assert!(peak(&rig.bus) > 0.05 && rig.bus != rig.bus_right, "the snare sounds, and not in mono");

    // The same note with and without a bend differs: the wheel reaches the voice.
    let render = |bend: f64| {
        let mut rig = Rig::new();
        rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)));
        rig.set(Command::PitchBend(bend));
        rig.keep_output();
        rig.press(Command::NoteOn(60, 0.8));
        rig.advance(4800);
        rig.bus
    };
    assert_ne!(render(0.0), render(2.0));
}

/// A note played on the heard downbeat of a first take lands on the loop's frame 0, as a guitar note
/// does (`tests/align.rs`): the player hears the click `LIMITER + OUT` after its frame and plays; the
/// instrument sounds `LEAD` later, and its record path lags it by the input side (`IN`) plus the
/// plugin's latency less that lead, where the guitar's note reaches the looper. A wrong input side
/// shifts the note by exactly the error.
#[test]
fn a_note_played_on_the_heard_click_lands_on_the_grid() {
    const IN: Frame = 480;
    const OUT: Frame = 1000;
    const PLUGIN: Frame = 57;
    const LIMITER: Frame = 288;
    let take = |input_latency: Frame| {
        let mut rig = Rig::with(Opts { align: IN + OUT + LIMITER, ..Default::default() });
        rig.input_latency = input_latency;
        rig.install(0, Box::new(Delay::new(PLUGIN)));
        rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)));
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(1.0));
        let mark = rig.events.len();
        rig.press(Command::RecDub(0));
        let downbeat = rig.count_one(mark) + 4 * 24_000;
        let note = downbeat + LIMITER + OUT;
        rig.send_at(note, Command::NoteOn(69, 1.0));
        rig.send_at(note + 12_000, Command::NoteOff(69));
        rig.advance_to(rig.end_frame() + 1);
        assert_eq!(rig.state(0), LaneState::Playing);
        rig.pcm(0)
    };
    let onset = |pcm: &[f32]| pcm.iter().position(|x| x.abs() > 1e-6).unwrap();
    let pcm = take(IN);
    // The attack's first sample is its zero: the note sounds from the frame after it.
    assert_eq!(onset(&pcm), 1, "the note starts on the loop's frame 0");
    assert!(pcm[pcm.len() - 1024..].iter().all(|&x| x == 0.0), "nothing of it before the downbeat");
    assert_eq!(onset(&take(IN + 100)), 101, "an input side reported 100 frames long records the note 100 late");
    let early = take(IN - 100);
    assert!(early[0].abs() > 1e-3, "100 short, the take opens mid-attack: {}", early[0]);
}

#[test]
fn a_lanes_reverb_send_reaches_the_stereo_bus_and_rings_on_after_the_lane_stops() {
    let mut rig = playing();
    rig.set(Command::SetFxParam(0, FxParam::Amount, 1.0));
    rig.set(Command::SetFxBypass(0, FxKind::Reverb, false));
    rig.advance(48_000);
    rig.keep_output();
    rig.advance(4800);
    assert_ne!(rig.bus, rig.bus_right, "the reverb is stereo");
    let (_, looper) = rig.output.as_ref().unwrap();
    assert!(looper.iter().all(|&x| x == 0.5), "the looper tap is the lane before its FX");

    rig.press(Command::Stop(0));
    rig.advance(4800);
    rig.keep_output();
    rig.advance(4800);
    let (_, looper) = rig.output.as_ref().unwrap();
    assert!(looper.iter().all(|&x| x == 0.0), "the lane stopped");
    assert!(peak(&rig.bus) > 1e-3, "the reverb tail rings on: {}", peak(&rig.bus));
}

#[test]
fn clear_resets_a_lanes_fx_and_copy_carries_them() {
    let mut rig = playing();
    rig.set(Command::SetFxBypass(0, FxKind::Filter, false));
    rig.set(Command::SetFxParam(0, FxParam::Cutoff, 300.0));
    rig.set(Command::SetFxParam(0, FxParam::Semitones, 5.0));
    rig.set(Command::SetFxBypass(0, FxKind::Delay, false));
    let state = rig.engine.fx().chain(0).get_state();
    assert_ne!(state, default_fx_states());

    rig.press(Command::Copy(0));
    rig.idle();
    assert_ne!(rig.state(1), LaneState::Empty);
    assert_eq!(rig.engine.fx().chain(1).get_state(), state, "the copy takes the source's FX");

    rig.press(Command::Clear(0));
    assert_eq!(rig.engine.fx().chain(0).get_state(), default_fx_states(), "CLEAR resets the lane's FX");
    assert_eq!(rig.engine.fx().chain(1).get_state(), state, "and only that lane's");
    rig.press(Command::ClearAll);
    assert_eq!(rig.engine.fx().chain(1).get_state(), default_fx_states());
}

/// The stutter at "8n" opens for the first half of every eighth from the loop's anchor, and follows the
/// anchor when an idle transport restarts from the top.
#[test]
fn the_stutter_gates_on_the_loops_grid_and_follows_a_restart() {
    let mut rig = playing();
    rig.set(Command::SetFxParam(0, FxParam::Rate, 1.0));
    rig.set(Command::SetFxBypass(0, FxKind::Stutter, false));
    rig.advance(4800); // past the bypass crossfade
    let period = rig.fpb() / 8;
    let check = |rig: &mut Rig| {
        rig.keep_output();
        rig.advance(rig.seconds(1.0));
        let (start, anchor) = (rig.output.as_ref().unwrap().0, rig.anchor());
        for (k, &x) in rig.bus.iter().enumerate() {
            let pos = (start + k as Frame - anchor).rem_euclid(period);
            let edge = pos.min(period - pos).min((pos - period / 2).abs());
            // Each edge settles over a few dozen frames (the gate's control buffer is interpolated), and
            // a closed gate leaks ~0.2 % of the dry signal (Tone's equal-power crossfade at full wet).
            if edge > 64 {
                let open = pos < period / 2;
                assert!(if open { x > 0.4 } else { x.abs() < 0.005 }, "pos {pos}: {x}");
            }
        }
    };
    check(&mut rig);
    let before = rig.anchor();
    rig.press(Command::StopAll);
    rig.advance(4801);
    rig.press(Command::PlayAll);
    assert_ne!((rig.anchor() - before).rem_euclid(period), 0, "the restart moved the grid");
    rig.advance(4800);
    check(&mut rig);
}

/// Notes, a drum kit, every effect on two lanes, the reverb, wheels and a switch, all frame-stamped:
/// the heard output is the same bits at any block size.
fn session(block: usize) -> [Vec<f32>; 2] {
    let mut rig = Rig::with(Opts { block, ..Default::default() });
    rig.set_input(|f| 0.4 * (f as f32 * 0.013).sin());
    rig.record_first_take(0, 1, 2400);
    rig.press(Command::Copy(0));
    rig.idle();
    rig.set_input(|_| 0.0);
    let t = 400_000;
    assert!(rig.frame < t);
    rig.send_at(t, Command::PlayStop(1));
    for lane in [0, 1] {
        for kind in FxKind::ALL {
            rig.send_at(t + 37 * lane as Frame, Command::SetFxBypass(lane, kind, false));
        }
    }
    let script = [
        (100, Command::SetFxParam(0, FxParam::Cutoff, 700.0)),
        (130, Command::SetFxParam(1, FxParam::Semitones, -5.0)),
        (170, Command::SetFxParam(0, FxParam::Rate, 3.0)),
        (211, Command::SetFxParam(1, FxParam::Feedback, 0.8)),
        (250, Command::SetFxParam(0, FxParam::Amount, 0.9)),
        (300, Command::SelectInstrument(NoteTarget::Builtin(Instrument::Drums))),
        (301, Command::NoteOn(36, 1.0)),
        (5_000, Command::NoteOn(38, 0.7)),
        (9_777, Command::NoteOn(42, 0.5)),
        (20_000, Command::SelectInstrument(NoteTarget::Builtin(Instrument::Bass))),
        (20_050, Command::Modulation(0.6)),
        (20_101, Command::NoteOn(40, 0.9)),
        (30_003, Command::PitchBend(-1.5)),
        (40_000, Command::NoteOff(40)),
        (41_000, Command::SelectInstrument(NoteTarget::Builtin(Instrument::Pad))),
        (41_001, Command::NoteOn(60, 0.8)),
        (41_002, Command::NoteOn(64, 0.8)),
        (60_000, Command::SetFxParam(0, FxParam::Cutoff, 5000.0)),
        (70_000, Command::AllNotesOff),
        (80_000, Command::Stop(0)),
    ];
    for (at, command) in script {
        rig.send_at(t + at, command);
    }
    rig.advance_to(t);
    rig.keep_output();
    rig.advance(rig.seconds(3.0));
    assert!(peak(&rig.heard) > 0.05);
    [std::mem::take(&mut rig.heard), std::mem::take(&mut rig.heard_right)]
}

#[test]
fn the_wired_sound_is_bit_identical_across_block_sizes() {
    let reference = session(128);
    assert_ne!(reference[0], reference[1], "the session is stereo");
    for block in [1, 32, 64, 127, 480, 1024] {
        for (side, (got, want)) in ["left", "right"].iter().zip(session(block).iter().zip(&reference)) {
            let first = got.iter().zip(want).position(|(a, b)| a.to_bits() != b.to_bits());
            assert_eq!(first, None, "block {block}: the {side} output differs from block 128");
        }
    }
}

/// A note played while a looper command waits for a block job sounds at once: the instruments never
/// wait behind the looper (in the web app notes go straight to the synth).
#[test]
fn a_note_never_waits_behind_a_held_looper_command() {
    let mut rig = Rig::with(Opts { loop_seconds: 60.0, ..Default::default() });
    rig.set_level(0.5);
    rig.record_first_take(0, 16, 2400); // 32 s: a copy job runs for ~31 ms
    rig.set_level(0.0);
    rig.set(Command::SetMute(0, true));
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)));
    rig.advance(4800);
    rig.press(Command::Copy(0));
    rig.press(Command::PlayStop(1));
    assert!(rig.engine.holding(), "PLAY on the copy waits for its job");
    rig.keep_output();
    rig.press(Command::NoteOn(60, 1.0));
    rig.advance(512);
    assert!(rig.engine.holding(), "the job is still running");
    assert!(peak(&rig.bus) > 0.05, "the note sounds meanwhile: {}", peak(&rig.bus));
}

/// More commands in one block than the engine's table holds are applied late, never dropped: every
/// NoteOff arrives and no note hangs.
#[test]
fn a_burst_of_notes_is_never_dropped() {
    let mut rig = Rig::with(Opts { block: 4096, ..Default::default() });
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)));
    for note in 0..100 {
        rig.send_at(rig.frame, Command::NoteOn(note, 0.8));
    }
    for note in 0..100 {
        rig.send_at(rig.frame, Command::NoteOff(note));
    }
    rig.advance(3 * 4096);
    assert_eq!(rig.engine.diag().commands_dropped, 0);
    rig.advance(rig.seconds(3.0));
    rig.keep_output();
    rig.advance(4096);
    assert!(peak(&rig.bus) < 1e-4, "a note hangs: {}", peak(&rig.bus));
}

/// Two slots may hold the same synth: switching between them releases the held notes, as switching
/// between two web synths did, though the pick does not change.
#[test]
fn picking_the_selected_instrument_again_releases_its_notes() {
    let mut rig = Rig::new();
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)));
    rig.press(Command::NoteOn(60, 0.8));
    rig.advance(4800);
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)));
    rig.advance(rig.seconds(3.0));
    rig.keep_output();
    rig.advance(4800);
    assert!(peak(&rig.bus) < 1e-4, "the note was released: {}", peak(&rig.bus));
}

#[test]
fn an_fx_parameter_is_clamped_to_its_range_and_a_note_past_127_does_nothing() {
    let mut rig = playing();
    rig.set(Command::SetFxParam(0, FxParam::Cutoff, -5.0));
    rig.set(Command::SetFxParam(0, FxParam::Feedback, 3.0));
    let chain = rig.engine.fx().chain(0);
    assert_eq!((chain.param(FxParam::Cutoff), chain.param(FxParam::Feedback)), (FxParam::Cutoff.def().min, FxParam::Feedback.def().max));
    let mut rig = Rig::new();
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Bass)));
    rig.keep_output();
    rig.press(Command::NoteOn(200, 1.0));
    rig.advance(4800);
    assert_eq!(peak(&rig.bus), 0.0);
}
