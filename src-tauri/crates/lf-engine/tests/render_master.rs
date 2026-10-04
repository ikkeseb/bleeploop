//! The export's wet master rendered by the engine ([`lf_engine::render`]), which replaced the Tone
//! render (`src/session/render.ts`, removed): its length, its alignment with the stems (frame 0 is loop
//! position 0: the limiter's pre-delay is compensated, which the Tone export did not), a reversed lane,
//! every lane playing (a STOPPED one too), each lane's mix and the master's settings (a lane's mix in the
//! settings changes nothing), a delay's echo wrapping to the head, the warm-up's steady state (a delay
//! with the reverb after it, a delay at full feedback), no click and no instrument whatever the settings
//! say, the same bits at any block size, and every refusal an `Err`. A snapshot's mix into the render:
//! `tests/session.rs`.

mod common;

use lf_engine::dsp::compressor::Compressor;
use lf_engine::dsp::fx::{FxKind, FxParam, MAX_FEEDBACK};
use lf_engine::grid::{frames_per_bar, Frame};
use lf_engine::render::{wet_master, wet_master_with, RenderOptions, WetMaster, TAIL_THRESHOLD};
use lf_engine::{Command, InputSend, Instrument, LaneMix, Load, LoadTrack, NoteTarget};

const RATE: u32 = 48_000;
const BPM: u32 = 120;
/// One bar at 120 BPM and 48 kHz: 2 s.
const MASTER: usize = 96_000;
/// An impulse's level: far under the limiter's threshold.
const A: f32 = 0.25;

/// Lane `index` of one bar, in buffer order, PLAYING, with `impulses` (position, level) in it.
fn lane(index: u8, impulses: &[(usize, f32)]) -> LoadTrack {
    let mut buf = vec![0.0f32; MASTER];
    for &(p, a) in impulses {
        buf[p] = a;
    }
    LoadTrack { index, buf, peaks: Vec::new(), reversed: false, playing: true, mix: LaneMix::default() }
}

/// A one-bar session at 120 BPM.
fn load(tracks: Vec<LoadTrack>) -> Load {
    assert_eq!(frames_per_bar(BPM as f64, RATE), MASTER as Frame);
    Load { bpm: BPM, bars: 1, master: MASTER as Frame, tracks, result: None }
}

/// Every track at the default mix but those in `mixes` (by load position).
fn mixes_of(tracks: &[LoadTrack], mixes: &[(usize, LaneMix)]) -> Vec<LaneMix> {
    let mut out = vec![LaneMix::default(); tracks.len()];
    for &(k, mix) in mixes {
        out[k] = mix;
    }
    out
}

/// The master of `tracks`, track `k` at mix `mixes[k]` (the default mix when absent), the master's
/// settings in `settings`.
fn render(tracks: Vec<LoadTrack>, mixes: &[(usize, LaneMix)], settings: &[Command]) -> WetMaster {
    let mixes = mixes_of(&tracks, mixes);
    wet_master(RATE, load(tracks), &mixes, settings).expect("the render")
}

/// A lane's mix with its delay (8n, `feedback`, half wet) and its reverb send on.
fn delay_and_reverb(feedback: f64) -> LaneMix {
    let mut mix = LaneMix::default();
    mix.fx[FxKind::Delay.index()].params = [1.0, feedback, 0.5];
    mix.fx[FxKind::Delay.index()].bypassed = false;
    mix.fx[FxKind::Reverb.index()].params[0] = 0.5;
    mix.fx[FxKind::Reverb.index()].bypassed = false;
    mix
}

/// A delay alone: `time` (a division's index), `feedback`, `wet`.
fn delay(time: f64, feedback: f64, wet: f64) -> LaneMix {
    let mut mix = LaneMix::default();
    mix.fx[FxKind::Delay.index()].params = [time, feedback, wet];
    mix.fx[FxKind::Delay.index()].bypassed = false;
    mix
}

/// The master limiter's gain below its threshold (its makeup gain): an impulse through a fresh one.
fn limiter_gain() -> f32 {
    let mut limiter = Compressor::master_limiter(RATE as f32);
    let (n, p) = (RATE as usize, RATE as usize / 2);
    let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
    (l[p], r[p]) = (A, A);
    limiter.process(0, &mut l, &mut r);
    let g = l[p + limiter.latency()] / A;
    assert!(g > 0.5 && g < 2.0, "a plausible linear gain: {g}");
    g
}

/// The frame of `x`'s largest magnitude.
fn argmax(x: &[f32]) -> usize {
    (0..x.len()).max_by(|&a, &b| x[a].abs().total_cmp(&x[b].abs())).unwrap()
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

/// `actual` is `expected` within 0.1 dB: a lane with every effect bypassed is not bit-transparent (its
/// chain's crossfades mix their wet paths in at −56 dB and lift it about 0.035 dB, as in Tone:
/// `dsp/mod.rs`).
fn assert_close(actual: f32, expected: f32, what: &str) {
    let ratio = actual / expected;
    assert!((ratio - 1.0).abs() < 0.012, "{what}: {actual} is not {expected}");
}

/// One sound in `x`: `level` at `p`, and beside it no more than the bypassed chain's wet paths leak
/// (−56 dB; at most −50 dB of the impulse here).
fn assert_only(x: &[f32], p: usize, level: f32, what: &str) {
    assert_eq!(argmax(x), p, "{what}: the impulse lands on its play position");
    assert_close(x[p], level, what);
    let rest = x.iter().enumerate().filter(|&(k, _)| k != p).fold(0.0f32, |m, (_, v)| m.max(v.abs()));
    assert!(rest < 3.2e-3 * level.abs(), "{what}: {rest} beside the impulse");
}

/// The largest difference between two signals.
fn max_diff(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).fold(0.0f32, |m, (x, y)| m.max((x - y).abs()))
}

/// A burst of deterministic noise and two impulses: something for a delay and the reverb to ring on.
fn busy_lane(index: u8) -> LoadTrack {
    let mut t = lane(index, &[(30_000, A), (71_000, -A)]);
    let mut seed = 0x2545_f491u32;
    for x in &mut t.buf[1_000..5_800] {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        *x = (seed as f32 / u32::MAX as f32 - 0.5) * 0.2;
    }
    t
}

// a)
#[test]
fn the_master_is_exactly_the_loop_long_on_both_channels() {
    // 3 bars at 97 BPM and 44.1 kHz: a loop no power of two or block divides.
    let (rate, bpm, bars) = (44_100, 97, 3);
    let master = bars * frames_per_bar(bpm as f64, rate);
    let tracks = || {
        (0..2u8)
            .map(|i| {
                let buf = (0..master).map(|k| ((k as f32 * 0.01 * (i + 1) as f32).sin()) * 0.1).collect();
                LoadTrack { index: i * 2, buf, peaks: Vec::new(), reversed: false, playing: true, mix: LaneMix::default() }
            })
            .collect::<Vec<_>>()
    };
    let session = |tracks| Load { bpm, bars: bars as Frame, master, tracks, result: None };
    let plain = [LaneMix::default(); 2];
    let out = wet_master(rate, session(tracks()), &plain, &[]).unwrap();
    assert_eq!((out.left.len(), out.right.len()), (master as usize, master as usize));
    assert_eq!(out.warmup, 1, "no delay, no reverb: one warm-up pass");
    let more = wet_master_with(rate, session(tracks()), &plain, &[], RenderOptions { extra_warmup: 2, ..RenderOptions::default() }).unwrap();
    assert_eq!((more.left.len(), more.right.len(), more.warmup), (master as usize, master as usize, 3));
}

// b)
#[test]
fn an_impulse_lands_on_its_play_position_through_the_limiter_unscaled_but_for_its_makeup() {
    let g = limiter_gain();
    let p = 1_234;
    let out = render(vec![lane(2, &[(p, A)])], &[], &[]);
    assert_only(&out.left, p, A * g, "left");
    assert_only(&out.right, p, A * g, "right");
    // The head and the end of the loop line up too: no pre-delay left over, none cut.
    let out = render(vec![lane(1, &[(0, A), (MASTER - 1, -A)])], &[], &[]);
    assert_close(out.left[0], A * g, "loop position 0 at frame 0");
    assert_close(out.left[MASTER - 1], -A * g, "the last loop position at the last frame");
}

// c)
#[test]
fn a_reversed_lane_comes_out_in_play_order() {
    let g = limiter_gain();
    let b = 1_000;
    let mut t = lane(0, &[(b, A)]);
    t.reversed = true;
    let out = render(vec![t], &[], &[]);
    assert_only(&out.left, MASTER - 1 - b, A * g, "buffer position b plays at master - 1 - b");
}

// d)
#[test]
fn every_lane_plays_a_muted_one_is_silent_and_the_volumes_scale() {
    let g = limiter_gain();
    // Saved STOPPED, still in the master (F26).
    let mut stopped = lane(3, &[(500, A)]);
    stopped.playing = false;
    assert_only(&render(vec![stopped], &[], &[]).left, 500, A * g, "a STOPPED lane");

    // Muted lane 0 is out; lane 1 at half volume under a master at 0.8 is lane 1 alone at unity, times
    // 0.4 (the limiter is linear this far under its threshold). The lanes' mix is theirs, whatever the
    // settings say of it.
    let muted = LaneMix { muted: true, ..LaneMix::default() };
    let half = LaneMix { volume: 0.5, ..LaneMix::default() };
    let settings = [Command::SetMute(0, false), Command::SetVolume(0, 1.0), Command::SetMute(1, true), Command::SetMasterVolume(0.8)];
    let out = render(vec![lane(0, &[(1_000, A)]), lane(1, &[(2_000, A)])], &[(0, muted), (1, half)], &settings);
    let unity = render(vec![lane(1, &[(2_000, A)])], &[], &[]);
    assert_only(&out.left, 2_000, A * 0.5 * 0.8 * g, "lane and master volume");
    for (x, y) in [(&out.left, &unity.left), (&out.right, &unity.right)] {
        let scaled: Vec<f32> = y.iter().map(|v| v * 0.5 * 0.8).collect();
        let diff = max_diff(x, &scaled);
        assert!(diff < 1e-6, "the muted lane is silent and the volumes scale: {diff} off");
    }

    // A muted master is silence.
    let out = render(vec![lane(0, &[(1_000, A)])], &[], &[Command::SetMasterMute(true)]);
    assert_eq!(peak(&out.left).max(peak(&out.right)), 0.0, "a muted master");
}

// e)
#[test]
fn a_delay_echo_past_the_loop_end_rings_at_the_head() {
    let g = limiter_gain();
    // An 8n echo at 120 BPM is 12000 frames: from 6000 before the end it lands 6000 into the head.
    let p = MASTER - 6_000;
    let out = render(vec![lane(0, &[(p, A)])], &[(0, delay(1.0, 0.0, 0.5))], &[]);
    for x in [&out.left, &out.right] {
        let head = argmax(&x[..12_000]);
        assert!(head.abs_diff(6_000) <= 2, "the echo at {head}, not 6000");
        assert!(x[head].abs() > 0.3 * A * g, "the echo is there: {}", x[head]);
        assert!(x[p].abs() > 0.3 * A * g, "the dry impulse is there: {}", x[p]);
    }
}

// f)
#[test]
fn one_more_warm_up_pass_moves_the_master_by_less_than_the_threshold() {
    let mixes = [delay_and_reverb(0.6)];
    let out = render(vec![busy_lane(0)], &[(0, mixes[0])], &[]);
    // 0.6 feedback: 19 echoes of 0.25 s and a quantum (4.8 s), then the reverb's 2.62 s on the last of
    // them (the lane's reverb send follows its delay): 7.42 s over 2 s passes. The longer of the two
    // alone is 3 passes.
    assert_eq!(out.warmup, 4);
    let more = wet_master_with(RATE, load(vec![busy_lane(0)]), &mixes, &[], RenderOptions { extra_warmup: 1, ..RenderOptions::default() }).unwrap();
    let diff = max_diff(&out.left, &more.left).max(max_diff(&out.right, &more.right));
    assert!(diff < 1e-4, "the kept pass moved by {diff}");
    // The tails are in it: the end of the loop rings with the delay and the reverb.
    assert!(peak(&out.left[MASTER - 10_000..]) > 1e-3, "a tail at the loop's end");
    assert!(out.left != out.right, "the reverb's stereo");
}

// f2)
#[test]
fn a_delay_at_full_feedback_warms_for_every_echo_with_its_quantum() {
    // 23 bars at 300 BPM and 8 kHz: an 18.4 s loop. The 4n delay (0.2 s, the longest division) at the
    // maximum feedback takes 180 recurrences to fall below the threshold, each its division plus the
    // 128-frame quantum its feedback is late (16 ms here): 38.9 s, 2.11 passes, so 3. Counted without the
    // quantum the tail is 36 s, 1.96 passes: 2, and the kept pass misses an echo above the threshold.
    let (rate, bpm, bars) = (8_000, 300, 23);
    let master = bars * frames_per_bar(bpm as f64, rate);
    // One impulse at the loop's end: its echoes reach furthest into the passes after it.
    let session = || {
        let mut buf = vec![0.0f32; master as usize];
        buf[master as usize - 1] = A;
        Load { bpm, bars, master, tracks: vec![LoadTrack { index: 0, buf, peaks: Vec::new(), reversed: false, playing: true, mix: LaneMix::default() }], result: None }
    };
    let quarter = |feedback: f64| [delay(0.0, feedback, 1.0)];
    // The first echo's level in the master: the one the threshold is relative to.
    let echo = peak(&wet_master(rate, session(), &quarter(0.0), &[]).unwrap().left);
    assert!(echo > 0.3 * A, "the echo is there: {echo}");
    let out = wet_master(rate, session(), &quarter(MAX_FEEDBACK), &[]).unwrap();
    assert_eq!(out.warmup, 3);
    let more = wet_master_with(rate, session(), &quarter(MAX_FEEDBACK), &[], RenderOptions { extra_warmup: 1, ..RenderOptions::default() }).unwrap();
    let diff = max_diff(&out.left, &more.left).max(max_diff(&out.right, &more.right));
    let relative = diff / echo;
    assert!(relative < TAIL_THRESHOLD as f32, "one more pass moved the kept pass by {relative} of the echo");
}

// g)
#[test]
fn no_click_no_instrument_and_no_lane_mix_whatever_the_settings_say() {
    let noisy = [
        Command::SetMetronome(true),
        Command::SetClickVolume(1.0),
        Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)),
        Command::NoteOn(60, 1.0),
        Command::SetInstrumentGain(Instrument::Lead, 1.0),
        Command::SetInputSend(InputSend::Echo, true),
        Command::SetSlotLive(0, true),
        Command::SetBpm(200.0),
    ];
    let quiet = render(vec![lane(0, &[])], &[], &noisy);
    assert_eq!(peak(&quiet.left).max(peak(&quiet.right)), 0.0, "a silent lane renders silence");
    let plain = render(vec![busy_lane(0)], &[(0, delay_and_reverb(0.3))], &[]);
    let mut settings = noisy.to_vec();
    // A lane's mix in the settings (the host's memory of it) is not the snapshot's: it changes nothing.
    settings.extend([
        Command::SetVolume(0, 0.25),
        Command::SetMute(0, true),
        Command::SetFxParam(0, FxParam::Feedback, 0.9),
        Command::SetFxBypass(0, FxKind::Reverb, true),
        Command::SetFxBypass(0, FxKind::Filter, false),
    ]);
    let out = render(vec![busy_lane(0)], &[(0, delay_and_reverb(0.3))], &settings);
    assert!(out == plain, "the click, notes, sends, slots, tempo and a lane's mix change nothing");
}

// h)
#[test]
fn the_render_repeats_bit_for_bit_at_any_block_size() {
    let mixes = [delay_and_reverb(0.6), LaneMix::default()];
    let reference = render(vec![busy_lane(0), lane(4, &[(7, A)])], &[(0, mixes[0])], &[]);
    assert!(render(vec![busy_lane(0), lane(4, &[(7, A)])], &[(0, mixes[0])], &[]) == reference, "a second render differs");
    for block in [1, 127, 4096] {
        let out = wet_master_with(RATE, load(vec![busy_lane(0), lane(4, &[(7, A)])]), &mixes, &[], RenderOptions { block, ..RenderOptions::default() }).unwrap();
        assert!(out == reference, "block {block} differs");
    }
}

#[test]
fn every_refusal_is_an_error_with_a_sentence() {
    let one = [LaneMix::default()];
    let cases: Vec<(&str, Result<WetMaster, String>)> = vec![
        ("rate 0", wet_master(0, load(vec![lane(0, &[])]), &one, &[])),
        ("bars and length disagree", wet_master(RATE, Load { bars: 2, ..load(vec![lane(0, &[])]) }, &one, &[])),
        ("bpm out of range", wet_master(RATE, Load { bpm: 0, ..load(vec![lane(0, &[])]) }, &one, &[])),
        ("no loop", wet_master(RATE, Load { master: 0, ..load(vec![lane(0, &[])]) }, &one, &[])),
        ("a short buffer", wet_master(RATE, load(vec![LoadTrack { buf: vec![0.0; 10], ..lane(0, &[]) }]), &one, &[])),
        ("a NaN sample", wet_master(RATE, load(vec![lane(0, &[(5, f32::NAN)])]), &one, &[])),
        ("a lane twice", wet_master(RATE, load(vec![lane(0, &[]), lane(0, &[])]), &[LaneMix::default(); 2], &[])),
        ("a lane out of range", wet_master(RATE, load(vec![lane(9, &[])]), &one, &[])),
        ("a mix short", wet_master(RATE, load(vec![lane(0, &[]), lane(1, &[])]), &one, &[])),
        ("block 0", wet_master_with(RATE, load(vec![lane(0, &[])]), &one, &[], RenderOptions { block: 0, ..RenderOptions::default() })),
    ];
    for (what, result) in cases {
        match result {
            Err(e) => assert!(e.len() > 20, "{what}: {e}"),
            Ok(_) => panic!("{what}: rendered"),
        }
    }
}
