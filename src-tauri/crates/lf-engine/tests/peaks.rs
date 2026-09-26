//! The waveform peaks the UI draws (`lf_engine::overview`), after reverse.mjs A's peak check: the peaks
//! describe what the lane holds. The engine keeps peaks per buffer, in buffer order, and a lane's view
//! names its buffer and orientation (the feed turns a reversed lane's bins around), so the check here is
//! "every bin of the lane's buffer is the min and max of its frames up to what the lane holds", through
//! a take in flight, the commit's fill, an overdub, UNDO's swap, REVERSE and CLEAR.

mod common;

use common::{code, Rig};
use lf_engine::overview::PEAK_FRAMES;
use lf_engine::Command;

/// Every bin of lane `lane`'s buffer, up to the frames the lane holds, is its min and max.
fn peaks_describe(rig: &Rig, lane: usize, tag: &str) {
    let looper = rig.engine.looper();
    let view = looper.overview().lane(lane);
    let pcm = &looper.live_buffer(lane)[..view.frames as usize];
    for (bin, frames) in pcm.chunks(PEAK_FRAMES).enumerate() {
        let want = frames.iter().fold((0.0f32, 0.0f32), |(lo, hi), &x| (lo.min(x), hi.max(x)));
        assert_eq!(looper.overview().bin(view.buf, bin), want, "{tag}: bin {bin}");
    }
}

#[test]
fn the_peaks_follow_every_write_and_the_view_follows_undo_reverse_and_clear() {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(200.0));
    rig.set_input(|f| code(f) - 0.25);
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(1.2) + 3000);
    let view = rig.engine.looper().overview().lane(0);
    assert!(view.frames > 0 && view.frames < rig.seconds(1.2), "a take in flight shows what it captured: {}", view.frames);
    peaks_describe(&rig, 0, "take in flight");

    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.idle();
    let master = rig.master();
    let committed = rig.engine.looper().overview().lane(0);
    assert_eq!((committed.buf, committed.frames, committed.reversed), (view.buf, master, false));
    peaks_describe(&rig, 0, "committed, the fill done");

    rig.set_level(0.5);
    rig.advance_to(rig.next_boundary() - master / 3);
    rig.press(Command::RecDub(0));
    rig.advance(master / 2);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.idle();
    peaks_describe(&rig, 0, "overdubbed");
    let dubbed = rig.engine.looper().overview().lane(0);
    assert_eq!(dubbed.buf, committed.buf, "an overdub sums in place");

    rig.press(Command::Undo(0));
    rig.idle();
    let undone = rig.engine.looper().overview().lane(0);
    assert_ne!(undone.buf, dubbed.buf, "UNDO shows the other buffer");
    peaks_describe(&rig, 0, "undone");

    rig.press(Command::Reverse(0));
    let reversed = rig.engine.looper().overview().lane(0);
    assert_eq!((reversed.buf, reversed.reversed), (undone.buf, true), "REVERSE writes nothing");
    peaks_describe(&rig, 0, "reversed");

    rig.press(Command::Clear(0));
    assert_eq!(rig.engine.looper().overview().lane(0).frames, 0, "a cleared lane holds nothing");
}

#[test]
fn a_reader_takes_each_changed_bin_once() {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(200.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.idle();
    let overview = rig.engine.looper().overview().clone();
    let buf = overview.lane(0).buf;
    let mut bins = Vec::new();
    overview.take_dirty(buf, |b| bins.push(b));
    let loop_bins = (master as usize).div_ceil(PEAK_FRAMES);
    assert!((0..loop_bins).all(|b| bins.iter().filter(|&&x| x == b).count() == 1), "every bin of the loop changed, once: {bins:?}");
    assert!(bins.windows(2).all(|w| w[0] < w[1]), "(and a take's tail past the loop, which the feed does not draw)");
    bins.clear();
    rig.advance(4800);
    overview.take_dirty(buf, |b| bins.push(b));
    assert!(bins.is_empty(), "playing writes nothing: {bins:?}");
}
