//! A dropout keeps what it lands in and marks the bar (`api::damage`, `Overview::marked`): a starved or
//! spliced input block, a point xrun or a jump in the device frame, dry or through a unit's latency and
//! tail, the instruments' record path or the input sends, never changes or discards a sample of a take,
//! an overdub layer or a RETAKE pass. A span the device never delivered is silence in a take and the old
//! loop in a layer. The capture marks the bins those frames landed in; the marks travel with the audio
//! (UNDO, REDO, a fill's tiles, TRIM, a multiply, COPY and WIDE COPY's shift, REVERSE) and go with it
//! (CLEAR, a load). One `Dropout` per window says so. RETAKE's own cases are `tests/retake.rs`, a layer's
//! punch ramps `tests/punch_ramps.rs`, the routing proof `tests/dropout_tap.rs`.

mod common;

use std::f64::consts::TAU;

use common::dub::{pos_fn, ramp};
use common::{code, frame_of, Delay, Opts, Rig};
use lf_engine::api::damage;
use lf_engine::grid::{frames_per_bar, Frame, Meter};
use lf_engine::input_fx::InputFx;
use lf_engine::looper::JOB_RATE;
use lf_engine::overview::PEAK_FRAMES;
use lf_engine::{Command, InputSend, InputSendParam, Instrument, LaneState, Load, LoadTrack, NoteTarget, SessionError, SessionJob, SlotEvent, SlotKind, SlotProcessor, Snapshot};

const PF: Frame = PEAK_FRAMES as Frame;

/// The bins a mark on buffer bin `b` reaches when its frames move by `offset`: a mark is a bin's
/// precision, so its image covers the (at most two) bins those frames land in.
fn image(b: usize, offset: Frame) -> Vec<usize> {
    bins(b as Frame * PF + offset, PF, Frame::MAX)
}

/// The bins of `n` loop positions from `first`, wrapping at `master`, sorted.
fn bins(first: Frame, n: Frame, master: Frame) -> Vec<usize> {
    let mut out: Vec<usize> = (0..n).map(|k| ((first + k).rem_euclid(master) / PF) as usize).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// A toy amp: a 256-frame delay into a 15 ms one-pole ring (`y = (1 - a)·x + a·y`). Its tail is the
/// ring's −60 dB time, computed from its own coefficient: what it reports is its real memory.
struct Tail {
    line: Vec<f32>,
    next: usize,
    a: f32,
    y: f32,
    tail: Frame,
}

impl Tail {
    const LATENCY: Frame = 256;

    fn new(sr: u32) -> Self {
        let a = (-1.0 / (0.015 * sr as f64)).exp();
        Tail { line: vec![0.0; Self::LATENCY as usize], next: 0, a: a as f32, y: 0.0, tail: (0.001f64.ln() / a.ln()).ceil() as Frame }
    }
}

impl SlotProcessor for Tail {
    fn kind(&self) -> SlotKind {
        SlotKind::Effect
    }
    fn latency(&self) -> Frame {
        Self::LATENCY
    }
    fn tail(&self) -> Frame {
        self.tail
    }
    fn process(&mut self, _frame: Frame, input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
        for (o, &x) in out.iter_mut().zip(input) {
            let d = std::mem::replace(&mut self.line[self.next], x);
            self.next = (self.next + 1) % self.line.len();
            self.y = (1.0 - self.a) * d + self.a * self.y;
            *o = self.y;
        }
    }
    fn stop(&mut self) {}
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// A unit that ignores its input: silence until device frame `from`, then a 440 Hz tone at `level`,
/// whatever comes in. It reports no latency and no tail, so a dropout through it is marked on its own
/// frames: a false mark, pinned.
struct Ignore {
    from: Frame,
    level: f32,
}

impl SlotProcessor for Ignore {
    fn kind(&self) -> SlotKind {
        SlotKind::Effect
    }
    fn latency(&self) -> Frame {
        0
    }
    fn process(&mut self, frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
        for (k, o) in out.iter_mut().enumerate() {
            let f = frame + k as Frame;
            *o = if f < self.from { 0.0 } else { self.level * (TAU * 440.0 * f as f64 / 48_000.0).sin() as f32 };
        }
    }
    fn stop(&mut self) {}
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// A one-bar loop at 200 BPM on lane 0, the frame code; then silence.
fn playing_loop(o: Opts) -> (Rig, Frame) {
    let mut rig = Rig::with(o);
    rig.set(Command::SetBpm(200.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    (rig, master)
}

/// Render `n` frames as damaged blocks of the rig's size (each block's input never wholly arrived).
fn damaged(rig: &mut Rig, n: Frame) {
    let end = rig.frame + n;
    while rig.frame < end {
        rig.damage();
        rig.advance((rig.block as Frame).min(end - rig.frame));
    }
}

// ── 1. A layer keeps a dropout as recorded; UNDO takes the layer and its mark ───────────────────────

#[test]
fn a_dropout_in_a_layer_stays_as_recorded_and_marks_its_bin() {
    let (mut rig, master) = playing_loop(Opts::default());
    let before = rig.pcm(0);
    let start = rig.next_boundary() + master / 4;
    let (d, end) = (start + master / 3, start + master / 2);
    // The block the device rendered from silence: what the tap delivers there.
    let input = move |f: Frame| if (d..d + 128).contains(&f) { 0.0 } else { code(f) / 4.0 };
    rig.set_input(input);
    rig.send_at(start, Command::RecDub(0));
    rig.send_at(end, Command::RecDub(0));
    rig.advance_to(d);
    damaged(&mut rig, 128);
    rig.advance_to(end + 1);
    rig.set_level(0.0);
    rig.idle();
    let pos = pos_fn(rig.anchor(), master, 0);
    let mut want = before.clone();
    common::dub::dub(&mut want, (start, end), ramp(rig.sr), 1.0, &pos, input);
    assert_eq!(rig.pcm(0), want, "the layer as the tap delivered it, bit for bit");
    let marked = bins(pos(d) as Frame, 128, master);
    assert_eq!(rig.marks(0), marked, "the block's bins");
    assert_eq!(rig.dropouts(), [(end, 0, true)], "one Dropout, at the layer's close");
    rig.press(Command::Undo(0));
    assert!(rig.pcm(0) == before && rig.marks(0).is_empty(), "UNDO: the loop before the layer, no mark");
    rig.advance(master / JOB_RATE + 2);
    rig.press(Command::Undo(0));
    assert!(rig.pcm(0) == want && rig.marks(0) == marked, "REDO brings both back");
    // A clean layer over the marked loop: its undo target is the loop at its start, marks included.
    rig.advance(master / JOB_RATE + 2);
    let again = rig.next_boundary() + master / 4;
    rig.set_input(|f| code(f) / 8.0);
    rig.send_at(again, Command::RecDub(0));
    rig.send_at(again + master / 2, Command::RecDub(0));
    rig.advance_to(again + master / 2 + 1);
    rig.set_level(0.0);
    rig.idle();
    assert!(rig.pcm(0) != want && rig.marks(0) == marked, "the clean layer keeps the bar's mark");
    assert_eq!(rig.dropouts().len(), 1, "and says nothing");
    rig.press(Command::Undo(0));
    assert!(rig.pcm(0) == want && rig.marks(0) == marked, "UNDO: the marked layer, its mark with it");
}

// ── 3. A dropout the latency carries past a window's end leaves it unmarked ─────────────────────────

#[test]
fn a_dropout_in_a_windows_last_latency_leaves_it_unmarked() {
    // Delay(1024): the input frames of the window's last 1,024 reach the tap after it closes.
    let run = |damage: bool| {
        let mut rig = Rig::new();
        rig.install(0, Box::new(Delay::new(1024)));
        rig.set(Command::SetBpm(200.0));
        rig.set_input(code);
        let master = rig.record_first_take(0, 1, 2400);
        rig.set_level(0.0);
        rig.idle();
        let press = rig.next_boundary() + master / 4;
        rig.send_at(press, Command::RecDub(0));
        rig.send_at(press + master / 3, Command::RecDub(0));
        rig.advance_to(press + 1);
        let (_, s, e) = rig.window().unwrap();
        let e = e.unwrap_or(press + master / 3 + 1024);
        assert_eq!(s, Some(press + 1024), "the window opens the latency after the press");
        let d = e - 1024 + 300;
        rig.set_input(move |f| if damage && (d..d + 128).contains(&f) { 0.0 } else { code(f) / 4.0 });
        rig.advance_to(d);
        if damage {
            damaged(&mut rig, 128);
        }
        rig.advance_to(e + 2048);
        rig.idle();
        (rig.pcm(0), rig.marks(0), rig.dropouts())
    };
    let (clean, damaged_run) = (run(false), run(true));
    assert!(damaged_run.1.is_empty() && damaged_run.2.is_empty(), "no LOST inside the window: no mark, no Dropout");
    assert_eq!(damaged_run.0, clean.0, "the PCM is the clean run's");
}

// ── 4. A first take keeps its planned bars ──────────────────────────────────────────────────────────

#[test]
fn a_first_take_with_a_dropout_keeps_its_planned_bars() {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(120.0));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(2.0));
    rig.press(Command::RecDub(0));
    let (s, e) = (rig.start_frame(), rig.end_frame());
    let d = s + 50_000;
    rig.set_input(move |f| if (d..d + 256).contains(&f) { 0.0 } else { code(f) });
    rig.advance_to(d);
    damaged(&mut rig, 256);
    rig.advance_to(e + 1);
    rig.idle();
    let fpb = frames_per_bar(120.0, 48_000, Meter::FOUR_FOUR);
    assert_eq!((rig.state(0), rig.master()), (LaneState::Playing, 2 * fpb), "the master as planned");
    let pcm = rig.pcm(0);
    assert!(pcm.iter().enumerate().all(|(k, &x)| x == if (50_000..50_256).contains(&k) { 0.0 } else { code(s + k as Frame) }), "as recorded");
    assert_eq!(rig.marks(0), bins(50_000, 256, 2 * fpb));
    assert_eq!(rig.dropouts(), [(e, 0, false)]);
    rig.press(Command::Clear(0));
    rig.idle();
    assert!(rig.state(0) == LaneState::Empty && rig.master() == 0 && rig.marks(0).is_empty(), "CLEAR takes it");
}

// ── 6. The point-gap rule ───────────────────────────────────────────────────────────────────────────

/// What lands on tap frame `at` of an overdub window: a point gap (BREAK), a one-frame damaged block
/// (LOST), or both.
#[derive(Clone, Copy, Debug)]
enum Hit {
    Break,
    Lost,
    Both,
}

/// A 1/3-loop overdub on a 200 BPM loop, through `units` (slot 0, then slot 1, both live), and a hit
/// `hit` on the input frame `delay` frames before tap frame `s + offset` (`offset` −1, 0, 1 or `e - s`).
/// Returns the lane's marks and the window's first loop position and length.
fn point(units: Vec<Box<dyn SlotProcessor>>, delay: Frame, hit: Hit, offset: impl Fn(Frame, Frame) -> Frame) -> (Vec<usize>, Frame, Frame, Frame) {
    let two = units.len() == 2;
    let mut rig = Rig::new();
    for (k, unit) in units.into_iter().enumerate() {
        rig.install(k, unit);
    }
    if two {
        rig.set(Command::SetSlotLive(1, true));
    }
    let (master, s, e, first) = {
        rig.set(Command::SetBpm(200.0));
        rig.set_input(|f| code(f) / 4.0);
        let master = rig.record_first_take(0, 1, 2400);
        rig.idle();
        let press = rig.next_boundary() + master / 4;
        rig.send_at(press, Command::RecDub(0));
        rig.send_at(press + master / 3, Command::RecDub(0));
        // The window opens the live latency after each press (the rig's own alignment is 0).
        let align = rig.engine.live_latency();
        (master, press + align, press + master / 3 + align, (press - rig.anchor()).rem_euclid(master))
    };
    let at = s + offset(s, e) - delay;
    rig.advance_to(at);
    rig.block = 1;
    match hit {
        Hit::Break => rig.gap(),
        Hit::Lost => rig.damage(),
        Hit::Both => {
            rig.gap();
            rig.damage();
        }
    }
    rig.advance(1);
    rig.block = 128;
    rig.advance_to(e + 2048);
    rig.idle();
    assert_eq!(rig.state(0), LaneState::Playing);
    (rig.marks(0), first, s, master)
}

#[test]
fn a_point_gap_marks_only_strictly_inside() {
    let delay = |units: &str| -> (Vec<Box<dyn SlotProcessor>>, Frame) {
        match units {
            "dry" => (vec![], 0),
            "Delay(700)" => (vec![Box::new(Delay::new(700))], 700),
            // Slot 0 dry and slot 1 Delay(700), both live: slot 0's wet waits 700 on its line.
            _ => (vec![Box::new(Delay::new(0)), Box::new(Delay::new(700))], 700),
        }
    };
    for units in ["dry", "Delay(700)", "dry + Delay(700)"] {
        for (name, hit, offset, marks) in [
            ("BREAK on s - 1", Hit::Break, -1, false),
            ("BREAK on s", Hit::Break, 0, false),
            ("BREAK on e", Hit::Break, Frame::MAX, false),
            ("BREAK on s + 1", Hit::Break, 1, true),
            ("LOST on s", Hit::Lost, 0, true),
            ("LOST and BREAK on s", Hit::Both, 0, true),
        ] {
            let (list, d) = delay(units);
            let (got, first, _, master) = point(list, d, hit, move |s, e| if offset == Frame::MAX { e - s } else { offset });
            let want = if marks { bins(first + offset.max(0), 1, master) } else { vec![] };
            assert_eq!(got, want, "{units}: {name}");
        }
    }
    // Through memory the splice rings on inside the window: a BREAK on s starts the unit's TAIL there.
    let tail = Tail::new(48_000);
    let t = tail.tail;
    let (got, first, _, master) = point(vec![Box::new(tail)], Tail::LATENCY, Hit::Break, |_, _| 0);
    assert_eq!(got, bins(first + 1, t, master), "Tail: a BREAK on s marks the tail after it");
}

// ── 7. The marks follow the audio ───────────────────────────────────────────────────────────────────

/// A 4-bar master at 240 BPM and 48 kHz (a bar of 48,000 frames) on lane 0, silent.
fn four_bars(align: Frame, block: usize) -> Rig {
    let mut rig = Rig::with(Opts { align, block, ..Opts::default() });
    rig.set(Command::SetBpm(240.0));
    rig.set_level(0.25);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(4.0));
    rig.press(Command::RecDub(0));
    rig.advance_to(rig.end_frame() + 1);
    rig.idle();
    assert_eq!(rig.master(), 192_000);
    rig
}

#[test]
fn a_fill_sets_its_tiles_from_the_marks_it_read_whatever_the_visit_order() {
    // The critique's case: a 1-bar later take tiled over a 4-bar master in place, only its first bin
    // marked. Its images are the bins its frames reach in each tile; nothing it set is read again.
    for block in [1usize, 64, 127] {
        for align in [0, 2000] {
            let mut rig = four_bars(align, block);
            rig.set(Command::SetFixedBars(1.0));
            rig.set_input(code);
            rig.press(Command::RecDub(1));
            let (s, e) = (rig.start_frame(), rig.end_frame());
            rig.advance_to(s);
            damaged(&mut rig, 128);
            rig.advance_to(e + 1);
            rig.idle();
            assert_eq!(rig.marks(1), [0, 46, 47, 93, 94, 140, 141], "block {block}, align {align}");
            assert!(rig.pcm(1).iter().enumerate().all(|(p, &x)| x == code(s + p as Frame % 48_000)), "the bar tiled as recorded");
        }
    }
}

#[test]
fn a_trim_a_multiply_copy_and_clear_carry_the_marks_with_the_audio() {
    // Lane 0: 4 bars, a dropout in bar 1 ([10_000, 10_128)) and one in bar 3 ([100_000, 100_128)).
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(240.0));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(4.0));
    rig.set_input(code);
    rig.press(Command::RecDub(0));
    let s = rig.start_frame();
    for at in [10_000, 100_000] {
        rig.advance_to(s + at);
        damaged(&mut rig, 128);
    }
    rig.advance_to(rig.end_frame() + 1);
    rig.set_level(0.0);
    rig.idle();
    let (one, three) = (bins(10_000, 128, 192_000), bins(100_000, 128, 192_000));
    assert_eq!(rig.marks(0), [one.clone(), three.clone()].concat());

    // COPY takes them with the audio.
    rig.press(Command::Copy(0));
    rig.idle();
    assert_eq!((rig.pcm(1), rig.marks(1)), (rig.pcm(0), rig.marks(0)), "COPY");

    // TRIM to 2 bars keeps bar 1's mark, repeated on bar 3 (the bin's image), and cuts bar 3's own.
    rig.press(Command::Trim(0, 2));
    rig.idle();
    let tiled: Vec<usize> = [one.clone(), image(one[0], 96_000)].concat();
    assert_eq!(rig.marks(0), tiled, "TRIM: the kept bar's mark per tile, the cut bar's gone");
    rig.press(Command::Undo(0));
    rig.idle();
    assert_eq!(rig.marks(0), [one.clone(), three.clone()].concat(), "UNDO of the TRIM gives them back");

    // A multiply to 8 bars tiles lane 1's loop and its marks; lane 0's loop and its undo target too.
    rig.set(Command::SetFixedBars(8.0));
    rig.press(Command::RecDub(2));
    rig.advance_to(rig.end_frame() + 1);
    rig.idle();
    assert_eq!(rig.master(), 384_000);
    let twice = |marks: &[usize]| {
        let mut out: Vec<usize> = marks.iter().flat_map(|&b| [vec![b], image(b, 192_000)].concat()).collect();
        out.sort_unstable();
        out.dedup();
        out
    };
    let want = twice(&[one.clone(), three.clone()].concat());
    assert_eq!(rig.marks(1), want, "the multiply tiles lane 1's marks");
    assert_eq!(rig.marks(0), want, "and lane 0's");
    rig.press(Command::Undo(0));
    rig.idle();
    assert_eq!(rig.marks(0), twice(&tiled), "its undo target's too");

    // CLEAR hides them; the buffer's next take starts with none.
    rig.press(Command::Clear(1));
    rig.idle();
    assert!(rig.marks(1).is_empty(), "CLEAR");
}

#[test]
fn a_reversed_trim_and_a_reversed_lane_put_the_mark_where_it_is_heard() {
    // A 2-bar loop at 240 BPM, 96,000 frames (not whole bins), a point gap on buffer frame 48,127:
    // forward it sits in bin 46 (bar 1's last bin); reversed it is heard at frame 47,872, in heard bin 46
    // (bar 1), where a bin-count reversal would put it in bin 47 (bar 2).
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(240.0));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(2.0));
    rig.set_input(code);
    rig.press(Command::RecDub(0));
    let s = rig.start_frame();
    rig.advance_to(s + 48_127);
    rig.gap();
    rig.advance_to(rig.end_frame() + 1);
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.marks(0), [46]);
    rig.press(Command::Reverse(0));
    rig.idle();
    // Buffer bin 46 plays heard frames [47_872, 48_896): heard bins 46 and 47, both shown (a mark is a
    // bin's precision, never a miss), heard bin 46 the one that holds the gap's heard frame 47,872.
    assert!((46 * PF..47 * PF).contains(&(96_000 - 1 - 48_127)));
    assert_eq!(rig.marks(0), [46, 47], "where it is heard, not one bin off as the bin count's reversal");
    // A reversed TRIM to one bar keeps heard bar 1, where the gap is heard, and tiles it: the gap's
    // heard frame in each tile (47,872 and 95,872) sits in a marked heard bin.
    rig.press(Command::Trim(0, 1));
    rig.idle();
    assert_eq!(rig.marks(0), [46, 47, 92, 93], "the reversed TRIM's tiles, in heard order");
}

/// Lane 0: 4 bars at 240 BPM and `sr` from the frame code, reversed or not, then WIDE COPY into lane 1.
/// `lost`: the 128-frame block that many frames into the take reaches the engine as silence, damaged.
fn wide_copied(sr: u32, reversed: bool, lost: Option<Frame>) -> Rig {
    let mut rig = Rig::at(sr);
    rig.set(Command::SetBpm(240.0));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(4.0));
    rig.set_input(code);
    rig.press(Command::RecDub(0));
    if let Some(at) = lost {
        let d = rig.start_frame() + at;
        rig.set_input(move |f| if (d..d + 128).contains(&f) { 0.0 } else { code(f) });
        rig.advance_to(d);
        damaged(&mut rig, 128);
    }
    rig.advance_to(rig.end_frame() + 1);
    rig.set_level(0.0);
    rig.idle();
    if reversed {
        rig.press(Command::Reverse(0));
        rig.advance_to(rig.next_boundary() + 1);
    }
    rig.press(Command::CopyWide(0));
    rig.idle();
    rig
}

#[test]
fn a_wide_copy_marks_every_frame_its_shifted_audio_took_from_a_dropout() {
    // Differential: the same take clean and with a lost block, both WIDE COPY'd. Every heard frame where
    // the two copies differ took its audio from the lost block, so its bin is marked, wherever the
    // shift (20 ms: 960 or 882 frames, never whole bins) carried it: inside a bin's run, across a bin
    // edge, across the loop edge, forward or reversed. The loop (187.5 or 172.3 bins) is not whole bins.
    let mut missed = vec![];
    for sr in [48_000u32, 44_100] {
        let m = 4 * sr as Frame;
        for reversed in [false, true] {
            // Where the lost block is heard: 144 frames into a bin, which the shift carries past the next
            // heard bin's start and, reversed, past the heard bins its buffer bin reaches
            // (`Overview::heard_marked`); and 300 frames before the loop's end, which it carries across
            // the loop edge (reversed, the buffer's start, which the buffer's shift wraps to its end).
            for heard in [50 * PF + 144, m - 300] {
                let at = if reversed { m - heard - 128 } else { heard };
                let tag = format!("{sr} Hz, reversed {reversed}, lost at {at}");
                let (clean, hit) = (wide_copied(sr, reversed, None), wide_copied(sr, reversed, Some(at)));
                assert_eq!((hit.master(), hit.lane(1).reversed, hit.state(1)), (m, reversed, LaneState::Playing), "{tag}");
                let mut differ = [vec![], vec![]];
                for lane in [0, 1] {
                    let (a, b, marks) = (clean.pcm(lane), hit.pcm(lane), hit.marks(lane));
                    differ[lane] = (0..a.len()).filter(|&h| a[h] != b[h]).collect::<Vec<usize>>();
                    assert!(!differ[lane].is_empty(), "{tag}, lane {lane}: the lost block changed the loop");
                    let unmarked: Vec<usize> = differ[lane].iter().copied().filter(|&h| !marks.contains(&(h / PEAK_FRAMES))).collect();
                    if !unmarked.is_empty() {
                        missed.push(format!("{tag}, lane {lane}: {} affected frames unmarked, from {:?} (marks {marks:?})", unmarked.len(), unmarked.first()));
                    }
                }
                let (src, copy) = (hit.marks(0), hit.marks(1));
                assert!(differ[1].iter().any(|&h| !src.contains(&(h / PEAK_FRAMES))), "{tag}: the shift moves the damage out of the source's marked bins");
                // A mark is a bin's precision: a shift that is not whole bins spreads a run over one more
                // bin, the loop's partial last bin one more where it wraps.
                assert!(copy.len() <= src.len() + 2, "{tag}: two bins over at most ({src:?} → {copy:?})");
            }
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n"));
}

#[test]
fn a_load_clears_the_marks_of_the_buffers_it_fills() {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(120.0));
    rig.set_input(code);
    rig.press(Command::RecDub(0));
    rig.idle();
    rig.advance_to(rig.start_frame() + 30_000);
    rig.gap();
    rig.advance(30_000);
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(0.5));
    rig.set_level(0.0);
    rig.idle();
    let master = rig.master();
    assert_eq!(rig.marks(0).len(), 1);
    let buf = rig.engine.looper().overview().lane(0).buf;
    rig.press(Command::ClearAll);
    rig.idle();
    assert_eq!(rig.buffer_marks(buf).len(), 1, "CLEAR ALL leaves the buffer's mark, unseen");
    let capacity = rig.engine.looper().capacity() as usize;
    let mut pcm = Vec::with_capacity(capacity);
    pcm.resize(capacity, 0.25f32);
    let peaks = pcm[..master as usize].chunks(PEAK_FRAMES).map(|_| (0.25f32, 0.25f32)).collect();
    let load = Load {
        bpm: 120,
        meter: Meter::FOUR_FOUR,
        bars: master / frames_per_bar(120.0, 48_000, Meter::FOUR_FOUR),
        master,
        tracks: vec![LoadTrack { index: 0, buf: pcm, peaks, reversed: false, playing: true, mix: lf_engine::LaneMix::default() }],
        result: None,
    };
    assert!(rig.session().send(Box::new(SessionJob::Load(load))).is_ok());
    let mut done = false;
    for _ in 0..10_000 {
        rig.advance(128);
        if let Some(job) = rig.session().returned() {
            let SessionJob::Load(load) = *job else { unreachable!() };
            assert_eq!(load.result, Some(Ok(())));
            done = true;
            break;
        }
    }
    assert!(done, "the load came back");
    assert_eq!(rig.state(0), LaneState::Playing);
    assert!(rig.marks(0).is_empty(), "a loaded lane shows no mark");
}

// ── 8. The precision at a take's kept end ───────────────────────────────────────────────────────────

#[test]
fn damage_past_a_takes_kept_end_shows_nothing_and_inside_its_last_bin_marks_it() {
    // A first take punched out 2.5 bars in commits 2 bars (120 BPM: 192,000 frames = 187.5 bins).
    // Damage a bin or more past the kept end is padded away: no mark. Damage inside the bin that
    // straddles the end marks that bin: the pinned precision, a false mark of at most a bin.
    for (at, want) in [(192_000 + PF, vec![]), (192_000 + 200, vec![187usize]), (191_800, vec![187])] {
        let mut rig = Rig::new();
        rig.set(Command::SetBpm(120.0));
        rig.set_input(code);
        rig.press(Command::RecDub(0));
        rig.idle();
        let downbeat = rig.start_frame();
        rig.advance_to(downbeat + at);
        rig.block = 1;
        rig.damage();
        rig.advance(1);
        rig.block = 128;
        rig.advance_to(downbeat + 240_000);
        rig.punch_out();
        rig.idle();
        assert_eq!(rig.master(), 192_000);
        assert_eq!(rig.marks(0), want, "damage at {at}");
    }
}

#[test]
fn a_later_take_tiled_at_its_commit_shows_its_mark_in_every_tile() {
    let mut rig = Rig::new();
    rig.set_level(0.5);
    rig.set(Command::SetBpm(120.0));
    rig.record_first_take(0, 2, 2400);
    rig.set_input(code);
    rig.press(Command::RecDub(1));
    let s = rig.start_frame();
    rig.advance_to(s + 20_000);
    damaged(&mut rig, 64);
    let fpb = rig.fpb();
    rig.advance_to(s + fpb + fpb / 2);
    rig.punch_out();
    rig.idle();
    let mut want = bins(20_000, 64, 2 * fpb);
    want.extend(image(want[0], fpb));
    assert_eq!(rig.marks(1), want, "bar 1 tiled over the 2-bar master, its mark's image with it");
}

// ── 9. Jumps: holes and the undo copy ───────────────────────────────────────────────────────────────

#[test]
fn every_hole_in_a_take_reads_silence_and_is_marked_on_a_buffer_an_earlier_take_filled() {
    for jumps in [1, 70] {
        let mut rig = Rig::new();
        rig.set(Command::SetBpm(120.0));
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(2.0));
        // An earlier take fills the buffer with the frame code, then CLEAR leaves it there.
        rig.set_input(code);
        rig.press(Command::RecDub(0));
        rig.advance_to(rig.end_frame() + 1);
        rig.idle();
        rig.press(Command::ClearAll);
        rig.idle();
        rig.set_level(0.5);
        rig.press(Command::RecDub(0));
        let (s, e) = (rig.start_frame(), rig.end_frame());
        rig.advance_to(s + 1000);
        let mut holes = Vec::new();
        for k in 0..jumps {
            let at = rig.frame;
            let n = 100 + 7 * k as Frame;
            rig.skip(n);
            holes.push((at - s, at - s + n));
            rig.advance(300);
        }
        rig.advance_to(e + 1);
        rig.idle();
        let pcm = rig.pcm(0);
        let mut want_marks = Vec::new();
        for &(a, b) in &holes {
            assert!(pcm[a as usize..b as usize].iter().all(|&x| x == 0.0), "{jumps} jumps: the hole [{a}, {b}) reads silence");
            want_marks.extend(bins(a, b - a, rig.master()));
        }
        want_marks.sort_unstable();
        want_marks.dedup();
        assert!(pcm.iter().enumerate().filter(|(k, _)| !holes.iter().any(|&(a, b)| (a..b).contains(&(*k as Frame)))).all(|(_, &x)| x == 0.5), "{jumps} jumps: the rest as recorded");
        assert_eq!(rig.marks(0), want_marks, "{jumps} jumps: every hole marked, nothing else");
        assert_eq!(rig.dropouts().len(), 1, "{jumps} jumps: one Dropout for the take");
    }
}

#[test]
fn a_hole_of_whole_bins_in_a_buffer_a_snapshot_pinned_says_the_loops_changed() {
    // A snapshot pins lane 0's loop; CLEAR and REC reuse its buffer before the copy is done, and a jump
    // skips the new take's whole window: its hole, four bars at 120 BPM from position 0 (375 whole
    // bins), is zeroed in the pinned buffer. The snapshot says the loops changed, never hands back the
    // old loop with silence in it.
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(120.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 4, 2400);
    assert_eq!(master % PF, 0, "the hole is whole bins");
    rig.press(Command::Copy(0)); // lane 1 keeps the master when lane 0 is cleared
    rig.idle();
    let pcm = vec![0.0f32; 2 * master as usize];
    assert!(rig.session().send(Box::new(SessionJob::Snapshot(Snapshot::new(pcm)))).is_ok());
    rig.advance(1); // it begins: lanes 0 and 1 pinned
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(4.0));
    rig.press(Command::Clear(0));
    rig.press(Command::RecDub(0));
    let (s, e) = (rig.start_frame(), rig.end_frame());
    assert_eq!(e - s, master);
    rig.skip(e + 64 - rig.frame);
    let mut back = None;
    for _ in 0..10_000 {
        rig.advance(rig.block as Frame);
        if let Some(job) = rig.session().returned() {
            back = Some(job);
            break;
        }
    }
    assert!(rig.pcm(0).iter().all(|&x| x == 0.0) && rig.state(0) == LaneState::Playing, "the take is its hole: silence, kept");
    let Some(SessionJob::Snapshot(s)) = back.map(|b| *b) else { panic!("no snapshot came back") };
    assert_eq!(s.result, Some(Err(SessionError::Changed)), "the hole wrote the loop it copied");
}

#[test]
fn a_jump_from_inside_a_layer_past_its_end_keeps_the_old_loop_there_marked_to_the_end() {
    // An alignment of 4,800: the stop press closes the window 4,800 frames later, and the jump starts
    // inside that tail and lands past it.
    const ALIGN: Frame = 4800;
    let (mut rig, master) = playing_loop(Opts { align: ALIGN, ..Opts::default() });
    let before = rig.pcm(0);
    rig.set_input(|f| code(f) / 4.0);
    let press = rig.next_boundary() + master / 4;
    let (start, end) = (press + ALIGN, press + master / 3 + ALIGN);
    rig.send_at(press, Command::RecDub(0));
    rig.send_at(end - ALIGN, Command::RecDub(0));
    let at = end - 2000;
    rig.advance_to(at);
    assert_eq!(rig.window(), Some((0, Some(start), Some(end))));
    rig.skip(5000); // lands 3,000 frames past the window's end
    rig.advance(4096);
    rig.set_level(0.0);
    rig.idle();
    let pos = pos_fn(rig.anchor(), master, ALIGN);
    let mut want = before.clone();
    for f in start..at {
        let p = pos(f);
        want[p] = common::dub::write(want[p], code(f) / 4.0, 1.0, f - start, end - f, ramp(rig.sr));
    }
    assert_eq!(rig.pcm(0), want, "written to the jump, the old loop after it");
    assert_eq!(rig.marks(0), bins(pos(at) as Frame, 2000, master), "marked from the jump to the window's end");
    assert_eq!(rig.dropouts(), [(at + 5000, 0, true)]);
}

#[test]
fn a_jump_of_three_and_a_half_passes_zeroes_two_passes_at_most() {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(120.0));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    rig.set(Command::SetRetake(true));
    rig.set_input(code);
    rig.press(Command::RecDub(0));
    let s = rig.start_frame();
    let len = rig.end_frame() - s;
    rig.advance_to(s + len + len / 4);
    rig.skip(len * 7 / 2);
    rig.advance(rig.block as Frame);
    let zeroed = rig.engine.looper().zeroed_max();
    assert!(zeroed > 0 && zeroed <= 2 * len, "zeroed {zeroed} positions in one callback, two passes are {}", 2 * len);
    assert_eq!(rig.lane(0).retake_pass, 5, "the roll slid past the passes the jump skipped whole");
}

// ── 10. Instruments and lines through a jump ────────────────────────────────────────────────────────

#[test]
fn a_held_lead_through_a_jump_marks_exactly_its_out_of_place_frames_with_sound() {
    let mut rig = Rig::new();
    rig.input_latency = 1000; // the record path's delay: 1,000 - LEAD on the line
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)));
    rig.press(Command::NoteOn(60, 0.8));
    rig.advance(4800);
    rig.skip(4800);
    rig.keep_output();
    let from = rig.frame;
    rig.advance(4000);
    let (rec, dmg) = (&rig.record, &rig.record_damage);
    for (k, (&x, &d)) in rec.iter().zip(dmg).enumerate() {
        // The line's own delay reads from before the jump; the LEAD frames the synth rendered first
        // after it reach the tap just after.
        let out_of_place = k < 1000 && x != 0.0;
        assert_eq!(d & damage::LOST != 0, out_of_place, "frame {} ({k} after the jump): {x}", from + k as Frame);
        assert_eq!(d & damage::TAIL, 0);
    }
    assert!(rec[..1000].iter().any(|&x| x != 0.0) && rec[1000..].iter().any(|&x| x != 0.0), "the note sounds throughout");
}

#[test]
fn a_line_holding_only_silence_displaces_nothing_through_a_jump() {
    // Slot 0 a silent live Delay(1024), slot 1 the guitar dry (its wet 1,024 late on its line), silent
    // until well after the jump: nothing out of place is marked.
    let mut rig = Rig::new();
    rig.install(0, Box::new(Delay::new(1024)));
    rig.set(Command::SetSlotLive(1, true));
    rig.advance(4096);
    rig.skip(4800);
    let back = rig.frame + 3000;
    rig.set_inputs(|_| 0.0, move |f| if f >= back { code(f) } else { 0.0 });
    rig.keep_output();
    rig.advance(8000);
    assert!(rig.record_damage.iter().all(|&d| d & damage::LOST == 0), "nothing out of place: only the jump's break");
    assert!(rig.record.iter().any(|&x| x != 0.0), "the guitar reaches the tap");
}

#[test]
fn a_relatch_after_a_jump_marks_where_the_line_plays_pre_jump_input() {
    // Slot 1 dry and live, the frame code; slot 0 holds Delay(1024), not live. A jump while nothing
    // captures, then GO LIVE on slot 0: the record latency relatches to 1,024 and slot 1's wet reads its
    // line 1,024 back, which holds input from before the jump for a while.
    let mut rig = Rig::new();
    rig.install(0, Box::new(Delay::new(1024)));
    rig.set(Command::SetSlotLive(0, false));
    rig.set(Command::SetSlotLive(1, true));
    rig.set_inputs(|_| 0.0, code);
    rig.advance(8192);
    let jump_from = rig.frame;
    rig.skip(4800);
    let landed = rig.frame;
    rig.advance(300);
    rig.press(Command::SetSlotLive(0, true));
    rig.keep_output();
    let from = rig.frame;
    rig.advance(4000);
    let mut seen_pre = 0;
    for (k, (&x, &d)) in rig.record.iter().zip(&rig.record_damage).enumerate() {
        let f = from + k as Frame;
        let src = frame_of(x, f);
        let pre = src < jump_from;
        seen_pre += pre as usize;
        assert_eq!(d & damage::LOST != 0, pre, "tap frame {f} plays input frame {src} (the jump went {jump_from} to {landed})");
    }
    assert!(seen_pre > 0, "the relatched line plays pre-jump input for a while");
}

// ── 11. Memory: a unit's tail, ECHO and REVERB carry the mark into a later window ──────────────────

/// A unit with no latency and no effect that reports `tail` frames of memory (`Frame::MAX`: infinite).
struct Memory(Frame);

impl SlotProcessor for Memory {
    fn kind(&self) -> SlotKind {
        SlotKind::Effect
    }
    fn latency(&self) -> Frame {
        0
    }
    fn tail(&self) -> Frame {
        self.0
    }
    fn process(&mut self, _frame: Frame, input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
        out.copy_from_slice(input);
    }
    fn stop(&mut self) {}
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// A FIXED loop of `bars` at `bpm` on lane 0 (silent), `setup` applied after, a damaged silent block of
/// 128 input frames at `d` (the input `level` elsewhere), `mid` applied just after it, then an overdub
/// whose window opens `after` frames past tap frame `from(d)` and runs 9,600 frames. Returns its marks
/// and its first loop position's bin.
fn window_after(o: Opts, bpm: f64, bars: Frame, setup: impl FnOnce(&mut Rig), level: f32, from: impl Fn(Frame) -> Frame, after: Frame, mid: impl FnOnce(&mut Rig)) -> (Vec<usize>, usize) {
    let mut rig = Rig::with(o);
    rig.set(Command::SetBpm(bpm));
    rig.set_level(0.0);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(bars as f64));
    rig.press(Command::RecDub(0));
    rig.advance_to(rig.end_frame() + 1);
    rig.set(Command::SetFixedLength(false));
    rig.idle();
    assert_eq!(rig.state(0), LaneState::Playing);
    setup(&mut rig);
    rig.advance(128); // an install applies at the next block start
    let align = rig.engine.live_latency();
    let d = rig.next_boundary() + 1000;
    rig.set_input(move |f| if (d..d + 128).contains(&f) { 0.0 } else { level });
    rig.advance_to(d);
    damaged(&mut rig, 128);
    mid(&mut rig);
    let s = from(d) + after;
    let press = s - align;
    assert!(press > rig.frame, "the window opens after the setup");
    rig.send_at(press, Command::RecDub(0));
    rig.send_at(press + 9600, Command::RecDub(0));
    rig.advance_to(press + 9600 + align + 1);
    rig.idle();
    let first = ((press - rig.anchor()).rem_euclid(rig.master()) / PF) as usize;
    (rig.marks(0), first)
}

#[test]
fn a_tail_marks_a_window_opening_inside_it() {
    let t = Tail::new(48_000).tail;
    // The last LOST frame leaves the unit at tap frame d + 383; its tail runs the T frames after it.
    let last = |d: Frame| d + 127 + Tail::LATENCY;
    for (after, marked) in [(2400, true), (t - 1, true), (t + 1, false)] {
        let (marks, first) = window_after(Opts::default(), 200.0, 4, |r| r.install(0, Box::new(Tail::new(48_000))), 0.0, last, after, |_| ());
        assert_eq!(marks.first() == Some(&first), marked, "Tail, a window {after} frames after the dropout: {marks:?}");
        assert_eq!(marks.is_empty(), !marked);
    }
    // A pure delay has no memory: its marks end exactly where its LOST does.
    let mut rig = Rig::new();
    rig.install(0, Box::new(Delay::new(1024)));
    rig.advance(4096);
    let d = rig.frame + 500;
    rig.advance_to(d);
    rig.keep_output();
    damaged(&mut rig, 128);
    rig.advance(4096);
    let lost: Vec<usize> = rig.record_damage.iter().enumerate().filter(|(_, &x)| x != 0).map(|(k, _)| k).collect();
    assert_eq!(lost, (1024..1152).collect::<Vec<_>>(), "Delay(1024): LOST on its latency's frames, nothing after");
}

#[test]
fn a_reported_tail_outlasts_the_loop_capacity_and_an_infinite_one_holds() {
    // A one-second engine (one 240 BPM bar of capacity): a 2 s tail still marks a window 1.5 s on, an
    // infinite one a window 4 s on. The countdown is never clamped to the capacity.
    let o = || Opts { loop_seconds: 1.0, ..Opts::default() };
    let last = |d: Frame| d + 127;
    for (tail, after, marked) in [(96_000, 72_000, true), (96_000, 100_000, false), (Frame::MAX, 192_000, true)] {
        let (marks, first) = window_after(o(), 240.0, 1, move |r| r.install(0, Box::new(Memory(tail))), 0.0, last, after, |_| ());
        assert_eq!(marks.first() == Some(&first), marked, "tail {tail}, a window {after} frames on: {marks:?}");
    }
}

fn echo_on(rig: &mut Rig, feedback: f64) {
    rig.set(Command::SetInputSendParam(InputSendParam::EchoLevel, 0.8));
    rig.set(Command::SetInputSendParam(InputSendParam::EchoFeedback, feedback));
    rig.set(Command::SetInputSend(InputSend::Echo, true));
    rig.advance(4800); // past the gate's ramp
}

fn reverb_on(rig: &mut Rig) {
    rig.set(Command::SetInputSendParam(InputSendParam::ReverbLevel, 0.8));
    rig.set(Command::SetInputSend(InputSend::Reverb, true));
    rig.advance(4800);
}

#[test]
fn echo_and_reverb_mark_a_window_their_memory_reaches() {
    let s = |x: f64| (x * 48_000.0) as Frame;
    let last = |d: Frame| d + 127;
    // ECHO, 8n at 120 BPM (0.25 s) and feedback 0.4: 8 repeats down to −60 dB, about 2.0 s.
    for (after, marked) in [(s(1.9), true), (s(2.1), false)] {
        let (marks, first) = window_after(Opts::default(), 120.0, 4, |r| echo_on(r, 0.4), 0.25, last, after, |_| ());
        assert_eq!(marks.first() == Some(&first), marked, "ECHO, a window {after} frames on: {marks:?}");
    }
    // A feedback turned up to 0.95 mid-tail extends it: 135 repeats.
    let (marks, first) = window_after(Opts::default(), 120.0, 4, |r| echo_on(r, 0.4), 0.25, last, s(2.1), |r| r.set(Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.95)));
    assert_eq!(marks.first(), Some(&first), "ECHO turned up mid-tail");
    // REVERB: its impulse response, 2.62 s.
    for (after, marked) in [(s(2.5), true), (s(2.7), false)] {
        let (marks, first) = window_after(Opts::default(), 120.0, 4, reverb_on, 0.25, last, after, |_| ());
        assert_eq!(marks.first() == Some(&first), marked, "REVERB, a window {after} frames on: {marks:?}");
    }
}

/// The record tap and its damage over `frames` from a damaged block of 128 input frames at `d` (silence
/// where `damage`, a 220 Hz tone elsewhere), the echo on at feedback `feedback` and division `time`
/// (settled), each of `changes` applied that many frames after `d`.
fn echo_run(damage: bool, feedback: f64, time: f64, changes: &[(Frame, Command)], frames: Frame) -> (Vec<f32>, Vec<u8>) {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(120.0));
    rig.set(Command::SetInputSendParam(InputSendParam::EchoTime, time));
    echo_on(&mut rig, feedback);
    let d = rig.frame + 2000;
    rig.set_input(move |f| if damage && (d..d + 128).contains(&f) { 0.0 } else { 0.3 * (TAU * 220.0 * f as f64 / 48_000.0).sin() as f32 });
    for &(at, change) in changes {
        rig.send_at(d + at, change);
    }
    rig.advance_to(d);
    rig.keep_output();
    if damage {
        damaged(&mut rig, 128);
    }
    rig.advance(frames - if damage { 128 } else { 0 });
    (rig.record.clone(), rig.record_damage.clone())
}

#[test]
fn every_echo_repeat_above_minus_60_db_is_marked_when_the_controls_turn_down_mid_tail() {
    // Clean and damaged runs of the same input: wherever the record taps differ by more than −60 dB of
    // the first repeat's difference, the damaged run's lane is marked. The controls turned down after
    // the dropout (feedback 0.95 → 0; the delay 8n → 16n) leave repeats already in the line playing out:
    // the bound the dropout started under holds.
    // The critique's case: twelve repeats at 0.95, then the control turned down at 3.05 s, while the
    // repeat written just before it is still in the line.
    let change_at = 146_400;
    for (name, change) in [("feedback down", Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.0)), ("delay down", Command::SetInputSendParam(InputSendParam::EchoTime, 3.0))] {
        let frames = 5 * 48_000;
        let (clean, _) = echo_run(false, 0.95, 1.0, &[(change_at, change)], frames);
        let (hit, marks) = echo_run(true, 0.95, 1.0, &[(change_at, change)], frames);
        let diff: Vec<f32> = clean.iter().zip(&hit).map(|(a, b)| (a - b).abs()).collect();
        let first = diff[12_000..12_000 + 12_128 + 128].iter().fold(0.0f32, |m, &x| m.max(x));
        assert!(first > 0.01, "{name}: the first repeat differs ({first})");
        let missed: Vec<usize> = (128..diff.len()).filter(|&k| diff[k] > first * 0.001 && marks[k] == 0).collect();
        assert!(missed.is_empty(), "{name}: {} affected frames unmarked, from {:?} (last {:?})", missed.len(), missed.first(), missed.last());
        let after = change_at as usize + 2400;
        assert!(diff[after..].iter().any(|&x| x > first * 0.001), "{name}: a repeat already in the line differs after the change");
    }
}

#[test]
fn an_echo_delay_turned_up_after_the_countdown_ends_marks_the_dropout_it_plays_again() {
    // The critique's case: no feedback, 8n at 120 BPM (0.25 s): the countdown ends a repeat after the
    // dropout. At 0.375 s the delay goes to 4n (0.5 s): the read head moves back over what the line
    // still holds and plays the dropout again, during the ramp and at 0.5 s. Every frame that differs
    // from the clean run's is marked.
    let change = Command::SetInputSendParam(InputSendParam::EchoTime, 0.0);
    let (clean, _) = echo_run(false, 0.0, 1.0, &[(18_000, change)], 48_000);
    let (hit, marks) = echo_run(true, 0.0, 1.0, &[(18_000, change)], 48_000);
    let diff: Vec<f32> = clean.iter().zip(&hit).map(|(a, b)| (a - b).abs()).collect();
    let first = diff[12_000..12_000 + 256].iter().fold(0.0f32, |m, &x| m.max(x));
    assert!(first > 0.01, "the first repeat differs ({first})");
    assert!(diff[18_000..].iter().any(|&x| x > first * 0.001), "the dropout plays again after the change");
    let missed: Vec<usize> = (128..diff.len()).filter(|&k| diff[k] > first * 0.001 && marks[k] == 0).collect();
    assert!(missed.is_empty(), "{} affected frames unmarked, from {:?} (last {:?})", missed.len(), missed.first(), missed.last());
}

#[test]
fn an_echo_shortened_then_fed_back_then_lengthened_marks_the_repeat_it_reads_again() {
    // The critique's sequence at 120 BPM: a settled 4n delay (0.5 s) at no feedback, a block lost at
    // t = 0; 8n (0.25 s) at 0.10 s, feedback 0.4 at 0.15 s, 4n again at 2.05 s. The eighth-note repeats
    // fall under −60 dB of the first near 2.0 s, but the line still holds the one written at 1.75 s
    // (0.4⁷, −56 dB), and the quarter-note read head plays it again at 2.25 s. Every frame that differs
    // from the clean run's by more than −60 dB of the first repeat is marked, and so is a window opened
    // after 2.0 s that hears it; one opened at 4.5 s is not.
    let s = |x: f64| (x * 48_000.0) as Frame;
    let time = |v: f64| Command::SetInputSendParam(InputSendParam::EchoTime, v);
    let changes = [(s(0.10), time(1.0)), (s(0.15), Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.4)), (s(2.05), time(0.0))];
    let (clean, _) = echo_run(false, 0.0, 0.0, &changes, s(3.0));
    let (hit, marks) = echo_run(true, 0.0, 0.0, &changes, s(3.0));
    let diff: Vec<f32> = clean.iter().zip(&hit).map(|(a, b)| (a - b).abs()).collect();
    let first = diff.iter().fold(0.0f32, |m, &x| m.max(x));
    assert!(first > 0.01, "the first repeat differs ({first})");
    let replay = s(2.25) as usize;
    assert!(diff[replay..replay + 256].iter().any(|&x| x > first * 0.001), "the repeat written at 1.75 s plays again at 2.25 s");
    let missed: Vec<usize> = (128..diff.len()).filter(|&k| diff[k] > first * 0.001 && marks[k] == 0).collect();
    assert!(missed.is_empty(), "{} affected frames unmarked, from {:?} (last {:?})", missed.len(), missed.first(), missed.last());
    for (after, marked) in [(s(2.15), true), (s(4.5), false)] {
        let setup = |r: &mut Rig| {
            r.set(time(0.0));
            echo_on(r, 0.0);
        };
        let mid = |r: &mut Rig| {
            let d = r.frame - 128;
            for &(at, change) in &changes {
                r.send_at(d + at, change);
            }
        };
        let (marks, first) = window_after(Opts::default(), 120.0, 4, setup, 0.25, |d| d, after, mid);
        assert_eq!(marks.first() == Some(&first), marked, "a window {after} frames after the dropout: {marks:?}");
    }
}

/// xorshift64*: the source of the random control sequences, the same on every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    }
}

/// A control change in an echo sequence.
#[derive(Clone, Copy, Debug)]
enum Turn {
    Time(f64),
    Feedback(f64),
    Tempo(u32),
    On(bool),
}

const ECHO_SR: u32 = 48_000;
const ECHO_FRAMES: usize = 6 * ECHO_SR as usize;

/// ECHO alone (level 1, 120 BPM, on from frame 0 at division and feedback `start`), rendered clean
/// and with input block `lost` silent and `LOST`, side by side, each of `turns` applied on its frame and
/// the blocks cut at `blocks`' sizes in turn. The input is a 220 Hz tone under a hash of the frame, or
/// with `burst` that only inside `lost`, so the damaged run's line stays silent and its echo idles
/// whenever the send is off. Returns the frames where the outputs differ by more than −60 dB of the
/// lost block's peak (the first repeat at no feedback) and those of them the damaged run left unmarked.
fn echo_sides(burst: bool, lost: std::ops::Range<usize>, start: (f64, f64), turns: &[(usize, Turn)], blocks: &[usize]) -> (Vec<usize>, Vec<usize>) {
    let ir = [0.0f32; 256]; // the reverb stays off
    let tone = |f: usize| 0.3 * (TAU * 220.0 * f as f64 / ECHO_SR as f64).sin() as f32 + 0.1 * ((f * 7919 % 1000) as f32 / 1000.0 - 0.5);
    let clean: Vec<f32> = (0..ECHO_FRAMES).map(|f| if burst && !lost.contains(&f) { 0.0 } else { tone(f) }).collect();
    let render = |hit: bool| -> (Vec<f32>, Vec<u8>) {
        let mut fx = InputFx::new(ECHO_SR, [&ir[..], &ir[..]]);
        fx.follow_tempo(120, Meter::FOUR_FOUR, 0);
        fx.set_param(InputSendParam::EchoLevel, 1.0, 0);
        fx.set_param(InputSendParam::EchoTime, start.0, 0);
        fx.set_param(InputSendParam::EchoFeedback, start.1, 0);
        fx.set_on(InputSend::Echo, true, 0);
        let mut x = clean.clone();
        let mut marks = vec![0u8; ECHO_FRAMES];
        if hit {
            x[lost.clone()].fill(0.0);
            marks[lost.clone()].fill(damage::LOST);
        }
        let mut out = vec![0.0f32; ECHO_FRAMES];
        let (mut f, mut next, mut b) = (0, 0, 0);
        while f < ECHO_FRAMES {
            while next < turns.len() && turns[next].0 == f {
                let at = f as Frame;
                match turns[next].1 {
                    Turn::Time(v) => fx.set_param(InputSendParam::EchoTime, v, at),
                    Turn::Feedback(v) => fx.set_param(InputSendParam::EchoFeedback, v, at),
                    Turn::Tempo(bpm) => fx.follow_tempo(bpm, Meter::FOUR_FOUR, at),
                    Turn::On(on) => fx.set_on(InputSend::Echo, on, at),
                }
                next += 1;
            }
            let end = turns.get(next).map_or(ECHO_FRAMES, |t| t.0).min(f + blocks[b % blocks.len()]);
            fx.render(f as Frame, &x[f..end], &mut out[f..end], &mut marks[f..end]);
            (f, b) = (end, b + 1);
        }
        (out, marks)
    };
    let (dry, _) = render(false);
    let (wet, marks) = render(true);
    let bar = 0.001 * clean[lost].iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    let above: Vec<usize> = (0..ECHO_FRAMES).filter(|&k| (dry[k] - wet[k]).abs() > bar).collect();
    let missed = above.iter().copied().filter(|&k| marks[k] == 0).collect();
    (above, missed)
}

#[test]
fn every_echo_replay_above_minus_60_db_is_marked_under_random_controls() {
    // A random control sequence per seed: the division and the tempo up and down, the feedback (0 to
    // 0.95), the send on and off, every 50 ms to half a second, from a random division and feedback;
    // every other seed a burst (`echo_sides`). Wherever the clean and the damaged run differ by more
    // than −60 dB, the damaged run is marked: no sequence replays a dropout unmarked.
    const SEEDS: u64 = 1000;
    const SR: usize = ECHO_SR as usize;
    let (mut late, mut changed, mut idle) = (0, 0, 0);
    for seed in 0..SEEDS {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (seed + 1).wrapping_mul(0xD1B5_4A32_D192_ED03));
        let lost = 4800 + rng.below(2 * SR as u64) as usize;
        let lost = lost..lost + 64 + rng.below(449) as usize;
        let start = (rng.below(4) as f64, rng.below(96) as f64 / 100.0);
        let mut turns = Vec::new();
        let mut at = 0;
        loop {
            at += 2400 + rng.below(SR as u64 / 2) as usize;
            if at >= ECHO_FRAMES - SR {
                break;
            }
            turns.push((at, match rng.below(8) {
                0..=2 => Turn::Time(rng.below(4) as f64),
                3 => Turn::Tempo(40 + rng.below(261) as u32),
                4 | 5 => Turn::Feedback(if rng.below(3) == 0 { 0.0 } else { rng.below(96) as f64 / 100.0 }),
                _ => Turn::On(rng.below(4) != 0),
            }));
        }
        let blocks: Vec<usize> = (0..64).map(|_| 1 + rng.below(1024) as usize).collect();
        let burst = seed % 2 == 1;
        let (above, missed) = echo_sides(burst, lost.clone(), start, &turns, &blocks);
        assert!(missed.is_empty(), "seed {seed}: {} frames above −60 dB unmarked, from {:?} (last {:?}); lost {lost:?}, burst {burst}, starting at {start:?}, turns {turns:?}", missed.len(), missed.first(), missed.last());
        let after = |k: usize| turns.iter().any(|&(at, t)| at > lost.end && at < k && matches!(t, Turn::Time(_) | Turn::Tempo(_)));
        // The send off at `k` (by the last on or off before it, past its ramp): a burst's damaged run idles.
        let off = |k: usize| turns.iter().rev().find(|&&(at, t)| at + 1200 < k && matches!(t, Turn::On(_))).is_some_and(|&(_, t)| matches!(t, Turn::On(false)));
        late += above.last().is_some_and(|&k| k > lost.end + 2 * SR) as u32;
        changed += above.iter().any(|&k| after(k)) as u32;
        idle += (burst && above.iter().any(|&k| off(k))) as u32;
    }
    // Not vacuous: many sequences replay the dropout past 2 s, after the delay has changed, and while a
    // burst's damaged run idles.
    assert!(late > SEEDS as u32 / 5 && changed > SEEDS as u32 / 2 && idle > SEEDS as u32 / 20, "replays past 2 s in {late} seeds, after a delay change in {changed}, idle in {idle}");
}

#[test]
fn an_echo_idling_on_a_line_the_dropout_emptied_follows_the_controls_the_clean_line_repeats_under() {
    // A burst lost whole (`echo_sides`): the damaged run's line never sounds, so once the send is off
    // its echo idles and computes no control values, while the clean run repeats the burst. A feedback
    // or a delay turned up meanwhile still sets how long that runs: every repeat above −60 dB is marked.
    let lost = 4800..5056;
    let cases = [
        ("feedback up while idle", (1.0, 0.0), [(6000, Turn::On(false)), (9600, Turn::Feedback(0.9))]),
        ("delay up while idle", (3.0, 0.5), [(6000, Turn::On(false)), (9600, Turn::Time(0.0))]),
    ];
    for (name, start, turns) in cases {
        let (above, missed) = echo_sides(true, lost.clone(), start, &turns, &[480]);
        assert!(above.last().is_some_and(|&k| k > 48_000), "{name}: the clean run repeats the burst past 1 s");
        assert!(missed.is_empty(), "{name}: {} frames above −60 dB unmarked, from {:?} (last {:?})", missed.len(), missed.first(), missed.last());
    }
}

#[test]
fn a_jump_displaces_what_the_sends_still_hold_and_marks_it_whatever_their_gates() {
    // A clean tone, the send on, then off (its gate shut) before its memory has played out; the device
    // frame jumps a second. What the send still held comes out after the jump, displaced: marked, though
    // no damage entered it.
    for (name, on) in [("ECHO", echo_on as fn(&mut Rig, f64)), ("REVERB", |r: &mut Rig, _| reverb_on(r))] {
        let mut rig = Rig::new();
        rig.set(Command::SetBpm(120.0));
        rig.set_level(0.0);
        rig.record_first_take(0, 4, 2400);
        rig.idle();
        on(&mut rig, 0.4);
        rig.set_input(|f| 0.3 * (TAU * 220.0 * f as f64 / 48_000.0).sin() as f32);
        rig.advance(2400);
        rig.set_level(0.0);
        let send = if name == "ECHO" { InputSend::Echo } else { InputSend::Reverb };
        rig.set(Command::SetInputSend(send, false));
        rig.advance(4800);
        rig.skip(48_000);
        rig.keep_output();
        rig.advance(9600); // the echo's first repeat was 4,800 frames away when the device jumped
        assert!(rig.record[1..].iter().any(|&x| x != 0.0), "{name}: its memory sounds after the jump");
        assert!(rig.record_damage[1..].iter().all(|&d| d & damage::TAIL != 0), "{name}: displaced, marked TAIL from the jump");
        // A window opening after it is marked from its start.
        let press = rig.frame + 2400;
        rig.send_at(press, Command::RecDub(0));
        rig.send_at(press + 4800, Command::RecDub(0));
        rig.advance_to(press + 4801);
        rig.idle();
        let first = ((press - rig.anchor()).rem_euclid(rig.master()) / PF) as usize;
        assert_eq!(rig.marks(0).first(), Some(&first), "{name}: the window after the jump is marked");
    }
}

// ── 12. A unit that ignores its input: the false mark, pinned ───────────────────────────────────────

#[test]
fn a_unit_that_ignores_its_input_is_marked_and_unchanged() {
    let run = |damage: bool| {
        let mut rig = Rig::new();
        rig.install(0, Box::new(Ignore { from: 0, level: 0.25 }));
        rig.set(Command::SetBpm(120.0));
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(1.0));
        rig.set_input(code);
        rig.press(Command::RecDub(0));
        let s = rig.start_frame();
        rig.advance_to(s + 30_000);
        if damage {
            damaged(&mut rig, 128);
        }
        rig.advance_to(rig.end_frame() + 1);
        rig.idle();
        (rig.pcm(0), rig.marks(0))
    };
    let ((clean, none), (hit, marks)) = (run(false), run(true));
    assert!(none.is_empty());
    assert_eq!(hit, clean, "the take is the clean run's: the unit never heard the input");
    assert_eq!(marks, bins(30_000, 128, frames_per_bar(120.0, 48_000, Meter::FOUR_FOUR)), "marked all the same: the routing model");
}

// ── 13. The ring follows every latency up to the capacity ───────────────────────────────────────────

#[test]
fn the_ring_follows_every_latency_to_the_capacity() {
    // A one-second engine: the unit ring holds 48,001 entries.
    let run = |latency: Frame, damage: bool| {
        let mut rig = Rig::with(Opts { loop_seconds: 1.0, ..Opts::default() });
        rig.install(0, Box::new(Delay::new(latency)));
        rig.set_input(code);
        rig.advance(70_000); // past the unit's first L frames, and the ring around once
        let d = rig.frame;
        rig.keep_output();
        rig.set_input(move |f| if damage && (d..d + 128).contains(&f) { 0.0 } else { code(f) });
        if damage {
            damaged(&mut rig, 128);
        }
        rig.advance(latency + 4096 - if damage { 128 } else { 0 });
        (rig.record.clone(), rig.record_damage.clone())
    };
    for latency in [47_999, 48_000, 48_001] {
        let ((clean, _), (hit, marks)) = (run(latency, false), run(latency, true));
        let differ: Vec<usize> = clean.iter().zip(&hit).enumerate().filter(|(_, (a, b))| a != b).map(|(k, _)| k).collect();
        let lost: Vec<usize> = marks.iter().enumerate().filter(|(_, &d)| d & damage::LOST != 0).map(|(k, _)| k).collect();
        assert_eq!(differ, (latency as usize..latency as usize + 128).collect::<Vec<_>>(), "Delay({latency}): where the taps differ");
        if latency <= 48_000 {
            assert_eq!(lost, differ, "Delay({latency}): LOST exactly there");
        } else {
            assert!(differ.iter().all(|k| lost.contains(k)), "Delay({latency}) past the ring: its hull covers the difference");
        }
    }
}

#[test]
fn a_removed_units_unplayed_output_and_a_new_units_first_latency_are_unmarked() {
    let mut rig = Rig::new();
    rig.install(0, Box::new(Delay::new(1024)));
    rig.set_input(code);
    rig.advance(8192);
    rig.keep_output();
    damaged(&mut rig, 128);
    rig.advance(128);
    rig.remove(0); // its fade is 480 frames: the block's LOST would come out 1,024 after it
    rig.advance(128);
    let _ = rig.returned(0);
    rig.advance(1024);
    let gone = rig.returned(0);
    assert!(gone.is_some(), "the unit came back");
    rig.install(0, Box::new(Delay::new(1024)));
    rig.advance(4096);
    assert!(rig.record_damage.iter().all(|&d| d & damage::LOST == 0), "nothing of the removed unit is marked, nor the new unit's first frames");
}

// ── 14. AUTO ─────────────────────────────────────────────────────────────────────────────────────────

/// AUTO armed on an empty looper, `setup` applied first; the input `input`; `damage` damaged blocks
/// of 128 frames at the input frames listed. Renders `frames`; returns the take's onset (its window
/// start), the frame it triggered on, its marks and the rig.
fn auto(setup: impl FnOnce(&mut Rig), input: impl Fn(Frame) -> f32 + 'static, damage: &[Frame], frames: Frame) -> (Option<Frame>, Option<Frame>, Rig) {
    let mut rig = Rig::new();
    setup(&mut rig);
    rig.set(Command::SetAutoRecord(true));
    rig.set_input(input);
    rig.press(Command::RecDub(0));
    let end = rig.frame + frames;
    let mut triggered = None;
    while rig.frame < end {
        if damage.contains(&rig.frame) {
            damaged(&mut rig, 128);
        } else {
            rig.advance(1);
        }
        if triggered.is_none() && !rig.lane(0).auto_armed {
            triggered = Some(rig.frame);
        }
    }
    let onset = rig.window().and_then(|w| w.1);
    (onset, triggered, rig)
}

#[test]
fn auto_never_hears_a_lost_run_and_its_preroll_starts_after_one() {
    let t0 = 100_000;
    // A soft signal (above the onset floor, under the trigger), an attack at `t0 + 2000`; Delay(1024)
    // carries a damaged block at `t0` to the tap 1,024 later.
    let soft = move |f: Frame| if f >= t0 + 2000 { 0.5 } else { 0.01 };
    let (onset, _, _) = auto(|r| r.install(0, Box::new(Delay::new(1024))), soft, &[t0], 120_000);
    let onset = onset.expect("AUTO triggered");
    assert!(onset >= t0 + 1024 + 128, "the pre-roll starts after the LOST frames reach the tap: {onset}");
    // Loud damaged frames never trigger: the device's evidence is not fed.
    let (onset, _, rig) = auto(|_| (), move |f| if (t0..t0 + 128).contains(&f) { 0.5 } else { 0.0 }, &[t0], 120_000);
    assert!(onset.is_none() && rig.lane(0).auto_armed, "still listening");
}

#[test]
fn a_clean_attack_inside_a_tail_triggers_as_clean_with_its_preroll_marked() {
    let t0 = 100_000;
    let attack = move |f: Frame| if (t0 + 1000..t0 + 1100).contains(&f) { 0.5 } else { 0.0 };
    let run = |damage: &[Frame]| {
        let (onset, at, mut rig) = auto(|r| r.install(0, Box::new(Tail::new(48_000))), attack, damage, 110_000);
        rig.press(Command::RecDub(0));
        rig.advance(4800);
        rig.idle();
        (onset.expect("AUTO triggered"), at, rig.pcm(0), rig.marks(0))
    };
    let (clean, hit) = (run(&[]), run(&[t0]));
    assert_eq!(hit.1, clean.1, "the trigger is the clean run's");
    // The detector's 4 ms analysis blocks count from its history's start, which the LOST frames reset:
    // the onset can move by less than one block. Frame for frame the take is the clean run's.
    let shift = hit.0 - clean.0;
    assert!((0..192).contains(&shift), "the onset moved {shift} frames");
    let raw = (clean.1.unwrap() + 4800 - hit.0) as usize;
    assert_eq!(hit.2[..raw], clean.2[shift as usize..shift as usize + raw], "the take is the clean run's, bit for bit, frame for frame");
    assert!(clean.3.is_empty());
    // TAIL from the last LOST frame at the tap (t0 + 127 + 256) for the unit's tail: the pre-roll and
    // the take's frames it reaches are marked.
    let until = t0 + 127 + Tail::LATENCY + Tail::new(48_000).tail + 1;
    assert_eq!(hit.3, bins(0, until - hit.0, Frame::MAX), "marked from the pre-roll to the tail's end");
}

#[test]
fn a_false_lost_span_shortens_autos_preroll_and_still_triggers() {
    let f = 100_000;
    let unit = move |r: &mut Rig| r.install(0, Box::new(Ignore { from: f, level: 0.3 }));
    let (clean, clean_at, _) = auto(unit, |_| 0.0, &[], 110_000);
    let (hit, hit_at, _) = auto(unit, |_| 0.0, &[f - 128], 110_000);
    let (clean_at, hit_at) = (clean_at.expect("clean: AUTO triggered"), hit_at.expect("the sustained tone triggers after the false LOST span"));
    // The reset empties the detector's 4 ms energy window: it triggers once that has refilled.
    assert!((clean_at..=clean_at + 192).contains(&hit_at), "triggered at {hit_at}, clean at {clean_at}");
    let (clean, hit) = (clean.unwrap(), hit.unwrap());
    assert!(hit > clean && hit >= f, "the pre-roll stops at the false LOST span: {hit} vs {clean}");
}
