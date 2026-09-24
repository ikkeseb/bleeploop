//! The poly synths (`dsp::synth`: lead, pad, piano, organ on `dsp::oscillator`, `dsp::envelope` and
//! `dsp::delay`) against the Tone references: each `synth-*` scenario replayed from the manifest the
//! way the probe drove it, the same bits at any block size (scheduled up front, as the probe does, and
//! live, as the engine will: every call made at its own frame), and no allocation while scheduling or
//! rendering.
//!
//! The probe calls every noteOn/noteOff before the offline render starts (Blink's context time 0), and
//! pitch bend and mod wheel from Tone's offline clock: a tick every 128 frames whose time is a running
//! float sum of `128 / rate`, on the first tick whose rounded frame reaches the event's frame. The
//! replay does the same.

mod common;

use assert_no_alloc::assert_no_alloc;
use common::refs::{self, Class, ScriptEvent};
use common::violation_count;
use lf_engine::dsp::synth::{PolyKind, PolySynth};

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

/// Make the planned calls, all while Blink's context time is 0 (before the render starts).
fn schedule(synth: &mut PolySynth, calls: &[(&ScriptEvent, f64)]) {
    for &(e, time) in calls {
        match e.kind.as_str() {
            "on" => synth.note_on(e.note.unwrap(), e.velocity.unwrap(), time, 0),
            "off" => synth.note_off(e.note.unwrap(), time, 0),
            _ => control(synth, e, time, 0),
        }
    }
}

fn control(synth: &mut PolySynth, e: &ScriptEvent, now: f64, frame: u64) {
    let value = e.value.as_ref().and_then(|v| v.as_f64()).expect("a control value");
    match e.kind.as_str() {
        "bend" => synth.set_pitch_bend(value, now, frame),
        "mod" => synth.set_modulation(value, now, frame),
        other => panic!("unknown event {other}"),
    }
}

fn kind(id: &str) -> PolyKind {
    PolyKind::from_id(refs::scenario(id).setup["synth"].as_str().expect("setup.synth")).expect("a poly synth")
}

/// The synth as the probe built it. Tone caches the vibrato's LFO wave module-wide, so every scenario
/// runs the wave the page's first pitched synth built, at that scenario's rate (the manifest keeps the
/// probe's order).
fn build(id: &str) -> PolySynth {
    let first = refs::manifest().scenarios.iter().find(|s| s.group == "synth" && s.setup["synth"] != "drum").expect("a pitched synth");
    PolySynth::with_lfo_wave_rate(kind(id), refs::scenario(id).rate as f32, first.rate as f32, 0.0, 0)
}

/// The scenario rendered `block` frames per call, every call under assert_no_alloc.
fn render(id: &str, block: usize) -> Vec<f32> {
    let s = refs::scenario(id);
    let mut synth = build(id);
    let calls = plan(id);
    let before = violation_count();
    assert_no_alloc(|| schedule(&mut synth, &calls));
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

/// Live replay: every call made at its own frame (notes a quantum ahead, controls at their frame),
/// blocks split where an event falls, as the engine's process() splits them.
fn render_live(id: &str, block: usize) -> Vec<f32> {
    const LEAD: u64 = 128;
    let s = refs::scenario(id);
    let rate = s.rate as f64;
    let mut synth = build(id);
    let mut events: Vec<&ScriptEvent> = s.events.iter().collect();
    events.sort_by_key(|e| e.frame);
    let mut next = 0;
    let mut out = vec![0.0f32; s.frames];
    let mut at = 0usize;
    let before = violation_count();
    while at < s.frames {
        while next < events.len() && events[next].frame <= at as u64 {
            let e = events[next];
            let f = e.frame;
            let time = (f + LEAD) as f64 / rate;
            assert_no_alloc(|| match e.kind.as_str() {
                "on" => synth.note_on(e.note.unwrap(), e.velocity.unwrap(), time, f),
                "off" => synth.note_off(e.note.unwrap(), time, f),
                _ => control(&mut synth, e, time, f),
            });
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

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

fn assert_scenario(id: &str, class: Class) {
    let reference = render(id, 128);
    refs::assert_class(id, std::slice::from_ref(&reference), class);
    for block in [1, 64, 127, 480] {
        assert!(bits(&render(id, block)) == bits(&reference), "{id}: block {block} differs");
    }
    let live = render_live(id, 128);
    for block in [1, 64, 127, 480] {
        assert!(bits(&render_live(id, block)) == bits(&live), "{id}: live, block {block} differs");
    }
}

#[test]
fn synth_lead_notes_48000() {
    assert_scenario("synth-lead-notes-48000", Class::N);
}

#[test]
fn synth_lead_poly_48000() {
    assert_scenario("synth-lead-poly-48000", Class::N);
}

#[test]
fn synth_lead_poly_44100() {
    assert_scenario("synth-lead-poly-44100", Class::N);
}

#[test]
fn synth_piano_notes_48000() {
    assert_scenario("synth-piano-notes-48000", Class::N);
}

#[test]
fn synth_piano_poly_48000() {
    assert_scenario("synth-piano-poly-48000", Class::N);
}

#[test]
fn synth_organ_notes_48000() {
    assert_scenario("synth-organ-notes-48000", Class::N);
}

#[test]
fn synth_organ_poly_48000() {
    assert_scenario("synth-organ-poly-48000", Class::N);
}

#[test]
fn synth_pad_notes_48000() {
    assert_scenario("synth-pad-notes-48000", Class::N);
}

#[test]
fn synth_pad_poly_48000() {
    assert_scenario("synth-pad-poly-48000", Class::N);
}

/// The densest case's cost: the pad, all 12 voices sounding, per 128 frames at 48 k. Ignored by
/// default (timing on CI is noise):
///   cargo test -p lf-engine --test synth -- --ignored --nocapture
#[test]
#[ignore]
fn pad_full_polyphony_cost_per_quantum() {
    let rate = 48000.0f32;
    let quanta = 48000 * 20 / 128;
    let mut synth = PolySynth::new(PolyKind::Pad, rate, 0.0, 0);
    for k in 0..12u8 {
        synth.note_on(48 + 4 * k, 0.8, 0.0, 0);
    }
    synth.set_pitch_bend(0.5, 0.0, 0);
    synth.set_modulation(1.0, 0.0, 0);
    let mut out = [0.0f32; 128];
    let started = std::time::Instant::now();
    for q in 0..quanta {
        synth.render(q as u64 * 128, &mut out);
    }
    let per = started.elapsed().as_secs_f64() / quanta as f64;
    println!("pad x12: {:.2} µs per 128 frames ({:.2} % of the quantum at 48 k)", per * 1e6, per / (128.0 / 48000.0) * 100.0);
    std::hint::black_box(&out);
}
