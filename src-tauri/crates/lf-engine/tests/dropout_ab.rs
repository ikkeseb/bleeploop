//! The listening render for the owner's dropout choice: what a dropout inside an overdub leaves in the
//! loop under each policy, rendered offline as WAV files. Ignored: it asserts the model it renders
//! with, not a behaviour, and writes files.
//!
//!     cargo test -p lf-engine --release --test dropout_ab -- --ignored --nocapture
//!
//! It writes to `$DROPOUT_AB_DIR`, else `$CARGO_TARGET_TMPDIR/dropout-ab/`, and prints the path.
//!
//! The script, at 48 kHz and 100 BPM: lane 0's one-bar loop (115,200 frames, 2.4 s) is a first take of
//! the built-in Bass playing A1, A1, C2 and D2 on the beats through the instrument record path, the
//! input silent. Then slot 0 (live) takes `common::amp::ToyAmp` (latency 256), and the input is a
//! Karplus-Strong power chord (E3, B3, E4) plucked on beats 1 and 3, a bar before the DUB and through
//! it. DUB lane 0 for one pass, its window a quarter beat before the bar line to a quarter beat before
//! the next. Inside it, on the "and" of beat 2, 0.9 s after the chord's pluck, 128 input frames are
//! silent: a dropped block. Four runs of that script:
//! - `clean`: no silent block.
//! - `kept`: the silent block, unflagged: the layer exactly as the engine records it today, and what a
//!   "keep it as recorded" policy keeps.
//! - `dropped`: no DUB: the loop a rejection gives back, bit for bit. Today's engine does this when the
//!   device side flags the block (a fourth run, `flagged`, asserts it and writes no file).
//! - `repaired`: smoothed over. No engine has the repair yet, so this file is computed here, not
//!   rendered: the revision's rule (engine-red revision 2, § a M1 item 4 and § b item 3) applied to the
//!   kept layer. The routing lane would mark the record-tap frames the silent block reaches through
//!   the amp's reported latency, `[d + 256, d + 384)`; the overdub punches out there (`fade_tail`'s
//!   5 ms back toward what it overwrote), writes nothing over the marked frames, and punches back in
//!   with `punch_in`'s 5 ms ramp from the first clean one. That is `common::dub` run over the two
//!   segments either side of the mark with the kept run's record tap as its input; the same reference
//!   over the whole window must reproduce the engine's kept layer bit for bit first.
//!
//! Every file is two passes of the loop as `Taps::looper` plays it (the click off) and all four share
//! one gain (the loudest of them at -1 dBFS): no file is levelled on its own. `compare.wav` is kept,
//! dropped and repaired in that order, 0.3 s of silence between them.
//! What this cannot hear: a real amp-sim. The toy amp's low-pass and cab carry the dropout on past the
//! marked frames, so `repaired` keeps what the amp itself made of it (the revision's § c rank 3), and
//! the ramps take 5 ms of clean playing on each side of the mark with them.

mod common;

use common::amp::ToyAmp;
use common::dub::{dub, pos_fn, ramp};
use common::{Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Command, Event, Instrument, LaneState, NoteTarget};
use std::path::PathBuf;

const SR: u32 = 48000;
const BLOCK: usize = 128;
const BEAT: Frame = 28_800;
const BAR: Frame = 4 * BEAT;
const LOST: Frame = 128;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Run {
    Clean,
    Kept,
    Flagged,
    Dropped,
}

struct Out {
    /// The loop before the DUB, after it, and two passes of `Taps::looper` from a bar line.
    pre: Vec<f32>,
    pcm: Vec<f32>,
    looper: Vec<f32>,
    /// The record tap from frame `tap.0`.
    tap: (Frame, Vec<f32>),
    anchor: Frame,
    /// The DUB window and the first silent input frame.
    window: (Frame, Frame),
    d: Frame,
    rejected: usize,
}

/// A Karplus-Strong power chord, `len` frames, plucked on beats 1 and 3 of every bar from frame 0.
fn chord(len: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; len];
    let mut seed = 0x2545_f491u32;
    for (hz, level) in [(164.81, 0.40), (246.94, 0.32), (329.63, 0.24)] {
        let n = (SR as f64 / hz - 0.5).round() as usize; // the averaging adds half a frame
        let mut line = vec![0.0f32; n];
        let mut at = 0;
        for (f, o) in out.iter_mut().enumerate() {
            if f as Frame % BAR == 0 || f as Frame % BAR == 2 * BEAT {
                let mut soft = 0.0f32;
                for s in line.iter_mut() {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    soft = 0.6 * soft + 0.4 * (seed as f32 / u32::MAX as f32 * 2.0 - 1.0); // a pick, not a click
                    *s = level * soft;
                }
            }
            let y = line[at];
            line[at] = 0.996 * 0.5 * (y + line[(at + 1) % n]);
            at = (at + 1) % n;
            *o += y;
        }
    }
    out
}

fn script(run: Run) -> Out {
    let mut rig = Rig::with(Opts { sr: SR, start: SR as Frame, loop_seconds: 20.0, block: BLOCK, align: 0 });
    rig.set(Command::SetBpm(100.0));
    rig.set(Command::SetMetronome(false));
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Bass)));
    // The Bass take: the count-in, one bar of notes, REC 240 frames past its bar line.
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let downbeat = rig.count_one(mark) + 4 * BEAT;
    for (k, note) in [33u8, 33, 36, 38].into_iter().enumerate() {
        let on = downbeat + k as Frame * BEAT;
        rig.send_at(on, Command::NoteOn(note, 0.8));
        rig.send_at(on + BEAT * 9 / 10, Command::NoteOff(note));
    }
    rig.advance_to(downbeat + BAR + 240);
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(0.25));
    rig.idle();
    assert_eq!((rig.master(), rig.state(0)), (BAR, LaneState::Playing), "{run:?}: a one-bar Bass loop");
    let pre = rig.pcm(0);
    let anchor = rig.anchor();

    // GO LIVE: slot 0 is live from the rig's start; the amp goes in now and crossfades in.
    rig.install(0, Box::new(ToyAmp::new(SR)));
    rig.set(Command::SetDubFeedback(0, 1.0));
    rig.advance(rig.seconds(0.1));
    // The guitar from the next bar line: input frame x lands on loop position x - anchor.
    let b1 = rig.next_boundary();
    let b2 = b1 + BAR;
    let d = b2 + 3 * BEAT / 2;
    let sig = chord(4 * BAR as usize);
    let silent = matches!(run, Run::Kept | Run::Flagged);
    rig.set_input(move |f| {
        if f < b1 || (silent && (d..d + LOST).contains(&f)) {
            0.0
        } else {
            sig.get((f - b1) as usize).copied().unwrap_or(0.0)
        }
    });
    let start = b2 - BEAT / 4;
    let end = start + BAR;
    let l = ToyAmp::LATENCY;
    if run != Run::Dropped {
        rig.send_at(start - l, Command::RecDub(0)); // a window opens the live latency after its press
        rig.send_at(end - l, Command::RecDub(0));
    }
    rig.advance_to(start - 2 * l);
    rig.keep_output();
    let tap0 = rig.frame;
    rig.advance_to(start);
    if run != Run::Dropped {
        assert_eq!(rig.window(), Some((0, Some(start), None)), "{run:?}: the window, the amp's latency after the press");
    }
    rig.advance_to(d);
    if run == Run::Flagged {
        rig.damage();
    }
    rig.advance(LOST);
    rig.advance_to(end + 1);
    assert!(rig.window().is_none() && rig.state(0) == LaneState::Playing, "{run:?}: the DUB closed");
    let tap = (tap0, rig.record.clone());
    rig.set_input(|_| 0.0);
    rig.idle();
    let pcm = rig.pcm(0);

    // Two passes from a bar line, read block by block off the looper tap.
    rig.advance_to(rig.next_boundary());
    let mut looper = Vec::new();
    for _ in 0..2 * BAR / BLOCK as Frame {
        rig.advance(BLOCK as Frame);
        looper.extend_from_slice(rig.engine.taps().looper);
    }
    let rejected = rig.events.iter().filter(|e| matches!(e, Event::TakeRejected { overdub: true, .. })).count();
    Out { pre, pcm, looper, tap, anchor, window: (start, end), d, rejected }
}

fn bits(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

/// The largest |a - b| over positions `[lo, hi)`, in dBFS (-inf when equal).
fn peak_db(a: &[f32], b: &[f32], lo: usize, hi: usize) -> f64 {
    let p = (lo..hi).map(|i| (a[i] - b[i]).abs()).fold(0.0f32, f32::max);
    20.0 * (p as f64).log10()
}

#[test]
#[ignore = "a listening render: writes WAV files (run with --ignored --nocapture)"]
fn dropout_ab_renders_kept_dropped_and_repaired() {
    let [clean, kept, flagged, dropped] = [Run::Clean, Run::Kept, Run::Flagged, Run::Dropped].map(script);
    for o in [&kept, &flagged, &dropped] {
        assert!(bits(&o.pre, &clean.pre), "every run dubs over the same Bass loop");
    }
    assert_eq!((clean.rejected, kept.rejected, dropped.rejected), (0, 0, 0), "nothing but the flagged run rejects");
    assert_eq!(flagged.rejected, 1, "today's engine rejects a layer a flagged block falls in");
    assert!(bits(&flagged.pcm, &dropped.pcm) && bits(&dropped.pcm, &dropped.pre), "the rejection gives back the loop before the DUB");

    let (start, end) = kept.window;
    let n = ramp(SR);
    let pos = pos_fn(kept.anchor, BAR, ToyAmp::LATENCY);
    let tap = |o: &Out| {
        let (t0, v) = (o.tap.0, o.tap.1.clone());
        move |f: Frame| v[(f - t0) as usize]
    };
    // The reference model first: one punch over the whole window reproduces each engine layer.
    for o in [&clean, &kept] {
        let mut whole = o.pre.clone();
        dub(&mut whole, (start, end), n, 1.0, &pos, tap(o));
        assert!(bits(&whole, &o.pcm), "common::dub reproduces the engine's layer");
    }
    // The repair: the mark is where the silent block reaches the tap through the amp's reported latency.
    let (a, b) = (kept.d + ToyAmp::LATENCY, kept.d + ToyAmp::LATENCY + LOST);
    let mut repaired = kept.pre.clone();
    dub(&mut repaired, (start, a), n, 1.0, &pos, tap(&kept));
    dub(&mut repaired, (b, end), n, 1.0, &pos, tap(&kept));

    // Taps::looper plays the loop as stored (volume 1, the click off), so the repaired file is that loop
    // twice, as the other three are.
    for o in [&clean, &kept, &dropped] {
        assert!(bits(&o.looper, &[o.pcm.as_slice(), o.pcm.as_slice()].concat()), "the looper tap plays the stored loop");
    }
    let looped = [repaired.as_slice(), repaired.as_slice()].concat();

    // How each differs from the clean layer, around the mark (loop positions).
    let (pa, pb) = (pos(a), pos(b));
    let first = (0..BAR as usize).find(|&i| kept.pcm[i].to_bits() != clean.pcm[i].to_bits());
    assert_eq!(first, Some(pa), "the silent block first changes the kept layer where the mark starts");
    let cab = (0.015 * SR as f64) as usize;
    println!("the mark: tap frames [{a}, {b}) = loop positions [{pa}, {pb}), {:.1} s into the bar; ramps of {n} frames", pa as f64 / SR as f64);
    println!("clean guitar peak over [pa - n, pb + n): {:.1} dBFS", peak_db(&clean.pcm, &clean.pre, pa - n as usize, pb + n as usize));
    for (name, layer) in [("kept", &kept.pcm), ("repaired", &repaired)] {
        let spans = [
            ("5 ms before the mark", pa - n as usize, pa),
            ("the mark", pa, pb),
            ("5 ms after it", pb, pb + n as usize),
            ("the 15 ms after that", pb + n as usize, pb + n as usize + cab),
            ("before all that", 0, pa - n as usize),
            ("after all that", pb + n as usize + cab, BAR as usize),
        ];
        let line: Vec<String> = spans.iter().map(|&(what, lo, hi)| format!("{what} {:.1}", peak_db(layer, &clean.pcm, lo, hi))).collect();
        println!("{name} vs clean, peak difference in dBFS: {}", line.join("; "));
        let last = (0..BAR as usize).rev().find(|&i| (layer[i] - clean.pcm[i]).abs() > 1e-5);
        println!("{name}: last position more than -100 dBFS off clean: {last:?} ({:.1} ms past the mark)", last.map_or(0.0, |p| (p as f64 - pb as f64) / SR as f64 * 1000.0));
    }

    // One gain for every file: the loudest at -1 dBFS.
    let files = [("clean", clean.looper.as_slice()), ("kept", kept.looper.as_slice()), ("dropped", dropped.looper.as_slice()), ("repaired", looped.as_slice())];
    let peak = files.iter().flat_map(|f| f.1.iter()).fold(0.0f32, |m, s| m.max(s.abs()));
    let gain = 10f32.powf(-1.0 / 20.0) / peak;
    let dir = std::env::var_os("DROPOUT_AB_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("dropout-ab"));
    std::fs::create_dir_all(&dir).expect("the output folder");
    for (name, pcm) in files {
        let clipped = common::wav::write(&dir.join(format!("{name}.wav")), SR, pcm, gain).expect("a WAV file");
        assert_eq!(clipped, 0, "{name}: no sample clipped");
    }
    let gap = vec![0.0f32; (0.3 * SR as f64) as usize];
    let compare = [files[1].1, &gap, files[2].1, &gap, files[3].1].concat();
    common::wav::write(&dir.join("compare.wav"), SR, &compare, gain).expect("compare.wav");
    println!("gain {:.2} dB on every file (raw peak {:.2} dBFS); compare.wav {:.1} s", 20.0 * (gain as f64).log10(), 20.0 * (peak as f64).log10(), compare.len() as f64 / SR as f64);
    println!("wrote {}", dir.display());
}
