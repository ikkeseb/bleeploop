//! A `synth` scenario of the Tone references replayed live through a SYNTH (`units::SynthUnit`,
//! `docs/unit-contract.md` §4), as its slot drives it: picked to the scenario's instrument before its
//! first call, each event a slot event on its own frame in `block`-frame calls from DSP frame 0, every
//! call under assert_no_alloc. The oracle is the instrument's DSP alone, called as the SYNTH's bank calls
//! it (`instruments.rs` `Bank`): the pick's release and wheels at frame 0, a note scheduled
//! [`LEAD`] after its frame at its velocity as an f32 clamped to 0..1, a wheel applied on its frame
//! and re-applying both wheels (the mod wheel clamped to 0..1), the output as the bank's sum leaves it
//! (`0.0 + x`, so a -0.0 reads +0.0). The DSP alone is pinned to Tone by the calling file; this pins
//! the unit's event handling to the DSP, bit for bit, at blocks 1, 64, 127, 128 and 480.
//!
//! A bit-for-bit replay of the Tone reference itself through the unit is not possible: the reference
//! schedules every note up front and a wheel on Tone's offline clock ticks, where the bank applies them
//! on their frames (and the 44.1 kHz scenario's vibrato wave is the page's first synth's, at 48 kHz).

use std::sync::Arc;

use assert_no_alloc::assert_no_alloc;
use lf_engine::dsp::buffer_source::AudioBuffer;
use lf_engine::dsp::synth::{Bass, DrumKit, PolySynth};
use lf_engine::grid::{Frame, Meter};
use lf_engine::instruments::LEAD;
use lf_engine::units::{SynthUnit, SYNTH_IDS};
use lf_engine::{Instrument, SlotEvent, SlotEventKind, UnitCx, UnitParam, UnitProcessor};

use super::refs::{self, ScriptEvent};
use super::violation_count;

/// The block sizes a unit replay runs at.
pub const BLOCKS: [usize; 5] = [1, 64, 127, 128, 480];

/// One instrument's DSP as the bank calls it.
pub trait Oracle {
    fn note_on(&mut self, note: u8, velocity: f64, time: f64, frame: u64);
    fn note_off(&mut self, note: u8, time: f64, frame: u64);
    fn all_notes_off(&mut self, frame: u64);
    /// Both wheels on `frame` (the bank's `wheels`): the bend, then the mod wheel.
    fn wheels(&mut self, bend: f64, modulation: f64, time: f64, frame: u64);
    /// Frames `frame..frame + left.len()` into `left`/`right` (overwritten).
    fn render(&mut self, frame: u64, left: &mut [f32], right: &mut [f32]);
}

impl Oracle for PolySynth {
    fn note_on(&mut self, note: u8, velocity: f64, time: f64, frame: u64) {
        PolySynth::note_on(self, note, velocity, time, frame)
    }
    fn note_off(&mut self, note: u8, time: f64, frame: u64) {
        PolySynth::note_off(self, note, time, frame)
    }
    fn all_notes_off(&mut self, frame: u64) {
        PolySynth::all_notes_off(self, frame)
    }
    fn wheels(&mut self, bend: f64, modulation: f64, time: f64, frame: u64) {
        self.set_pitch_bend(bend, time, frame);
        self.set_modulation(modulation, time, frame);
    }
    fn render(&mut self, frame: u64, left: &mut [f32], right: &mut [f32]) {
        PolySynth::render(self, frame, left);
        right.copy_from_slice(left);
    }
}

impl Oracle for Bass {
    fn note_on(&mut self, note: u8, velocity: f64, time: f64, frame: u64) {
        Bass::note_on(self, note, velocity, time, frame)
    }
    fn note_off(&mut self, note: u8, time: f64, frame: u64) {
        Bass::note_off(self, note, time, frame)
    }
    fn all_notes_off(&mut self, frame: u64) {
        Bass::all_notes_off(self, frame)
    }
    fn wheels(&mut self, bend: f64, modulation: f64, time: f64, frame: u64) {
        self.set_pitch_bend(bend, time, frame);
        self.set_modulation(modulation, time, frame);
    }
    fn render(&mut self, frame: u64, left: &mut [f32], right: &mut [f32]) {
        Bass::render(self, frame, left);
        right.copy_from_slice(left);
    }
}

/// The kit takes no wheels and no all-notes-off (the bank skips both for it).
impl Oracle for DrumKit {
    fn note_on(&mut self, note: u8, velocity: f64, time: f64, frame: u64) {
        DrumKit::note_on(self, note, velocity, time, frame)
    }
    fn note_off(&mut self, note: u8, time: f64, frame: u64) {
        DrumKit::note_off(self, note, time, frame)
    }
    fn all_notes_off(&mut self, _frame: u64) {}
    fn wheels(&mut self, _bend: f64, _modulation: f64, _time: f64, _frame: u64) {}
    fn render(&mut self, frame: u64, left: &mut [f32], right: &mut [f32]) {
        DrumKit::render(self, frame, left, right)
    }
}

/// The scenario's events by frame (a stable sort: one frame keeps the manifest's order), those inside
/// its render only, as the files' live replays apply them.
fn events(id: &str) -> Vec<&'static ScriptEvent> {
    let s = refs::scenario(id);
    let mut events: Vec<&ScriptEvent> = s.events.iter().filter(|e| (e.frame as usize) < s.frames).collect();
    events.sort_by_key(|e| e.frame);
    events
}

fn value(e: &ScriptEvent) -> f64 {
    e.value.as_ref().and_then(|v| v.as_f64()).expect("a control value")
}

fn slot_event(e: &ScriptEvent) -> SlotEventKind {
    match e.kind.as_str() {
        "on" => SlotEventKind::NoteOn { key: e.note.unwrap(), velocity: e.velocity.unwrap() as f32 },
        "off" => SlotEventKind::NoteOff { key: e.note.unwrap() },
        "bend" => SlotEventKind::PitchBend { semitones: value(e) },
        "mod" => SlotEventKind::Modulation { depth: value(e) },
        other => panic!("unknown event {other}"),
    }
}

/// The scenario through a SYNTH picked to `instrument` over Tone's noise tables `[white, pink]`, `block`
/// frames per call: [left, right].
pub fn render_unit(id: &str, instrument: Instrument, [white, pink]: &[Arc<AudioBuffer>; 2], block: usize) -> [Vec<f32>; 2] {
    let s = refs::scenario(id);
    let cx = |frame: u64| UnitCx { frame: frame as Frame, dsp_frame: frame as Frame, bpm: 120.0, meter: Meter::FOUR_FOUR };
    let mut unit = SynthUnit::with_noise(s.rate, white, pink);
    let index = SYNTH_IDS.iter().position(|&i| i == instrument.id()).expect("every instrument is in SYNTHS") as f64;
    let events = events(id);
    let mut calls: Vec<SlotEvent> = Vec::with_capacity(events.len());
    let (mut left, mut right) = (vec![0.0f32; s.frames], vec![0.0f32; s.frames]);
    let before = violation_count();
    assert_eq!(assert_no_alloc(|| unit.set_param(&cx(0), UnitParam::Instrument, index)), Some(index), "{id}: the pick");
    let (mut at, mut next) = (0usize, 0usize);
    while at < s.frames {
        let n = block.min(s.frames - at);
        calls.clear();
        while next < events.len() && (events[next].frame as usize) < at + n {
            calls.push(SlotEvent { offset: (events[next].frame as usize - at) as u32, kind: slot_event(events[next]) });
            next += 1;
        }
        let (l, r) = (&mut left[at..at + n], &mut right[at..at + n]);
        assert_no_alloc(|| unit.process(&cx(at as u64), [&[], &[]], &calls, [l, r]));
        at += n;
    }
    assert_eq!(violation_count(), before, "{id}: the SYNTH allocated");
    [left, right]
}

/// The scenario through `oracle` alone, called as the bank calls it: [left, right].
pub fn render_oracle(id: &str, oracle: &mut dyn Oracle) -> [Vec<f32>; 2] {
    let s = refs::scenario(id);
    let rate = s.rate as f32 as f64;
    let time = |f: u64| f as f64 / rate;
    let events = events(id);
    // The pick on frame 0: its release, then both wheels at rest.
    oracle.all_notes_off(0);
    oracle.wheels(0.0, 0.0, 0.0, 0);
    let (mut bend, mut modulation) = (0.0, 0.0);
    let (mut left, mut right) = (vec![0.0f32; s.frames], vec![0.0f32; s.frames]);
    let (mut at, mut next) = (0usize, 0usize);
    while at < s.frames {
        while next < events.len() && events[next].frame as usize <= at {
            let e = events[next];
            let f = e.frame;
            match e.kind.as_str() {
                "on" => oracle.note_on(e.note.unwrap(), (e.velocity.unwrap() as f32).clamp(0.0, 1.0) as f64, time(f + LEAD as u64), f),
                "off" => oracle.note_off(e.note.unwrap(), time(f + LEAD as u64), f),
                "bend" => {
                    bend = value(e);
                    oracle.wheels(bend, modulation, time(f), f);
                }
                "mod" => {
                    modulation = value(e).clamp(0.0, 1.0);
                    oracle.wheels(bend, modulation, time(f), f);
                }
                other => panic!("unknown event {other}"),
            }
            next += 1;
        }
        let limit = events.get(next).map_or(s.frames, |e| e.frame as usize);
        let n = 128.min(limit - at);
        oracle.render(at as u64, &mut left[at..at + n], &mut right[at..at + n]);
        at += n;
    }
    for x in left.iter_mut().chain(right.iter_mut()) {
        *x += 0.0;
    }
    [left, right]
}

/// The scenario through a SYNTH picked to `instrument` at every block of [`BLOCKS`], bit for bit
/// against `oracle` (a fresh instrument as the bank builds it).
pub fn assert_unit(id: &str, instrument: Instrument, noise: &[Arc<AudioBuffer>; 2], oracle: &mut dyn Oracle) {
    let want = render_oracle(id, oracle);
    assert!(want[0].iter().any(|&x| x != 0.0), "{id}: the oracle is silent");
    for block in BLOCKS {
        let got = render_unit(id, instrument, noise, block);
        for c in 0..2 {
            if let Some(k) = got[c].iter().zip(&want[c]).position(|(a, b)| a.to_bits() != b.to_bits()) {
                panic!("{id}: through a SYNTH, block {block}, side {c}: first difference at frame {k}: {} vs {}", got[c][k], want[c][k]);
            }
        }
    }
}
