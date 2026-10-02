//! D23's overdub punch ramps (STATUS § Decisions): a layer's first and last 5 ms are linear ramps stored
//! in the loop. From its window's first input frame `s` the layer fades in, `a = (f - s) / N` (its first
//! frame writes nothing new); at a clean end `e` its last writes fade back toward what each overwrote,
//! `b = (e - f) / N` (the last write keeps `1 / N` of its change, and nothing at or past `e` is written).
//! `N` is 5 ms of frames. Every check here holds the stored loop bit for bit to `common::dub`, which
//! computes the layer in one chronological pass without the engine; what no ramp touches is today's
//! `x + feedback * old`.
//!
//! Held here: both edges at feedback 0, 0.5 and 1, aligned and not; gestures shorter than two ramps; a
//! multi-pass dub whose punch-out returns to its last pass, not to the undo target; a stopped device's
//! punch-out finishing the ramp on the frames it retained; a damaged layer restored bit for bit with its
//! previous undo target; the peaks and a session snapshot after the punch-out; an alignment past a whole
//! loop; and block-size identity.
//! What this cannot hear: whether 5 ms is enough on a real guitar layer (the owner's ear,
//! `seam_continuity.rs` holds the tone's seam).

mod common;

use common::dub::{pos_fn, ramp};
use common::{code, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::overview::PEAK_FRAMES;
use lf_engine::{Command, LaneState, SessionError, SessionJob, Snapshot};

/// Rounding-sensitive input: a full mantissa over the take's small codes, so another operation order
/// lands on other bits.
fn input(f: Frame) -> f32 {
    ((f as f64 * 0.7373).sin() * 0.3 + 1.0 / 3.0) as f32
}

/// Lane 0 PLAYING a one-bar loop of the frame code at `bpm`, at `sr`, the record alignment `align`, the
/// input silent, no job running.
fn looping(sr: u32, bpm: f64, align: Frame) -> Rig {
    let mut rig = Rig::with(Opts { sr, start: sr as Frame, align, ..Default::default() });
    rig.set(Command::SetBpm(bpm));
    rig.set_input(code);
    rig.record_first_take(0, 1, 240);
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.state(0), LaneState::Playing);
    rig
}

/// Overdub lane 0 over input frames `[start, end)`: DUB and its stop sent `align` frames before each
/// edge, on their exact frames; then render through the window's end.
fn dub_over(rig: &mut Rig, start: Frame, end: Frame) {
    let align = rig.align;
    rig.send_at(start - align, Command::RecDub(0));
    rig.send_at(end - align, Command::RecDub(0));
    rig.advance_to(end + 1);
    assert!(rig.window().is_none() && rig.state(0) == LaneState::Playing, "the layer committed");
}

/// The positions where `got` and `want` differ in their bits.
fn off(got: &[f32], want: &[f32]) -> Vec<usize> {
    (0..got.len()).filter(|&p| got[p].to_bits() != want[p].to_bits()).collect()
}

#[test]
fn punch_edges_match_an_independent_frame_reference() {
    for (sr, align) in [(48000, 0), (44100, 333)] {
        for fb in [0.0f32, 0.5, 1.0] {
            let mut rig = looping(sr, 200.0, align);
            rig.set(Command::SetDubFeedback(0, fb));
            let (master, n) = (rig.master(), ramp(sr));
            let pre = rig.pcm(0);
            // A window across the loop point, under a pass long: both ramps land on positions read once.
            let start = rig.next_boundary() + master / 2 + align + 17;
            let end = start + master * 2 / 3 + 77;
            rig.set_input(input); // loud past the end too: nothing there may land
            dub_over(&mut rig, start, end);
            rig.set_level(0.0);
            rig.idle();
            let pos = pos_fn(rig.anchor(), master, align);
            let mut want = pre.clone();
            common::dub::dub(&mut want, (start, end), n, fb, &pos, input);
            let got = rig.pcm(0);
            assert_eq!(off(&got, &want), Vec::<usize>::new(), "sr {sr} align {align} feedback {fb}");
            // Read directly: the first frame keeps the loop, the interior is today's sum, past the end
            // nothing moved, and the edges are neither.
            assert_eq!(got[pos(start)].to_bits(), pre[pos(start)].to_bits(), "the punch-in's first frame writes nothing new");
            for f in [start + n, start + master / 3, end - n] {
                assert_eq!(got[pos(f)].to_bits(), (input(f) + fb * pre[pos(f)]).to_bits(), "frame {f}: today's sum between the ramps");
            }
            for f in end..end + 2 * n {
                assert_eq!(got[pos(f)].to_bits(), pre[pos(f)].to_bits(), "frame {f}: nothing at or past the end");
            }
            for f in [start + n / 2, end - n / 2] {
                let hard = input(f) + fb * pre[pos(f)];
                assert!(got[pos(f)] != hard && got[pos(f)] != pre[pos(f)], "frame {f}: half-way up a ramp");
            }
        }
    }
}

#[test]
fn short_dubs_compose_both_envelopes_without_extending_capture() {
    const ALIGN: Frame = 100;
    let n = ramp(48000);
    for d in [0, 1, n - 1, n, n + 1, 2 * n - 1, 2 * n] {
        let mut rig = looping(48000, 200.0, ALIGN);
        rig.set(Command::SetDubFeedback(0, 0.5));
        let master = rig.master();
        let pre = rig.pcm(0);
        // From half a ramp before the loop point: the longer gestures wrap.
        let start = rig.next_boundary() + master + ALIGN - n / 2;
        rig.set_input(input);
        dub_over(&mut rig, start, start + d);
        rig.set_level(0.0);
        rig.idle();
        let pos = pos_fn(rig.anchor(), master, ALIGN);
        let mut want = pre.clone();
        common::dub::dub(&mut want, (start, start + d), n, 0.5, &pos, input);
        let got = rig.pcm(0);
        assert_eq!(off(&got, &want), Vec::<usize>::new(), "{d} frames");
        if d <= 1 {
            assert_eq!(got, pre, "{d} frames: the punch-in's first frame writes nothing new");
        }
        for f in start + d..start + d + ALIGN + n {
            assert_eq!(got[pos(f)].to_bits(), pre[pos(f)].to_bits(), "{d} frames: frame {f}, at or past the end");
        }
        // Under two ramps no frame reaches today's sum.
        for f in start + 1..start + d {
            let hard = input(f) + 0.5 * pre[pos(f)];
            assert!(d >= 2 * n || got[pos(f)] != hard, "{d} frames: frame {f} at full level");
        }
    }
}

#[test]
fn a_multi_pass_dub_fades_out_to_its_last_pass_not_to_the_undo_target() {
    let n = ramp(48000);
    let mut rig = looping(48000, 200.0, 0);
    rig.set(Command::SetDubFeedback(0, 0.5));
    let master = rig.master();
    let pre = rig.pcm(0);
    // Two passes and more, ending a third of a ramp past the loop point: the punch-out ramp wraps.
    let start = rig.next_boundary() + master / 3;
    let end = rig.next_boundary() + 3 * master + n / 3;
    rig.set_input(input);
    dub_over(&mut rig, start, end);
    rig.set_level(0.0);
    rig.idle();
    let pos = pos_fn(rig.anchor(), master, 0);
    let mut want = pre.clone();
    common::dub::dub(&mut want, (start, end), n, 0.5, &pos, input);
    let got = rig.pcm(0);
    assert_eq!(off(&got, &want), Vec::<usize>::new());
    assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), pre, "the undo target is the loop at dub start");
    // The same ramp toward the undo target instead: every faded write lands elsewhere.
    let mut unfaded = pre.clone();
    for f in start..end {
        let p = pos(f);
        unfaded[p] = common::dub::write(unfaded[p], input(f), 0.5, f - start, Frame::MAX, n);
    }
    for f in end - n + 1..end {
        let b = (end - f) as f32 / n as f32;
        let toward_undo = pre[pos(f)] + b * (unfaded[pos(f)] - pre[pos(f)]);
        assert!(got[pos(f)] != toward_undo, "frame {f}: the punch-out fades toward the last pass, not the undo target");
    }
}

#[test]
fn a_stopped_devices_punch_out_finishes_the_ramp_on_the_frames_it_retained() {
    const ALIGN: Frame = 1920;
    let n = ramp(48000);
    // A window open past a ramp; one a ramp has not filled; one whose stop press set an end inside the
    // alignment the device then never renders.
    for (case, frames, stop_press) in [("open", 5 * n, false), ("short", n / 2, false), ("pressed", 5 * n, true)] {
        let mut rig = looping(48000, 200.0, ALIGN);
        let master = rig.master();
        let pre = rig.pcm(0);
        let start = rig.next_boundary() + master / 4 + ALIGN;
        rig.set_input(input);
        rig.send_at(start - ALIGN, Command::RecDub(0));
        if stop_press {
            // Its window would close half the alignment after the device stops.
            rig.send_at(start + frames - ALIGN / 2, Command::RecDub(0));
        }
        rig.advance_to(start + frames);
        let end = rig.frame; // the next frame the device would have rendered
        rig.punch_out();
        assert!(rig.window().is_none() && rig.state(0) == LaneState::Playing, "{case}");
        let pos = pos_fn(rig.anchor(), master, ALIGN);
        let mut want = pre.clone();
        common::dub::dub(&mut want, (start, end), n, 1.0, &pos, input);
        // At once, with no frame rendered after it.
        assert_eq!(off(&rig.pcm(0), &want), Vec::<usize>::new(), "{case}");
        rig.set_level(0.0);
        rig.idle();
        assert_eq!(off(&rig.pcm(0), &want), Vec::<usize>::new(), "{case}: and as rendering resumes");
        assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), pre, "{case}");
    }
}

#[test]
fn a_damaged_dub_restores_the_loop_and_the_previous_undo_target_bit_for_bit() {
    let n = ramp(48000);
    // An input gap inside the punch-in ramp, and one inside the punch-out ramp.
    for gap_at in [n / 2, 3 * n - n / 2] {
        let mut rig = looping(48000, 200.0, 0);
        rig.set(Command::SetDubFeedback(0, 0.5));
        let master = rig.master();
        let take = rig.pcm(0);
        rig.set_input(input);
        let first = rig.next_boundary() + master / 5;
        dub_over(&mut rig, first, first + 3 * n);
        rig.idle();
        let layered = rig.pcm(0);
        assert_ne!(layered, take);
        let start = rig.next_boundary() + master / 2;
        rig.send_at(start, Command::RecDub(0));
        rig.send_at(start + 3 * n, Command::RecDub(0));
        rig.advance_to(start + gap_at);
        rig.gap(); // a point xrun at this frame, inside the window
        rig.advance_to(start + 3 * n + 1);
        rig.set_level(0.0);
        rig.idle();
        assert_eq!(rig.rejected(), 1, "gap at {gap_at}");
        assert_eq!(rig.state(0), LaneState::Playing);
        assert_eq!(off(&rig.pcm(0), &layered), Vec::<usize>::new(), "gap at {gap_at}: the loop before the layer");
        assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), take, "gap at {gap_at}: and the undo target before it");
        rig.press(Command::Undo(0));
        rig.idle();
        assert_eq!(rig.pcm(0), take);
    }
}

#[test]
fn the_punch_out_redraws_the_peaks_and_a_snapshot_after_it_holds_the_faded_loop() {
    let n = ramp(48000);
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(200.0));
    rig.set_level(0.0);
    rig.record_first_take(0, 1, 240); // a silent loop
    rig.idle();
    let master = rig.master();
    let buf = rig.engine.looper().overview().lane(0).buf;
    // The layer ends 100 positions into bin 20: before the punch-out that bin's top is the layer's 0.5,
    // after it the faded tail's.
    let end = rig.next_boundary() + 20 * PEAK_FRAMES as Frame + 100;
    let start = end - 3000;
    rig.set_level(0.5);
    rig.send_at(start, Command::RecDub(0));
    rig.send_at(end, Command::RecDub(0));
    rig.advance_to(end);
    let overview = rig.engine.looper().overview().clone();
    overview.take_dirty(buf, |_| {});
    assert_eq!(overview.bin(buf, 20), (0.0, 0.5), "the layer before its punch-out");
    rig.advance(1);
    let mut dirty = Vec::new();
    overview.take_dirty(buf, |bin| dirty.push(bin));
    let first = (end - n + 1 - rig.anchor()).rem_euclid(master) as usize;
    assert_eq!(first / PEAK_FRAMES, 19);
    assert_eq!(dirty, [19, 20], "the punch-out marks the bins it rewrote, and only those");
    let pcm = rig.pcm(0);
    for (bin, frames) in pcm.chunks(PEAK_FRAMES).enumerate() {
        let want = frames.iter().fold((0.0f32, 0.0f32), |(lo, hi), &x| (lo.min(x), hi.max(x)));
        assert_eq!(overview.bin(buf, bin), want, "bin {bin}");
    }
    assert!(overview.bin(buf, 20).1 < 0.25, "the faded tail's top: {:?}", overview.bin(buf, 20));
}

#[test]
fn a_snapshot_spanning_a_punch_out_says_so_and_one_after_it_holds_the_faded_loop() {
    // The snapshot pins the loop while it plays; a layer begins and punches out while it copies. (While a
    // lane overdubs a snapshot pins its loop before the layer, and it pins the live loop again only once
    // the punch-out has run: the punch-out's own write count is the peaks test's dirty bins.)
    let mut rig = Rig::with(Opts { sr: 8000, start: 8000, ..Default::default() });
    let n = ramp(8000);
    rig.set(Command::SetBpm(60.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 4, 240); // 128000 frames: 125 frames of copying
    rig.set_level(0.0);
    rig.idle();
    let pre = rig.pcm(0);
    let mut dest = Vec::with_capacity(master as usize);
    dest.resize(master as usize, 0.0f32);
    assert!(rig.session().send(Box::new(SessionJob::Snapshot(Snapshot::new(dest)))).is_ok());
    rig.advance(1);
    rig.set_level(0.5);
    let start = rig.frame;
    rig.send_at(start, Command::RecDub(0));
    rig.send_at(start + 2 * n, Command::RecDub(0));
    let mut back = None;
    while back.is_none() {
        rig.advance(1);
        back = rig.session().returned();
    }
    assert!(rig.frame > start + 2 * n, "the copy outlasted the layer");
    let Some(SessionJob::Snapshot(s)) = back.map(|b| *b) else { panic!("no snapshot came back") };
    assert_eq!(s.result, Some(Err(SessionError::Changed)), "a layer and its punch-out wrote the loop it copied");
    rig.set_level(0.0);
    rig.idle();
    let mut want = pre.clone();
    common::dub::dub(&mut want, (start, start + 2 * n), n, 1.0, pos_fn(rig.anchor(), master, 0), |_| 0.5);
    assert_eq!(off(&rig.pcm(0), &want), Vec::<usize>::new(), "the layer, faded in and out");
    let mut dest = Vec::with_capacity(master as usize);
    dest.resize(master as usize, 0.0f32);
    assert!(rig.session().send(Box::new(SessionJob::Snapshot(Snapshot::new(dest)))).is_ok());
    let mut back = None;
    while back.is_none() {
        rig.advance(rig.block as Frame);
        back = rig.session().returned();
    }
    let Some(SessionJob::Snapshot(s)) = back.map(|b| *b) else { panic!("no snapshot came back") };
    assert_eq!(s.result, Some(Ok(())));
    assert_eq!(off(&s.pcm[..master as usize], &rig.pcm(0)), Vec::<usize>::new(), "the snapshot holds the faded loop");
}

#[test]
fn the_punch_edges_are_bit_identical_at_any_block_size() {
    const ALIGN: Frame = 333;
    let n = ramp(48000);
    let mut runs: Vec<(usize, Vec<f32>, Vec<f32>)> = Vec::new();
    for block in [1usize, 7, 64, 127, 128, 241, 480, 1024] {
        let mut rig = looping(48000, 200.0, ALIGN);
        rig.block = block;
        rig.set(Command::SetDubFeedback(0, 0.5));
        let master = rig.master();
        let pre = rig.pcm(0);
        let b = rig.next_boundary() + ALIGN;
        rig.set_input(input);
        rig.keep_output();
        // A layer across the loop point, a gesture of one ramp and a frame, and a punch-out by the device,
        // every edge off any block boundary but the device's.
        let windows = [(b + master - 1001, b + master + 2999), (b + 2 * master + 1500, b + 2 * master + 1501 + n)];
        for (start, end) in windows {
            dub_over(&mut rig, start, end);
        }
        let start = b + 3 * master - 77;
        rig.send_at(start - ALIGN, Command::RecDub(0));
        rig.advance_to(start + 2 * n + 5);
        let end = rig.frame;
        rig.punch_out();
        rig.set_level(0.0);
        rig.advance(master);
        let pos = pos_fn(rig.anchor(), master, ALIGN);
        let mut want = pre.clone();
        for w in windows.into_iter().chain([(start, end)]) {
            common::dub::dub(&mut want, w, n, 0.5, &pos, input);
        }
        let got = rig.pcm(0);
        assert_eq!(off(&got, &want), Vec::<usize>::new(), "block {block}");
        runs.push((block, got, rig.output.take().unwrap().1));
    }
    let (_, pcm, out) = &runs[0];
    for (block, p, o) in &runs[1..] {
        assert_eq!(off(p, pcm), Vec::<usize>::new(), "block {block}: the loop as at block 1");
        assert_eq!(off(o, out), Vec::<usize>::new(), "block {block}: the output as at block 1");
    }
}

/// A total latency past a whole loop is accepted (a slow device plus a plugin's latency): the ramps land
/// the same and a debug build faults nothing. At `2 * master - 100` the read head runs 100 positions
/// behind the writer, modulo the loop, so the newest writes are heard once unfaded; the stored loop is
/// the same.
#[test]
fn an_alignment_past_a_whole_loop_ramps_the_same_and_faults_nothing() {
    let n = ramp(48000);
    for align in [48_000, 2 * 38_400 - 100] {
        // `looping` with a FIXED one-bar first take: a free take's stop lands `align` later, past a bar.
        let mut rig = Rig::with(Opts { sr: 48000, start: 48000, align, ..Default::default() });
        rig.set(Command::SetBpm(300.0));
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(1.0));
        rig.set_input(code);
        rig.press(Command::RecDub(0));
        rig.advance_to(rig.end_frame() + 1);
        rig.set(Command::SetFixedLength(false));
        rig.set_level(0.0);
        rig.idle();
        assert_eq!(rig.state(0), LaneState::Playing, "align {align}: the first take committed");
        let master = rig.master();
        assert_eq!(master, 38_400, "one bar at 300 BPM");
        assert!(align > master + n);
        let pre = rig.pcm(0);
        let start = rig.next_boundary() + master / 2 + align + 17;
        let end = start + master / 3;
        rig.set_input(input);
        dub_over(&mut rig, start, end);
        rig.set_level(0.0);
        rig.idle();
        let pos = pos_fn(rig.anchor(), master, align);
        let mut want = pre.clone();
        common::dub::dub(&mut want, (start, end), n, 1.0, &pos, input);
        assert_eq!(off(&rig.pcm(0), &want), Vec::<usize>::new(), "align {align}");
        assert_eq!(rig.state(0), LaneState::Playing, "align {align}");
    }
}
