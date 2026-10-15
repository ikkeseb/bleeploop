//! The routing proof (`api::damage`): random scripts of damaged blocks, live slots through pure delays
//! of every latency (0, 64, 1,024 and 115,200 frames: past a second of the wet line), GO LIVE on and off
//! (same-frame pairs, reversals inside a ramp), unit installs and removals (an effect, an instrument),
//! and captures (overdubs, a later take), each run twice: once with every damaged block's input silenced
//! and flagged, as the device side renders it, once clean with no flag. Then, run by run:
//!
//! 1. the musical decisions are the same: both runs' Lane events are equal;
//! 2. the record tap's damage is `LOST` exactly where the two taps differ;
//! 3. every lane's marks equal an oracle's: the tapped damage of each window, routed through the window's
//!    position map (a take's `f - s`, a layer's loop position) by the window rule, into bins;
//! 4. every bin whose PCM differs between the runs is marked: no miss;
//! 5. each run's undo target is its own loop before that DUB;
//! 6. a `Dropout` goes to exactly the windows the oracle marks.
//!
//! What it keeps out, so that both directions hold exactly: AUTO (its onset depends on the input), jumps
//! and point xruns (they change no input), slot gain changes (their glides reach weights far under a
//! frame code's step), RETAKE rolls (`tests/retake.rs`), and every frame before 2^20, so each frame code
//! is at least 1/8 and every nonzero path weight shows in f32. A routing proof for the processors here:
//! for a plugin the mark is its report's word (`slots` module doc).

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::{code, Delay, Opts, Rig};
use lf_engine::api::damage;
use lf_engine::grid::Frame;
use lf_engine::overview::PEAK_FRAMES;
use lf_engine::{Command, Event, LaneState, SlotEvent, SlotKind, SlotProcessor};
use proptest::prelude::*;

const SR: u32 = 48_000;
/// Every frame from here has a code of at least 1/8.
const START: Frame = 1 << 20;
const PF: Frame = PEAK_FRAMES as Frame;

/// An instrument plugin stand-in: a fixed hum whatever the notes, `latency` reported.
struct Hum {
    latency: Frame,
}

impl SlotProcessor for Hum {
    fn kind(&self) -> SlotKind {
        SlotKind::Instrument
    }
    fn latency(&self) -> Frame {
        self.latency
    }
    fn process(&mut self, frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
        for (k, o) in out.iter_mut().enumerate() {
            *o = 0.05 * ((frame + k as Frame) % 97) as f32 / 97.0;
        }
    }
    fn stop(&mut self) {}
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

#[derive(Clone, Copy, Debug)]
enum Unit {
    Delay(Frame),
    Hum(Frame),
}

impl Unit {
    fn build(self) -> Box<dyn SlotProcessor> {
        match self {
            Unit::Delay(l) => Box::new(Delay::new(l)),
            Unit::Hum(l) => Box::new(Hum { latency: l }),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Step {
    /// The next blocks covering this many frames are damaged, their input silenced in the damaged run.
    Damage(Frame),
    Live(u8, bool),
    /// GO LIVE flipped and flipped back on the same frame.
    LivePair(u8),
    /// GO LIVE flipped, then back this many frames later, inside its 5 ms ramp.
    LiveReversal(u8, Frame),
    Install(u8, Unit),
    Remove(u8),
    /// An overdub on lane 0 for this many frames.
    Dub(Frame),
    /// A later take on lane 1, one loop long.
    Take,
    Wait(Frame),
}

fn latency() -> impl Strategy<Value = Frame> {
    prop_oneof![Just(0), Just(64), Just(1024), Just(115_200)]
}

fn unit() -> impl Strategy<Value = Unit> {
    prop_oneof![3 => latency().prop_map(Unit::Delay), 1 => latency().prop_map(Unit::Hum)]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => (1i64..600).prop_map(Step::Damage),
        2 => (0u8..2, any::<bool>()).prop_map(|(s, on)| Step::Live(s, on)),
        1 => (0u8..2).prop_map(Step::LivePair),
        1 => (0u8..2, 1i64..200).prop_map(|(s, d)| Step::LiveReversal(s, d)),
        1 => (0u8..2, unit()).prop_map(|(s, u)| Step::Install(s, u)),
        1 => (0u8..2).prop_map(Step::Remove),
        4 => (300i64..40_000).prop_map(Step::Dub),
        1 => Just(Step::Take),
        5 => (1i64..30_000).prop_map(Step::Wait),
    ]
}

/// One capture window as the oracle sees it: its lane, kind, frames and how a frame maps to a position.
#[derive(Clone, Copy, Debug)]
struct Window {
    lane: usize,
    overdub: bool,
    s: Frame,
    e: Frame,
    /// A layer's loop position of frame `s`.
    first: Frame,
}

struct Run {
    /// The frame the kept tap starts on.
    from: Frame,
    lanes: Vec<Event>,
    record: Vec<f32>,
    damage: Vec<u8>,
    windows: Vec<Window>,
    pcm: [Vec<f32>; 2],
    marks: [Vec<usize>; 2],
    dropouts: Vec<(u8, bool)>,
    master: Frame,
    undo_ok: bool,
}

/// Run `steps` from a 2-bar loop at 240 BPM on lane 0 (96,000 frames), slot 0 holding `slot0` live, slot
/// 1 holding `slot1` live or not; `damaged`: flag the damaged blocks and silence their input.
fn run(slot0: Option<Unit>, slot1: Option<Unit>, live1: bool, steps: &[Step], block: usize, damaged: bool) -> Run {
    let mut rig = Rig::with(Opts { sr: SR, start: START, loop_seconds: 20.0, block, align: 0 });
    let silent: Rc<RefCell<Vec<(Frame, Frame)>>> = Rc::new(RefCell::new(Vec::new()));
    let spans = silent.clone();
    rig.set_input(move |f| if spans.borrow().iter().any(|&(a, b)| (a..b).contains(&f)) { 0.0 } else { code(f) });
    let mut held = [slot0.is_some(), slot1.is_some()];
    for (k, u) in [slot0, slot1].into_iter().enumerate() {
        if let Some(u) = u {
            rig.install(k, u.build());
        }
    }
    rig.set(Command::SetSlotLive(1, live1));
    rig.set(Command::SetBpm(240.0));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(2.0));
    rig.keep_output();
    let from = rig.frame;
    let mut windows = Vec::new();
    let mut undo_ok = true;
    // The first take, its window whole.
    rig.press(Command::RecDub(0));
    let (s, e) = (rig.start_frame(), rig.end_frame());
    windows.push(Window { lane: 0, overdub: false, s, e, first: 0 });
    rig.advance_to(e + 1);
    rig.idle();
    let master = rig.master();
    let mut live = [true, live1];
    for &step in steps {
        // Units come back from a removal on their own; the slot takes a new one once its last is back.
        for (k, h) in held.iter_mut().enumerate() {
            if rig.returned(k).is_some() {
                *h = false;
            }
        }
        match step {
            Step::Damage(n) => {
                let to = rig.frame + n;
                while rig.frame < to {
                    let b = (rig.block as Frame).min(to - rig.frame);
                    if damaged {
                        silent.borrow_mut().push((rig.frame, rig.frame + b));
                        rig.damage();
                    }
                    rig.advance(b);
                }
            }
            Step::Live(slot, on) => {
                live[slot as usize] = on;
                rig.press(Command::SetSlotLive(slot, on));
            }
            Step::LivePair(slot) => {
                let k = slot as usize;
                rig.send_at(rig.frame, Command::SetSlotLive(slot, !live[k]));
                rig.send_at(rig.frame, Command::SetSlotLive(slot, live[k]));
                rig.advance(1);
            }
            Step::LiveReversal(slot, d) => {
                let k = slot as usize;
                rig.send_at(rig.frame, Command::SetSlotLive(slot, !live[k]));
                rig.send_at(rig.frame + d, Command::SetSlotLive(slot, live[k]));
                rig.advance(d + 1);
            }
            Step::Install(slot, u) => {
                if !held[slot as usize] {
                    held[slot as usize] = true;
                    rig.install(slot as usize, u.build());
                    rig.advance(1);
                }
            }
            Step::Remove(slot) => {
                if held[slot as usize] {
                    rig.remove(slot as usize);
                    rig.advance(1);
                }
            }
            Step::Dub(n) => {
                if rig.window().is_some() || rig.state(0) != LaneState::Playing {
                    continue;
                }
                let before = rig.pcm(0);
                let press = rig.frame;
                rig.press(Command::RecDub(0));
                let Some((0, Some(s), None)) = rig.window() else { continue };
                let first = (press - rig.anchor()).rem_euclid(master);
                rig.advance(n);
                let stop = rig.frame;
                rig.press(Command::RecDub(0));
                let e = stop + (s - press);
                rig.advance_to(e + 1);
                rig.idle();
                windows.push(Window { lane: 0, overdub: true, s, e, first });
                undo_ok &= rig.engine.looper().undo_pcm(0).as_deref() == Some(&before[..]);
            }
            Step::Take => {
                if rig.window().is_some() || rig.state(1) != LaneState::Empty {
                    continue;
                }
                rig.set(Command::SetFixedBars(2.0));
                rig.press(Command::RecDub(1));
                let (s, e) = (rig.start_frame(), rig.end_frame());
                rig.advance_to(e + 1);
                rig.idle();
                windows.push(Window { lane: 1, overdub: false, s, e, first: 0 });
            }
            Step::Wait(n) => rig.advance(n),
        }
    }
    rig.advance(4096);
    rig.idle();
    let lanes = rig.events.iter().filter(|e| matches!(e, Event::Lane { .. })).copied().collect();
    let dropouts = rig.dropouts().iter().map(|d| (d.1, d.2)).collect();
    Run {
        from,
        lanes,
        record: rig.record.clone(),
        damage: rig.record_damage.clone(),
        windows,
        pcm: [rig.pcm(0), rig.pcm(1)],
        marks: [rig.marks(0), rig.marks(1)],
        dropouts,
        master,
        undo_ok,
    }
}

/// The oracle: each window's tapped damage through the window rule and its position map, into bins.
/// Returns each lane's bins and, per window, whether it marked anything.
fn oracle(run: &Run, from: Frame) -> ([Vec<usize>; 2], Vec<bool>) {
    let mut bins: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
    let mut marked = Vec::new();
    for w in &run.windows {
        let mut any = false;
        for f in w.s..w.e {
            let d = run.damage[(f - from) as usize];
            let counts = d & (damage::LOST | damage::TAIL) != 0 || (d & damage::BREAK != 0 && f > w.s);
            if counts {
                any = true;
                let p = if w.overdub { (w.first + f - w.s).rem_euclid(run.master) } else { f - w.s };
                bins[w.lane].push((p / PF) as usize);
            }
        }
        marked.push(any);
    }
    for b in bins.iter_mut() {
        b.sort_unstable();
        b.dedup();
    }
    (bins, marked)
}

fn check(slot0: Option<Unit>, slot1: Option<Unit>, live1: bool, steps: &[Step], block: usize) -> Result<(), TestCaseError> {
    let clean = run(slot0, slot1, live1, steps, block, false);
    let hit = run(slot0, slot1, live1, steps, block, true);
    // 1. The same decisions.
    prop_assert_eq!(&hit.lanes, &clean.lanes, "the Lane events");
    prop_assert_eq!(hit.windows.len(), clean.windows.len());
    // 2. LOST exactly where the taps differ.
    prop_assert_eq!(hit.record.len(), clean.record.len());
    for (k, ((a, b), d)) in hit.record.iter().zip(&clean.record).zip(&hit.damage).enumerate() {
        prop_assert_eq!(a != b, d & damage::LOST != 0, "tap frame {}: {} vs clean {}, damage {}", k, a, b, d);
    }
    prop_assert!(clean.damage.iter().all(|&d| d == 0), "the clean run carries no damage");
    // 3. The marks are the oracle's.
    let (bins, marked) = oracle(&hit, hit.from);
    prop_assert_eq!(&hit.marks, &bins, "each lane's marks are the oracle's");
    prop_assert!(clean.marks.iter().all(|m| m.is_empty()));
    // 4. No miss: every bin whose PCM differs is marked.
    for lane in 0..2 {
        let (a, b) = (&hit.pcm[lane], &clean.pcm[lane]);
        prop_assert_eq!(a.len(), b.len());
        for (p, (x, y)) in a.iter().zip(b).enumerate() {
            if x != y {
                prop_assert!(hit.marks[lane].contains(&(p / PEAK_FRAMES)), "lane {} position {} differs and its bin is unmarked", lane, p);
            }
        }
    }
    // 5. Each layer's undo target is its own loop before the DUB.
    prop_assert!(hit.undo_ok && clean.undo_ok, "an undo target that is not the loop before its layer");
    // 6. A Dropout for exactly the windows the oracle marks.
    let want: Vec<(u8, bool)> = hit.windows.iter().zip(&marked).filter(|(_, &m)| m).map(|(w, _)| (w.lane as u8, w.overdub)).collect();
    prop_assert_eq!(&hit.dropouts, &want, "the Dropouts");
    prop_assert!(clean.dropouts.is_empty());
    Ok(())
}

#[test]
fn a_damaged_frame_on_a_layers_first_frame_is_marked_and_changes_no_sample() {
    // The punch-in's first frame writes the loop as it was (`punch_in`): the mark is the routing claim,
    // and the PCM is the clean run's.
    for block in [64, 127] {
        let one = |damaged: bool| {
            let mut rig = Rig::with(Opts { sr: SR, start: START, loop_seconds: 20.0, block, align: 0 });
            rig.set_input(code);
            rig.set(Command::SetBpm(240.0));
            rig.set(Command::SetFixedLength(true));
            rig.set(Command::SetFixedBars(2.0));
            rig.press(Command::RecDub(0));
            rig.advance_to(rig.end_frame() + 1);
            rig.idle();
            rig.advance(4000);
            rig.block = 1;
            let s = rig.frame;
            rig.send_at(s, Command::RecDub(0));
            if damaged {
                rig.damage();
            }
            rig.advance(1);
            rig.block = block;
            rig.advance(5000);
            rig.press(Command::RecDub(0));
            rig.advance(2000);
            rig.idle();
            ((s - rig.anchor()).rem_euclid(rig.master()), rig.pcm(0), rig.marks(0), rig.dropouts().len())
        };
        let ((first, clean, none, zero), (_, hit, marks, said)) = (one(false), one(true));
        assert!(none.is_empty() && zero == 0);
        assert_eq!(hit, clean, "block {block}: the PCM is the clean run's");
        assert_eq!((marks, said), (vec![(first / PF) as usize], 1), "block {block}: its first frame's bin, said once");
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn the_damage_lane_routes_every_dropout_to_the_bins_it_reaches(
        slot0 in proptest::option::of(latency().prop_map(Unit::Delay)),
        slot1 in proptest::option::of(unit()),
        live1 in any::<bool>(),
        steps in proptest::collection::vec(step(), 1..24),
    ) {
        for block in [64, 127] {
            check(slot0, slot1, live1, &steps, block)?;
        }
    }
}
