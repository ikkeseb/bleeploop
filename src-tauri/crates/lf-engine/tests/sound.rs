//! Stage 3 wired into the engine (`src/effects.rs`, `src/instruments.rs`): the selected instrument on
//! the bus and in the record tap, on the guitar's grid; each lane through its FX chain into the stereo
//! bus, with a reverb tail; CLEAR and COPY on a lane's FX (`machine.ts` `clear`, `copy`); the rhythmic
//! FX on the looper's grid; and the whole wired sound bit-identical at any block size. The ports
//! themselves are held to Tone in `synth.rs`, `fx_*.rs` and `limiter.rs`. What the removed Web Audio
//! probes fx-grid.mjs (two lanes' stutters, a delay after a new tempo) and fx-pitch-cost.mjs (a live
//! pitch switch) checked is here on a lane's rendered output.

mod common;

use common::{Delay, Opts, Rig};
use lf_engine::dsp::fx::{default_fx_states, division_beats, FxKind, FxParam};
use lf_engine::grid::{frames_per_bar, Frame};
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

/// `NoteTarget::Off`: a note sounds nothing, and switching to it releases the note held (an organ
/// holds while its key is down: the same run without the switch still sounds).
#[test]
fn off_sounds_no_note_and_switching_to_it_releases_the_held_one() {
    let mut rig = Rig::new();
    rig.set(Command::SelectInstrument(NoteTarget::Off));
    rig.keep_output();
    rig.press(Command::NoteOn(60, 0.8));
    rig.advance(4800);
    assert_eq!(peak(&rig.bus), 0.0, "off: a note sounds nothing");

    let held = |off: bool| {
        let mut rig = Rig::new();
        rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Organ)));
        rig.press(Command::NoteOn(60, 0.8));
        rig.advance(rig.seconds(0.5));
        if off {
            rig.set(Command::SelectInstrument(NoteTarget::Off));
        }
        rig.advance(rig.seconds(3.0));
        rig.keep_output();
        rig.advance(4800);
        peak(&rig.bus)
    };
    assert!(held(false) > 0.01, "the organ holds its note: {}", held(false));
    assert!(held(true) < 1e-4, "off released it: {}", held(true));
}

/// A built-in instrument's level (`SetInstrumentGain`) on a held organ note, against the same run at
/// unity: it glides down without a jump and scales what is heard and what is recorded alike; set
/// while another instrument is the target, it reaches the organ still, and it stays with the organ
/// across a switch away and back.
#[test]
fn an_instruments_level_glides_scales_heard_and_recorded_and_stays_with_it() {
    let run = |levels: bool| {
        let mut rig = Rig::new();
        let set = |rig: &mut Rig, gain: f32| {
            if levels {
                rig.press(Command::SetInstrumentGain(Instrument::Organ, gain));
            } else {
                rig.advance(1);
            }
        };
        rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Organ)));
        rig.press(Command::NoteOn(60, 0.8));
        rig.advance(4800);
        rig.keep_output();
        set(&mut rig, 0.5);
        rig.advance(9600);
        let glide = (std::mem::take(&mut rig.bus), std::mem::take(&mut rig.record));
        rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)));
        set(&mut rig, 0.25);
        rig.advance(9600);
        rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Organ)));
        rig.keep_output();
        rig.press(Command::NoteOn(64, 0.8));
        rig.advance(9600);
        (glide, rig.bus.clone())
    };
    let ((bus, record), back) = run(true);
    let ((bus_ref, record_ref), back_ref) = run(false);
    let ratios = |x: &[f32], reference: &[f32]| -> Vec<f32> { x.iter().zip(reference).filter(|(_, r)| r.abs() > 1e-3).map(|(a, r)| a / r).collect() };
    let glide = ratios(&bus, &bus_ref);
    assert!(glide.len() > 1000, "the organ sounds");
    assert!(glide[0] > 0.99, "no jump: {}", glide[0]);
    assert!(glide.windows(2).all(|w| w[1] <= w[0] + 1e-4), "the level glides down");
    assert!((glide[glide.len() - 1] - 0.5).abs() < 1e-3, "to the level: {}", glide[glide.len() - 1]);
    let recorded = ratios(&record[4800..], &record_ref[4800..]);
    assert!(!recorded.is_empty() && recorded.iter().all(|r| (r - 0.5).abs() < 1e-3), "recorded at the level too");
    let after = ratios(&back, &back_ref);
    assert!(after.len() > 1000 && after.iter().all(|r| (r - 0.25).abs() < 1e-3), "the level set away from the organ holds once it is back");
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

/// fx-grid.mjs's two chains, on two lanes: lane `heard` plays a constant 0.2 loop and the other a silent
/// one (its chain adds exactly nothing to the bus), both through a STUTTER switched on 93 ms apart, and
/// each division set on lane 0 and 38 frames later on lane 1. Per division: the bus over max(0.6 s, two
/// periods) from its start frame; and the beat grid's origin.
fn stutter_pair(bpm: u32, heard: usize) -> (Vec<(Frame, Vec<f32>)>, Frame) {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(bpm as f64));
    let level = |lane: usize| if lane == heard { 0.2 } else { 0.0 };
    rig.set_level(level(0));
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(level(1));
    rig.press(Command::RecDub(1));
    rig.advance_to(rig.start_frame() + master * 13 / 10); // one loop, committed at once (E10)
    rig.press(Command::RecDub(1));
    rig.set_level(0.0);
    rig.idle();
    assert!(rig.state(0) == LaneState::Playing && rig.state(1) == LaneState::Playing);
    rig.set(Command::SetFxBypass(0, FxKind::Stutter, false));
    rig.advance(rig.seconds(0.093));
    rig.set(Command::SetFxBypass(1, FxKind::Stutter, false));
    rig.advance(4800); // past the bypass crossfades
    let beat = frames_per_bar(bpm as f64, rig.sr) as f64 / 4.0;
    let mut out = Vec::new();
    for division in 0..4 {
        rig.set(Command::SetFxParam(0, FxParam::Rate, division as f64));
        rig.advance(37);
        rig.set(Command::SetFxParam(1, FxParam::Rate, division as f64));
        rig.advance(4800);
        rig.keep_output();
        let start = rig.frame;
        rig.advance(rig.seconds(0.6).max((2.0 * beat * division_beats(division)).ceil() as Frame));
        out.push((start, rig.bus.clone()));
    }
    (out, rig.engine.looper().grid_origin())
}

#[test]
fn two_lanes_stutters_on_one_division_open_and_close_on_the_same_frames() {
    // The bus of the session with lane 0 heard is lane 0's gate, with lane 1 heard lane 1's: the engine
    // is deterministic and a gate does not depend on what passes through it.
    for bpm in [120u32, 137] {
        let ((a, origin), (b, other)) = (stutter_pair(bpm, 0), stutter_pair(bpm, 1));
        assert_eq!(origin, other);
        let beat = frames_per_bar(bpm as f64, 48_000) as f64 / 4.0;
        let open = |v: &[f32]| v.iter().map(|&x| x > 0.1).collect::<Vec<bool>>();
        let edges = |v: &[bool]| (1..v.len()).filter(|&k| v[k] != v[k - 1]).collect::<Vec<usize>>();
        for (division, ((start, x), (_, y))) in a.iter().zip(&b).enumerate() {
            let (on_a, on_b) = (open(x), open(y));
            let (edges_a, edges_b) = (edges(&on_a), edges(&on_b));
            assert!(edges_a.len() >= 3, "{bpm} BPM, division {division}: the gate moves ({} edges)", edges_a.len());
            assert_eq!(edges_a, edges_b, "{bpm} BPM, division {division}: both gates open and close on the same frames");
            // The probe's grid check: open for the first half of each division from the grid's origin,
            // 3 ms either side of an edge left out, under 0.5 % of the frames wrong.
            let period = beat * division_beats(division);
            let margin = 0.003 * 48_000.0;
            let (mut checked, mut wrong) = (0, 0);
            for (k, &on) in on_a.iter().enumerate() {
                let phase = ((start + k as Frame - origin) as f64).rem_euclid(period);
                if phase.min((phase - period / 2.0).abs()).min(period - phase) < margin {
                    continue;
                }
                checked += 1;
                wrong += usize::from(on != (phase < period / 2.0));
            }
            let apart = x.iter().zip(y).map(|(p, q)| (p - q).abs()).fold(0.0f32, f32::max);
            println!("{bpm} BPM, division {division}: {} edges, {wrong} of {checked} frames off the grid, the two gates at most {apart:e} apart", edges_a.len());
            assert!(checked > 1000 && (wrong as f64) < 0.005 * checked as f64, "{bpm} BPM, division {division}: {wrong} of {checked} off the grid");
            // Not bit for bit: each bypassed DELAY still mixes its echoes in at -56 dB, and those hold each
            // lane's own switch-on moments, dying away at its feedback.
            assert!(apart < 1e-3, "{bpm} BPM, division {division}: the two lanes' outputs {apart} apart");
        }
    }
}

/// A one-bar first take at `bpm` on lane 0 whose loop is silent but for 0.5 on its frame 0.
fn impulse_loop(rig: &mut Rig, bpm: u32) {
    rig.set(Command::SetBpm(bpm as f64));
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let downbeat = rig.count_one(mark) + (4.0 * 60.0 / bpm as f64 * rig.sr as f64).round() as Frame;
    rig.set_input(move |f| if f == downbeat { 0.5 } else { 0.0 });
    rig.advance_to(downbeat + rig.fpb() + 2400);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.idle();
    assert!(rig.state(0) == LaneState::Playing && rig.pcm(0)[0] == 0.5);
}

/// Lane 0's DELAY on at its defaults (1/8, time untouched): frames from a loop start to the first echo.
fn first_echo(rig: &mut Rig) -> Frame {
    rig.set(Command::SetFxBypass(0, FxKind::Delay, false));
    rig.advance(4800);
    let boundary = rig.next_boundary();
    rig.advance_to(boundary);
    rig.keep_output();
    rig.advance(rig.seconds(0.4));
    let bus = &rig.bus;
    assert!(bus[0] > 0.1, "the loop's impulse is on the boundary: {}", bus[0]);
    let peak = (1..bus.len()).max_by(|&i, &j| bus[i].abs().total_cmp(&bus[j].abs())).unwrap();
    peak as Frame
}

#[test]
fn a_lanes_delay_takes_the_new_tempo_after_clear_all_and_a_first_take_at_another() {
    // fx-grid.mjs's reused lane: built at 120 BPM, CLEAR ALL, a new first take at 240 BPM, the delay
    // switched on without touching its time: its eighth is 125 ms.
    let mut rig = Rig::new();
    impulse_loop(&mut rig, 120);
    assert_eq!(first_echo(&mut rig), 12_000, "an eighth at 120 BPM: 250 ms");
    rig.press(Command::ClearAll);
    rig.advance(4800);
    impulse_loop(&mut rig, 240);
    assert_eq!(rig.bpm(), 240);
    assert_eq!(first_echo(&mut rig), 6_000, "an eighth at 240 BPM: 125 ms");
}

#[test]
fn a_live_pitch_enable_and_reset_on_a_playing_lane_add_no_step() {
    // fx-pitch-cost.mjs's dynamic render, on a lane: +12 semitones stored while bypassed, then enable,
    // bypass, enable again and reset (bypassed, 0 semitones, as a state reset) live, on a 220 Hz sine at
    // 0.1. The probe's bounds: no sample-to-sample step of 0.03 or more, no dropped block, and the
    // stored pitch sounding on each enable. At 44.1 kHz, as the probe ran.
    let sr = 44_100;
    let mut rig = Rig::at(sr);
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let downbeat = rig.count_one(mark) + 2 * sr as Frame; // four beats at 120 BPM
    // One bar at 120 BPM is 2 s: 440 whole cycles, so the loop is seamless.
    rig.set_input(move |f| 0.1 * (std::f64::consts::TAU * 220.0 * (f - downbeat) as f64 / sr as f64).sin() as f32);
    rig.advance_to(downbeat + rig.fpb() + 2400);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.master(), 2 * sr as Frame);
    rig.set(Command::SetFxParam(0, FxParam::Semitones, 12.0));
    rig.advance(4410);
    rig.keep_output();
    let t0 = rig.frame;
    let at = |s: f64| t0 + (s * sr as f64).round() as Frame;
    rig.send_at(at(0.4), Command::SetFxBypass(0, FxKind::Pitch, false));
    rig.send_at(at(0.95), Command::SetFxBypass(0, FxKind::Pitch, true));
    rig.send_at(at(1.4), Command::SetFxBypass(0, FxKind::Pitch, false));
    rig.send_at(at(1.9), Command::SetFxBypass(0, FxKind::Pitch, true));
    rig.send_at(at(1.9), Command::SetFxParam(0, FxParam::Semitones, 0.0));
    rig.advance_to(at(2.4));
    let bus = &rig.bus;
    let jump = bus.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
    let (mut run, mut silent) = (0, 0);
    for &x in bus {
        run = if x.abs() < 1e-7 { run + 1 } else { 0 };
        silent = silent.max(run);
    }
    let hz = |a: f64, b: f64| {
        let (i, j) = ((a * sr as f64).ceil() as usize, (b * sr as f64).floor() as usize);
        (i..j).filter(|&k| bus[k] <= 0.0 && bus[k + 1] > 0.0).count() as f64 / (b - a)
    };
    let heard = [hz(0.1, 0.35), hz(0.7, 0.9), hz(1.15, 1.35), hz(1.65, 1.85), hz(2.15, 2.35)];
    println!("pitch on a playing lane: largest step {jump:.5}, longest silent run {silent}, Hz dry/on/off/on/reset {heard:.1?}");
    assert!(jump < 0.03, "the pitch's enable, bypass or reset stepped the output by {jump}");
    assert!(silent < 8, "a dropped block: {silent} silent frames");
    for (k, want) in [220.0, 440.0, 220.0, 440.0, 220.0].into_iter().enumerate() {
        assert!((heard[k] - want).abs() < 10.0, "window {k}: {} Hz, want {want}", heard[k]);
    }
}

#[test]
fn a_note_to_each_selected_built_in_instrument_sounds_on_the_bus() {
    // instrument-routing.mjs's synth pick: a MIDI note sounds on the picked engine, its RMS over the
    // probe's analyser window (4096 frames, ending 150 ms after the note) above 0.01. A2 as the probe
    // played it; on the drum kit, its kick. Each pick renders its own sound: no two picks sound alike,
    // so a pick routed to the wrong instrument fails.
    let mut renders: Vec<(Instrument, Vec<f32>)> = Vec::new();
    for instrument in Instrument::ALL {
        let mut rig = Rig::new();
        rig.set(Command::SelectInstrument(NoteTarget::Builtin(instrument)));
        rig.keep_output();
        rig.press(Command::NoteOn(if instrument == Instrument::Drums { 36 } else { 45 }, 110.0 / 127.0));
        rig.advance(rig.seconds(0.15));
        let window = &rig.bus[rig.bus.len() - 4096..];
        let rms = (window.iter().map(|&x| x as f64 * x as f64).sum::<f64>() / window.len() as f64).sqrt();
        println!("{instrument:?}: rms {rms:.4}, peak {:.4}", peak(&rig.bus));
        assert!(rms > 0.01, "{instrument:?} sounds on the bus: rms {rms}");
        renders.push((instrument, rig.bus.clone()));
    }
    for (i, (a, x)) in renders.iter().enumerate() {
        for (b, y) in &renders[i + 1..] {
            assert!(x != y, "{a:?} and {b:?} render the same sound");
        }
    }
}
