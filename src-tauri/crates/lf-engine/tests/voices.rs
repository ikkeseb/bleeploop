//! The bass and the drum kit (`dsp::synth::{bass, drum}`) against the Tone references: each scenario
//! replayed from the manifest the way the probe drove it, the same bits at any block size (scheduled
//! up front, as the probe does, and live, every call made at its own frame), and no allocation while
//! scheduling or rendering. The replay rules are `tests/synth.rs`'s: notes before the render starts,
//! pitch bend and mod wheel on Tone's offline clock ticks.
//!
//! The drum kit's random draws are its three NoiseSynth starts (clap, e-snare, snare, in call order):
//! the probe seeds `Math.random` with mulberry32(1000 + the scenario's index), and the kit's random
//! source replays that seed, checked against the recorded draws.

mod common;

use std::sync::{Arc, OnceLock};

use assert_no_alloc::assert_no_alloc;
use common::refs::{self, Class, ScriptEvent};
use common::violation_count;
use lf_engine::dsp::buffer_source::AudioBuffer;
use lf_engine::dsp::noise::NoiseTables;
use lf_engine::dsp::rng::Mulberry32;
use lf_engine::dsp::synth::{Bass, DrumKit};

const BLOCKS: [usize; 4] = [1, 64, 127, 480];

/// A scenario's calls in the probe's order, each with the Tone time it is made at: notes up front,
/// controls at their ticks.
fn plan(id: &str) -> Vec<(&'static ScriptEvent, f64)> {
    let s = refs::scenario(id);
    let rate = s.rate as f64;
    let is_note = |e: &&ScriptEvent| matches!(e.kind.as_str(), "on" | "off");
    let mut calls: Vec<(&ScriptEvent, f64)> = s.events.iter().filter(is_note).map(|e| (e, e.frame as f64 / rate)).collect();
    let mut controls: Vec<&ScriptEvent> = s.events.iter().filter(|e| !is_note(e)).collect();
    controls.sort_by_key(|e| e.frame);
    let step = 128.0 / rate;
    let mut now = 0.0f64;
    for e in controls {
        while ((now * rate).round() as u64) < e.frame {
            now += step;
        }
        calls.push((e, now));
    }
    calls
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

// ── Bass ────────────────────────────────────────────────────────────────────────────────────────

/// The bass as the probe built it: the vibrato's LFO wave is the one the page's first pitched synth
/// built (Tone caches it module-wide; see `tests/synth.rs`).
fn bass(id: &str) -> Bass {
    let first = refs::manifest().scenarios.iter().find(|s| s.group == "synth" && s.setup["synth"] != "drum").expect("a pitched synth");
    Bass::with_lfo_wave_rate(refs::scenario(id).rate as f32, first.rate as f32, 0.0, 0)
}

fn bass_call(synth: &mut Bass, e: &ScriptEvent, time: f64, frame: u64) {
    match e.kind.as_str() {
        "on" => synth.note_on(e.note.unwrap(), e.velocity.unwrap(), time, frame),
        "off" => synth.note_off(e.note.unwrap(), time, frame),
        kind => {
            let value = e.value.as_ref().and_then(|v| v.as_f64()).expect("a control value");
            match kind {
                "bend" => synth.set_pitch_bend(value, time, frame),
                "mod" => synth.set_modulation(value, time, frame),
                other => panic!("unknown event {other}"),
            }
        }
    }
}

/// The scenario scheduled up front, rendered `block` frames per call, all under assert_no_alloc.
fn render_bass(id: &str, block: usize) -> Vec<f32> {
    let s = refs::scenario(id);
    let mut synth = bass(id);
    let calls = plan(id);
    let before = violation_count();
    assert_no_alloc(|| {
        for &(e, time) in &calls {
            bass_call(&mut synth, e, time, 0);
        }
    });
    let mut out = vec![0.0f32; s.frames];
    let mut at = 0;
    while at < s.frames {
        let n = block.min(s.frames - at);
        let chunk = &mut out[at..at + n];
        assert_no_alloc(|| synth.render(at as u64, chunk));
        at += n;
    }
    assert_eq!(violation_count(), before, "{id}: scheduling or rendering allocated");
    out
}

/// Live replay: every call made at its own frame (a quantum ahead), blocks split where an event falls.
fn render_bass_live(id: &str, block: usize) -> Vec<f32> {
    const LEAD: u64 = 128;
    let s = refs::scenario(id);
    let rate = s.rate as f64;
    let mut synth = bass(id);
    let mut events: Vec<&ScriptEvent> = s.events.iter().collect();
    events.sort_by_key(|e| e.frame);
    let mut next = 0;
    let mut out = vec![0.0f32; s.frames];
    let mut at = 0usize;
    let before = violation_count();
    while at < s.frames {
        while next < events.len() && events[next].frame <= at as u64 {
            let e = events[next];
            assert_no_alloc(|| bass_call(&mut synth, e, (e.frame + LEAD) as f64 / rate, e.frame));
            next += 1;
        }
        let limit = events.get(next).map_or(s.frames, |e| (e.frame as usize).min(s.frames));
        let n = block.min(limit - at);
        let chunk = &mut out[at..at + n];
        assert_no_alloc(|| synth.render(at as u64, chunk));
        at += n;
    }
    assert_eq!(violation_count(), before, "{id}: the live replay allocated");
    out
}

fn assert_bass(id: &str, class: Class) {
    let reference = render_bass(id, 128);
    refs::assert_class(id, std::slice::from_ref(&reference), class);
    for block in BLOCKS {
        assert!(bits(&render_bass(id, block)) == bits(&reference), "{id}: block {block} differs");
    }
    let live = render_bass_live(id, 128);
    for block in BLOCKS {
        assert!(bits(&render_bass_live(id, block)) == bits(&live), "{id}: live, block {block} differs");
    }
}

#[test]
fn synth_bass_notes_48000() {
    assert_bass("synth-bass-notes-48000", Class::N);
}

#[test]
fn synth_bass_poly_48000() {
    assert_bass("synth-bass-poly-48000", Class::N);
}

// ── Drum kit ────────────────────────────────────────────────────────────────────────────────────

const DRUM: &str = "synth-drum-kit-48000";

/// Tone's noise tables (white, pink), regenerated from their seed and checked against the manifest.
fn tables() -> &'static (Arc<AudioBuffer>, Arc<AudioBuffer>) {
    static T: OnceLock<(Arc<AudioBuffer>, Arc<AudioBuffer>)> = OnceLock::new();
    T.get_or_init(|| {
        let t = &refs::manifest().tables;
        let NoiseTables { white, pink } = NoiseTables::generate(&mut Mulberry32::new(t.seed));
        refs::assert_sha256("white table", &[&white[0], &white[1]], &t.sha256["white"]);
        refs::assert_sha256("pink table", &[&pink[0], &pink[1]], &t.sha256["pink"]);
        let buffer = |[l, r]: [Vec<f32>; 2]| Arc::new(AudioBuffer::new(t.rate as f32, vec![l, r]));
        (buffer(white), buffer(pink))
    })
}

/// The probe's `Math.random` for the scenario: mulberry32(1000 + its index), checked against the
/// draws the manifest recorded.
fn drum_rng() -> Mulberry32 {
    let m = refs::manifest();
    let index = m.scenarios.iter().position(|s| s.id == DRUM).expect("the drum scenario");
    let rng = Mulberry32::new(1000 + index as u32);
    let recorded = &refs::scenario(DRUM).random;
    let mut check = rng.clone();
    let replayed: Vec<f64> = recorded.iter().map(|_| check.next_f64()).collect();
    assert_eq!(&replayed, recorded, "the kit's random source is not the probe's");
    rng
}

fn drum_kit() -> DrumKit {
    let (white, pink) = tables();
    DrumKit::new(refs::scenario(DRUM).rate as f32, white, pink, drum_rng(), 0.0, 0)
}

fn render_drum(block: usize, live: bool) -> Vec<Vec<f32>> {
    const LEAD: u64 = 128;
    let s = refs::scenario(DRUM);
    let rate = s.rate as f64;
    let mut kit = drum_kit();
    let mut events: Vec<&ScriptEvent> = s.events.iter().collect();
    events.sort_by_key(|e| e.frame);
    assert!(events.iter().all(|e| e.kind == "on"), "the kit only takes hits");
    let before = violation_count();
    let mut next = 0;
    if !live {
        assert_no_alloc(|| {
            for e in &events {
                kit.note_on(e.note.unwrap(), e.velocity.unwrap(), e.frame as f64 / rate, 0);
            }
        });
        next = events.len();
    }
    let (mut left, mut right) = (vec![0.0f32; s.frames], vec![0.0f32; s.frames]);
    let mut at = 0usize;
    while at < s.frames {
        while next < events.len() && events[next].frame <= at as u64 {
            let e = events[next];
            assert_no_alloc(|| kit.note_on(e.note.unwrap(), e.velocity.unwrap(), (e.frame + LEAD) as f64 / rate, e.frame));
            next += 1;
        }
        let limit = events.get(next).map_or(s.frames, |e| (e.frame as usize).min(s.frames));
        let n = block.min(limit - at);
        let (l, r) = (&mut left[at..at + n], &mut right[at..at + n]);
        assert_no_alloc(|| kit.render(at as u64, l, r));
        at += n;
    }
    assert_eq!(violation_count(), before, "{DRUM}: scheduling or rendering allocated");
    vec![left, right]
}

#[test]
fn synth_drum_kit_48000() {
    let reference = render_drum(128, false);
    refs::assert_class(DRUM, &reference, Class::N);
    let whole: Vec<Vec<u32>> = reference.iter().map(|c| bits(c)).collect();
    for block in BLOCKS {
        let r: Vec<Vec<u32>> = render_drum(block, false).iter().map(|c| bits(c)).collect();
        assert!(r == whole, "{DRUM}: block {block} differs");
    }
    let live: Vec<Vec<u32>> = render_drum(128, true).iter().map(|c| bits(c)).collect();
    for block in BLOCKS {
        let r: Vec<Vec<u32>> = render_drum(block, true).iter().map(|c| bits(c)).collect();
        assert!(r == live, "{DRUM}: live, block {block} differs");
    }
}

/// The kit's cost with all 16 voices ringing, per 128 frames at 48 k. Ignored by default (timing on
/// CI is noise):
///   cargo test -p lf-engine --release --test voices -- --ignored --nocapture
#[test]
#[ignore]
fn drum_kit_all_voices_cost_per_quantum() {
    let rate = 48000.0f32;
    let (white, pink) = tables();
    let notes: Vec<u8> = DrumKit::new(rate, white, pink, Mulberry32::new(1), 0.0, 0).notes().collect();
    assert_eq!(notes.len(), 16);
    // The shortest voice (the pedal hat) rings for 41 ms: time the first 40 ms, all 16 sounding.
    let quanta = (0.04 * 48000.0 / 128.0) as u64;
    let rounds = 200;
    let (mut left, mut right) = ([0.0f32; 128], [0.0f32; 128]);
    let mut total = 0.0;
    for _ in 0..rounds {
        let mut kit = DrumKit::new(rate, white, pink, Mulberry32::new(1), 0.0, 0);
        for &note in &notes {
            kit.note_on(note, 1.0, 0.0, 0);
        }
        let started = std::time::Instant::now();
        for q in 0..quanta {
            kit.render(q * 128, &mut left, &mut right);
        }
        total += started.elapsed().as_secs_f64();
    }
    let per = total / (rounds as f64 * quanta as f64);
    println!("drum kit x16: {:.2} µs per 128 frames ({:.2} % of the quantum at 48 k)", per * 1e6, per / (128.0 / 48000.0) * 100.0);
    std::hint::black_box((&left, &right));
}
