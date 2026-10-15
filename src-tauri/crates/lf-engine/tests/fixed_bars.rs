//! FIXED's bar math, the engine's half of a parity check with the UI. The UI computes the same rule to
//! bound its bar selector and to draw a later take's window (`src/ui/state/engine-store.ts`:
//! `nextTakeMaxBars`, `laterTakeBars`); `verify/fixtures/fixed-bars.json` holds rows of (rate, BPM,
//! master, FIXED request) and what both sides must answer, and `verify/guards/fixed-bars.mjs` runs the
//! UI's code over the same rows. Here each row's master is loaded into an engine with the host's
//! 60-second lanes, then the engine answers the selector's bound (`next_take_max_bars`) and records the
//! window a FIXED take over it arms.

mod common;

use common::{Opts, Rig};
use lf_engine::grid::{frames_per_bar, Frame, Meter};
use lf_engine::{Command, EngineConfig, LaneMix, LoadTrack, Load, SessionJob};
use serde::Deserialize;

const FIXTURE: &str = include_str!("../../../../verify/fixtures/fixed-bars.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Fixture {
    lane_seconds: f64,
    rows: Vec<Row>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Row {
    rate: u32,
    bpm: u32,
    bars: Frame,
    master: Frame,
    requested: Frame,
    max_bars: Frame,
    take_bars: Frame,
    take_frames: Frame,
}

/// A silent one-lane session of `row`'s master, loaded into an engine at its rate: lane 0 plays it.
fn loaded(row: &Row, lane_seconds: f64) -> Rig {
    let mut rig = Rig::with(Opts { sr: row.rate, start: row.rate as Frame, loop_seconds: lane_seconds, ..Opts::default() });
    let mut buf = Vec::with_capacity(rig.engine.looper().capacity() as usize);
    buf.resize(rig.engine.looper().capacity() as usize, 0.0f32);
    let track = LoadTrack { index: 0, buf, peaks: Vec::new(), reversed: false, playing: true, mix: LaneMix::default() };
    let load = Load { bpm: row.bpm, meter: Meter::FOUR_FOUR, bars: row.bars, master: row.master, tracks: vec![track], result: None };
    assert!(rig.session().send(Box::new(SessionJob::Load(load))).is_ok(), "the port takes the load");
    for _ in 0..1000 {
        rig.advance(rig.block as Frame);
        if let Some(job) = rig.session().returned() {
            let SessionJob::Load(load) = *job else { panic!("a load came back as a snapshot") };
            assert_eq!(load.result, Some(Ok(())), "the row's master loads");
            return rig;
        }
    }
    panic!("the load never came back");
}

#[test]
fn the_engine_answers_every_fixed_bars_row_the_ui_is_held_to() {
    let fixture: Fixture = serde_json::from_str(FIXTURE).expect("the fixture parses");
    assert!(fixture.rows.len() >= 8, "the fixture has its rows");
    // The host builds every engine with this lane buffer (`HostConfig`, the engine's default); the UI
    // bounds a multiply by the same seconds (`LANE_BUFFER_SECONDS`).
    assert_eq!(EngineConfig::new(48_000).max_loop_seconds, fixture.lane_seconds);
    for row in &fixture.rows {
        let tag = format!("{} Hz, {} BPM, a {}-bar master, FIXED {}", row.rate, row.bpm, row.bars, row.requested);
        let fpb = frames_per_bar(row.bpm as f64, row.rate, Meter::FOUR_FOUR);
        assert_eq!(row.bars * fpb, row.master, "{tag}: the row's master is its bars");
        assert_eq!(row.take_bars * fpb, row.take_frames, "{tag}: the row's window is its bars");
        let mut rig = loaded(row, fixture.lane_seconds);
        assert_eq!((rig.master(), rig.bpm()), (row.master, row.bpm), "{tag}: the master the row names");
        assert_eq!(rig.engine.looper().next_take_max_bars(rig.bpm(), rig.meter()), row.max_bars, "{tag}: the selector's bound");
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(row.requested as f64));
        rig.press(Command::RecDub(1));
        assert_eq!(rig.end_frame() - rig.start_frame(), row.take_frames, "{tag}: the window FIXED arms");
    }
}
