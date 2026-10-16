//! The listening render for the owner's WIDE COPY choice, rendered offline through the real engine as
//! four stereo WAV files. Ignored: it writes files and asserts the model it renders with.
//!
//!     cargo test -p lf-engine --release --test wide_copy_clip -- --ignored --nocapture
//!
//! It writes to `$WIDE_COPY_DIR`, else `$CARGO_TARGET_TMPDIR/wide-copy/`, and prints the path.
//!
//! The script, at 48 kHz and 100 BPM: a plucked riff (Karplus-Strong, 16 eighths, one note at a time) is
//! the input, played through `common::amp::ToyAmp` in the live slot 0, and recorded as lane 0's first
//! take: two bars (230,400 frames, 4.8 s). Then four runs, each ending in 6.0 s of the looper's output
//! (the stereo master, limiter included) read from a loop boundary, the input silent, the click off:
//! - `1-alone`: lane 0 as recorded, centred.
//! - `2-today`: COPY into lane 1; lane 0 panned left (-0.5), lane 1 right (+0.5); lane 1's DELAY on at
//!   1/16 (150 ms at 100 BPM), Fbk 0, Mix 100 %: the stagger a player can make today.
//! - `3-wide`: COPY WIDE: lane 1 is lane 0 shifted 20 ms, and the pans (-0.5, +0.5) are the engine's.
//! - `4-wide-mono`: file 3's left and right summed to mono (half of each), on both channels.
//!
//! All four share one gain (the loudest sample of them at -1 dBFS): no file is levelled on its own.
//! What this cannot hear: a real amp-sim, a real guitar, speakers. The riff is synthetic.

mod common;

use common::amp::ToyAmp;
use common::{Opts, Rig};
use lf_engine::dsp::fx::{FxKind, FxParam};
use lf_engine::grid::Frame;
use lf_engine::{Command, Event, LaneState};
use std::path::{Path, PathBuf};

const SR: u32 = 48000;
const BEAT: Frame = 28_800;
const BAR: Frame = 4 * BEAT;
const EIGHTH: Frame = BEAT / 2;
/// The WAV length: 6.0 s of the loop playing on (the riff comes round again at 4.8 s).
const SECONDS: f64 = 6.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Run {
    Alone,
    Today,
    Wide,
}

/// One note, `len` frames: a Karplus-Strong pluck with a soft pick, 10 ms fade-out at its end.
fn pluck(out: &mut [f32], at: usize, len: usize, hz: f64, level: f32, seed: &mut u32) {
    let n = (SR as f64 / hz - 0.5).round() as usize;
    let mut line = vec![0.0f32; n];
    let mut soft = 0.0f32;
    for s in line.iter_mut() {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        soft = 0.5 * soft + 0.5 * (*seed as f32 / u32::MAX as f32 * 2.0 - 1.0);
        *s = soft;
    }
    let fade = (0.010 * SR as f64) as usize;
    let mut pos = 0;
    for k in 0..len {
        let y = line[pos];
        line[pos] = 0.997 * 0.5 * (y + line[(pos + 1) % n]);
        pos = (pos + 1) % n;
        let gain = if k + fade > len { (len - k) as f32 / fade as f32 } else { 1.0 };
        out[at + k] += level * gain * y;
    }
}

/// The riff, two bars: an E-minor chug in eighths, the last eighth a rest.
fn riff() -> Vec<f32> {
    let mut out = vec![0.0f32; 2 * BAR as usize];
    let (e, g, a, b, d) = (164.81, 196.0, 220.0, 246.94, 293.66);
    let notes = [e, e, g, e, a, e, b, a, e, e, d, b, g, a, e];
    let mut seed = 0x2545_f491u32;
    for (k, hz) in notes.into_iter().enumerate() {
        let accent = if k % 4 == 0 { 0.8 } else { 0.6 };
        pluck(&mut out, k * EIGHTH as usize, (EIGHTH * 9 / 10) as usize, hz, accent, &mut seed);
    }
    out
}

struct Out {
    left: Vec<f32>,
    right: Vec<f32>,
    pcm: [Vec<f32>; 2],
    pans: [f32; 2],
}

fn script(run: Run) -> Out {
    let mut rig = Rig::with(Opts { sr: SR, start: SR as Frame, loop_seconds: 20.0, block: 128, align: 0 });
    rig.set(Command::SetBpm(100.0));
    rig.set(Command::SetMetronome(false));
    rig.install(0, Box::new(ToyAmp::new(SR)));
    rig.advance(rig.seconds(0.1));
    // The first take: the count-in, the riff from the downbeat, REC 240 frames past the second bar line.
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let downbeat = rig.count_one(mark) + 4 * BEAT;
    let sig = riff();
    rig.set_input(move |f| if f >= downbeat { sig.get((f - downbeat) as usize).copied().unwrap_or(0.0) } else { 0.0 });
    rig.advance_to(downbeat + 2 * BAR + 240);
    rig.press(Command::RecDub(0));
    rig.set_input(|_| 0.0);
    rig.advance(rig.seconds(0.25));
    rig.idle();
    assert_eq!((rig.master(), rig.state(0)), (2 * BAR, LaneState::Playing), "{run:?}: a two-bar loop");
    assert!(rig.pcm(0).iter().fold(0.0f32, |m, s| m.max(s.abs())) > 0.05, "{run:?}: the riff reached the loop");

    match run {
        Run::Alone => {}
        Run::Today => {
            rig.set(Command::SetPan(0, -0.5));
            rig.press(Command::Copy(0));
            rig.idle();
            rig.set(Command::SetPan(1, 0.5));
            rig.set(Command::SetFxParam(1, FxParam::Time, 3.0)); // 1/16
            rig.set(Command::SetFxParam(1, FxParam::Feedback, 0.0));
            rig.set(Command::SetFxParam(1, FxParam::Mix, 1.0));
            rig.set(Command::SetFxBypass(1, FxKind::Delay, false));
        }
        Run::Wide => {
            rig.press(Command::CopyWide(0));
            rig.idle();
        }
    }
    if run != Run::Alone {
        let copied = rig.events.iter().filter(|e| matches!(e, Event::Copied { .. })).count();
        assert_eq!((copied, rig.state(1)), (1, LaneState::Playing), "{run:?}: one copy, playing");
    }
    // Three loop passes for the pans, the delay's ramp and every tail to settle; then 6.0 s from a boundary.
    rig.advance(3 * 2 * BAR);
    rig.advance_to(rig.next_boundary());
    rig.keep_output();
    rig.advance(rig.seconds(SECONDS));
    let (src, copy) = (rig.pcm(0), rig.pcm(1));
    if run == Run::Wide {
        let d = 960; // 20 ms at 48 kHz
        let m = src.len();
        assert!((0..m).all(|h| copy[h] == src[(h + m - d) % m]), "the wide copy is its source 20 ms behind");
    }
    let fx = rig.engine.fx();
    Out { left: rig.heard.clone(), right: rig.heard_right.clone(), pcm: [src, copy], pans: [fx.pan(0), fx.pan(1)] }
}

/// A 16-bit stereo PCM WAV, `l` and `r` times `gain`, clamped. Returns how many samples the clamp touched.
fn write_stereo(path: &Path, l: &[f32], r: &[f32], gain: f32) -> std::io::Result<usize> {
    assert_eq!(l.len(), r.len());
    let data = (l.len() * 4) as u32;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&2u16.to_le_bytes()); // stereo
    bytes.extend_from_slice(&SR.to_le_bytes());
    bytes.extend_from_slice(&(SR * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    let mut clipped = 0;
    for (&a, &b) in l.iter().zip(r) {
        for s in [a, b] {
            let v = s * gain;
            clipped += usize::from(v.abs() > 1.0);
            bytes.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
        }
    }
    std::fs::write(path, bytes)?;
    Ok(clipped)
}

fn peak(v: &[f32]) -> f32 {
    v.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

fn db(x: f32) -> f64 {
    20.0 * (x as f64).log10()
}

/// The correlation of the two channels: 1 is mono, 0 unrelated, -1 opposite.
fn correlation(l: &[f32], r: &[f32]) -> f64 {
    let (mut lr, mut ll, mut rr) = (0.0f64, 0.0f64, 0.0f64);
    for (&a, &b) in l.iter().zip(r) {
        lr += a as f64 * b as f64;
        ll += a as f64 * a as f64;
        rr += b as f64 * b as f64;
    }
    lr / (ll * rr).sqrt()
}

#[test]
#[ignore = "a listening render: writes WAV files (run with --ignored --nocapture)"]
fn wide_copy_renders_alone_today_wide_and_mono() {
    let [alone, today, wide] = [Run::Alone, Run::Today, Run::Wide].map(script);
    assert!(alone.left.iter().zip(&alone.right).all(|(a, b)| a == b), "the lone riff is centred: both channels the same");
    assert_eq!(alone.pans, [0.0, 0.0]);
    assert_eq!(today.pans, [-0.5, 0.5]);
    assert_eq!(wide.pans, [-0.5, 0.5], "the engine's own pans for a wide copy");
    assert_eq!(wide.pcm[0], alone.pcm[0], "every run is the same take");
    assert_eq!(today.pcm[1], today.pcm[0], "COPY is a copy: the delay is a lane effect, not in the loop");
    let mono: Vec<f32> = wide.left.iter().zip(&wide.right).map(|(a, b)| 0.5 * (a + b)).collect();

    let files: [(&str, &[f32], &[f32]); 4] =
        [("1-alone", &alone.left, &alone.right), ("2-today", &today.left, &today.right), ("3-wide", &wide.left, &wide.right), ("4-wide-mono", &mono, &mono)];
    let raw = files.iter().map(|f| peak(f.1).max(peak(f.2))).fold(0.0f32, f32::max);
    let gain = 10f32.powf(-1.0 / 20.0) / raw;
    let dir = std::env::var_os("WIDE_COPY_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wide-copy"));
    std::fs::create_dir_all(&dir).expect("the output folder");
    for (name, l, r) in files {
        let clipped = write_stereo(&dir.join(format!("{name}.wav")), l, r, gain).expect("a WAV file");
        assert_eq!(clipped, 0, "{name}: no sample clipped");
        println!(
            "{name}: {:.1} s, peak L {:.2} R {:.2} dBFS (after the shared gain), L/R correlation {:.2}",
            l.len() as f64 / SR as f64,
            db(peak(l) * gain),
            db(peak(r) * gain),
            correlation(l, r)
        );
    }
    println!("gain {:.2} dB on every file (raw peak {:.2} dBFS)", db(gain), db(raw));
    println!("wrote {}", dir.display());
}
