//! SYNTH as a chain source (`lf_engine::units::SynthUnit`, `docs/unit-contract.md` §4, §9 E3):
//! - *SYNTH alone is today's bank:* one note script (chords, overlaps, a retrigger, velocities, a pitch
//!   bend and a mod wheel sweep) through `NoteTarget::Builtin(i)` and through a slot holding a SYNTH
//!   picked to `i` (the slot the note target, the input OTT off): the master bus, the heard output and
//!   the record tap, bit for bit per channel, for each of the six, with the record line delayed (an
//!   input side over the lead) and clamped (one under it).
//! - Two SYNTH slots are two banks: notes and wheels to one never reach or sound in the other.
//! - A pick mid-note releases the held note on the pick's frame (it rings out, then falls silent where
//!   an unpicked note sustains), hands the new instrument the wheels, allocates nothing, and does all of
//!   it for the instrument already picked too, as `Instruments::select` does; a pick and a NoteOn on one
//!   frame give one result in both ring orders (the pick first).
//! - A source change (SYNTH → SYNTH, SYNTH → a fake source, a withdrawal and its reseat, a removal)
//!   under a held note and a note whose NoteOff never comes: each released on the applied frame, the
//!   new source silent until its own NoteOn, a stray NoteOff dropped, in both orders of a replacement
//!   and a NoteOff sent for the same block.
//! - The wheels never reach a chain without a SYNTH, nor split its render.
//!
//! Every render runs through `tests/common::Rig` (each `process` under `assert_no_alloc`) or the local
//! `assert_no_alloc` wrapper, bit-identical across block sizes 1, 32, 64, 127, 128, 480 and 1024.

mod common;

use std::sync::{Arc, OnceLock};

use common::{Opts, Rig};
use lf_engine::dsp::buffer_source::AudioBuffer;
use lf_engine::grid::{Frame, Meter};
use lf_engine::instruments::LEAD;
use lf_engine::units::{tone_noise, SynthUnit, SYNTH_IDS};
use lf_engine::{
    Command, Event, Instrument, Left, NoteTarget, SeatOp, SeatOutcome, SlotEvent, SlotEventKind, UnitCx, UnitId,
    UnitInfo, UnitKind, UnitParam, UnitProcessor,
};

const BLOCKS: [usize; 7] = [1, 32, 64, 127, 128, 480, 1024];
const SR: u32 = 48_000;
/// The rig's first frame (`Opts::default`).
const S0: Frame = 48_000;

/// A direct unit call under `assert_no_alloc` that fails on an allocation: with `warn_debug` the crate
/// only counts violations, so the count must not move (as `Rig` checks its renders).
fn assert_no_alloc<T, F: FnOnce() -> T>(f: F) -> T {
    let before = common::violation_count();
    let out = ::assert_no_alloc::assert_no_alloc(f);
    assert_eq!(common::violation_count(), before, "a direct unit call allocated");
    out
}

fn noise() -> &'static [Arc<AudioBuffer>; 2] {
    static NOISE: OnceLock<[Arc<AudioBuffer>; 2]> = OnceLock::new();
    NOISE.get_or_init(|| tone_noise(SR))
}

fn synth() -> SynthUnit {
    let [white, pink] = noise();
    SynthUnit::with_noise(SR, white, pink)
}

/// `i`'s index in the UI's `SYNTHS` order, as `UnitParam::Instrument` takes it.
fn ui_index(i: Instrument) -> f64 {
    SYNTH_IDS.iter().position(|&id| id == i.id()).expect("every instrument is in SYNTHS") as f64
}

fn cx_at(frame: Frame) -> UnitCx {
    UnitCx { frame, dsp_frame: frame, bpm: 120.0, meter: Meter::FOUR_FOUR }
}

/// A SYNTH picked to `i` before it is installed (as its host sends its params with the add).
fn picked(i: Instrument) -> SynthUnit {
    let mut u = synth();
    assert_eq!(assert_no_alloc(|| u.set_param(&cx_at(0), UnitParam::Instrument, ui_index(i))), Some(ui_index(i)));
    assert_eq!(u.instrument(), Some(i));
    u
}

fn assert_bits(a: &[f32], b: &[f32], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: length");
    if let Some(k) = a.iter().zip(b).position(|(x, y)| x.to_bits() != y.to_bits()) {
        panic!("{what}: first difference at frame {k} (S0 + {}): {} vs {}", k, a[k], b[k]);
    }
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|&v| (v as f64) * (v as f64)).sum()
}

/// What a render kept: the master bus, the heard output and the record tap, each side.
#[derive(Clone)]
struct Taps {
    bus: [Vec<f32>; 2],
    heard: [Vec<f32>; 2],
    record: [Vec<f32>; 2],
}

impl Taps {
    fn of(rig: &Rig) -> Taps {
        Taps {
            bus: [rig.bus.clone(), rig.bus_right.clone()],
            heard: [rig.heard.clone(), rig.heard_right.clone()],
            record: [rig.record.clone(), rig.record_right.clone()],
        }
    }

    fn assert_eq(&self, other: &Taps, what: &str) {
        for c in 0..2 {
            assert_bits(&self.bus[c], &other.bus[c], &format!("{what}: bus side {c}"));
            assert_bits(&self.heard[c], &other.heard[c], &format!("{what}: heard side {c}"));
            assert_bits(&self.record[c], &other.record[c], &format!("{what}: record side {c}"));
        }
    }
}

fn rig(block: usize) -> Rig {
    let mut rig = Rig::with(Opts { block, ..Opts::default() });
    rig.keep_output();
    rig
}

/// Render to `until`, each command stamped on its frame (`S0 +` its offset) and sent ahead of it, so it
/// is taken before it is due at any block size.
fn play(rig: &mut Rig, script: &[(Frame, Command)], until: Frame) {
    let mut next = 0;
    while rig.frame < until {
        while next < script.len() && S0 + script[next].0 < rig.frame + 2048 {
            rig.send_at(S0 + script[next].0, script[next].1);
            next += 1;
        }
        let step = (rig.block as Frame).min(until - rig.frame);
        rig.advance(step);
    }
    assert_eq!(next, script.len(), "the script runs past the render");
}

/// A deterministic phrase: a chord, an overlapping note, a retrigger, velocities, a pitch bend sweep up
/// and down and a mod wheel sweep (both mid-quantum), then everything off and one more note. The drum
/// kit gets its own keys.
fn phrase(i: Instrument) -> Vec<(Frame, Command)> {
    let k: [u8; 5] = if i == Instrument::Drums { [36, 38, 42, 46, 49] } else { [60, 64, 67, 72, 55] };
    let mut s = vec![
        (1_000, Command::NoteOn(k[0], 0.8)),
        (1_000, Command::NoteOn(k[1], 0.6)),
        (1_000, Command::NoteOn(k[2], 0.9)),
        (5_037, Command::NoteOn(k[3], 0.45)),
        (9_001, Command::NoteOff(k[0])),
        (9_500, Command::NoteOn(k[0], 0.3)),
        (20_011, Command::NoteOff(k[1])),
        (20_011, Command::NoteOff(k[2])),
        (22_000, Command::NoteOff(k[3])),
        (22_501, Command::NoteOn(k[4], 1.0)),
        (26_000, Command::NoteOff(k[0])),
        (30_003, Command::NoteOff(k[4])),
        (31_000, Command::PitchBend(0.0)),
        (31_000, Command::Modulation(0.0)),
        (32_000, Command::NoteOn(k[2], 0.7)),
        (40_000, Command::NoteOff(k[2])),
    ];
    for j in 0..24 {
        let up = j as f64 / 12.0;
        s.push((12_000 + j * 301, Command::PitchBend(up)));
        s.push((19_300 + j * 211, Command::PitchBend(2.0 - 3.5 * up / 2.0)));
    }
    for j in 0..20 {
        s.push((14_000 + j * 257, Command::Modulation(j as f64 / 19.0)));
    }
    s.sort_by_key(|&(f, _)| f);
    s
}

const PHRASE_END: Frame = S0 + 60_000;

/// The phrase through today's `NoteTarget::Builtin(i)`.
fn builtin_phrase(i: Instrument, block: usize, input_latency: Frame) -> Taps {
    let mut rig = rig(block);
    rig.input_latency = input_latency;
    let mut script = vec![(0, Command::SelectInstrument(NoteTarget::Builtin(i)))];
    script.extend(phrase(i));
    play(&mut rig, &script, PHRASE_END);
    Taps::of(&rig)
}

/// The phrase through slot 0 holding a SYNTH picked to `i` on the wire, the slot the note target.
fn unit_phrase(i: Instrument, block: usize, input_latency: Frame) -> Taps {
    let mut rig = rig(block);
    rig.input_latency = input_latency;
    let id = rig.install_id(0, Box::new(synth()));
    let mut script = vec![(0, Command::SetUnitParam(id, UnitParam::Instrument, ui_index(i))), (0, Command::SelectInstrument(NoteTarget::Slot(0)))];
    script.extend(phrase(i));
    play(&mut rig, &script, PHRASE_END);
    assert!(rig.events.iter().any(|e| matches!(*e, Event::UnitParam { unit, param: UnitParam::Instrument, value, .. } if unit == id && value == ui_index(i))), "the pick is answered");
    Taps::of(&rig)
}

#[test]
fn synth_alone_is_todays_bank() {
    for i in Instrument::ALL {
        // 256: the record line delays the bank by 256 - LEAD; 64: under the lead, the line clamps to 0.
        for input_latency in [256, 64] {
            let reference = builtin_phrase(i, 128, input_latency);
            assert!(energy(&reference.bus[0]) > 1.0 && energy(&reference.record[1]) > 1.0, "{i:?}: the phrase sounds and records");
            for block in BLOCKS {
                let what = format!("{i:?}, input side {input_latency}, block {block}");
                if block != 128 {
                    builtin_phrase(i, block, input_latency).assert_eq(&reference, &format!("{what}: today's bank across block sizes"));
                }
                unit_phrase(i, block, input_latency).assert_eq(&reference, &format!("{what}: SYNTH against today's bank"));
            }
        }
    }
}

// ── A unit that logs what it saw ────────────────────────────────────────────────────────────────────

/// Frames of output a spy keeps, from `S0`.
const SPY_FRAMES: usize = 4 * SR as usize;

/// Wraps a unit: logs every event it is handed at its absolute frame, every call's frame and length,
/// and its output frame by frame from `S0`, all in buffers allocated before it is installed.
struct Spy<U> {
    inner: U,
    events: Vec<(Frame, SlotEventKind)>,
    calls: Vec<(Frame, usize)>,
    out: [Vec<f32>; 2],
    stops: usize,
}

impl<U> Spy<U> {
    fn new(inner: U) -> Self {
        Spy { inner, events: Vec::with_capacity(4096), calls: Vec::with_capacity(1 << 18), out: [vec![0.0; SPY_FRAMES], vec![0.0; SPY_FRAMES]], stops: 0 }
    }

    fn keys(&self) -> Vec<(Frame, bool, u8)> {
        self.events
            .iter()
            .filter_map(|&(f, k)| match k {
                SlotEventKind::NoteOn { key, .. } => Some((f, true, key)),
                SlotEventKind::NoteOff { key } => Some((f, false, key)),
                _ => None,
            })
            .collect()
    }

    fn wheels(&self) -> usize {
        self.events.iter().filter(|(_, k)| matches!(k, SlotEventKind::PitchBend { .. } | SlotEventKind::Modulation { .. })).count()
    }

    /// Its output's energy over frames `a..b` (absolute).
    fn energy(&self, a: Frame, b: Frame) -> f64 {
        let r = (a - S0) as usize..(b - S0) as usize;
        energy(&self.out[0][r.clone()]) + energy(&self.out[1][r])
    }

    fn silent(&self, a: Frame, b: Frame) -> bool {
        let r = (a - S0) as usize..(b - S0) as usize;
        self.out[0][r.clone()].iter().chain(&self.out[1][r]).all(|&v| v == 0.0)
    }
}

impl<U: UnitProcessor + 'static> UnitProcessor for Spy<U> {
    fn info(&self) -> UnitInfo {
        self.inner.info()
    }

    fn process(&mut self, cx: &UnitCx, input: [&[f32]; 2], events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let [ol, or] = out;
        self.inner.process(cx, input, events, [&mut *ol, &mut *or]);
        for e in events {
            assert!(self.events.len() < self.events.capacity(), "the spy's event log is full");
            self.events.push((cx.frame + e.offset as Frame, e.kind));
        }
        assert!(self.calls.len() < self.calls.capacity(), "the spy's call log is full");
        self.calls.push((cx.frame, ol.len()));
        for (k, (&l, &r)) in ol.iter().zip(or.iter()).enumerate() {
            let at = cx.frame + k as Frame - S0;
            if (0..SPY_FRAMES as Frame).contains(&at) {
                self.out[0][at as usize] = l;
                self.out[1][at as usize] = r;
            }
        }
    }

    fn set_param(&mut self, cx: &UnitCx, param: UnitParam, value: f64) -> Option<f64> {
        self.inner.set_param(cx, param, value)
    }

    fn set_bypass(&mut self, cx: &UnitCx, bypassed: bool) -> bool {
        self.inner.set_bypass(cx, bypassed)
    }

    fn stop(&mut self) {
        self.stops += 1;
        self.inner.stop();
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// A source stand-in: 0.5 on both sides on every frame from a NoteOn while it holds a key.
struct FakeSource {
    held: [u64; 2],
}

impl UnitProcessor for FakeSource {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: true, latency: LEAD }
    }

    fn process(&mut self, _cx: &UnitCx, _input: [&[f32]; 2], events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let mut next = 0;
        for k in 0..out[0].len() {
            while let Some(e) = events.get(next).filter(|e| e.offset as usize == k) {
                match e.kind {
                    SlotEventKind::NoteOn { key, .. } => self.held[(key / 64) as usize] |= 1 << (key % 64),
                    SlotEventKind::NoteOff { key } => self.held[(key / 64) as usize] &= !(1 << (key % 64)),
                    _ => {}
                }
                next += 1;
            }
            let y = if self.held != [0; 2] { 0.5 } else { 0.0 };
            out[0][k] = y;
            out[1][k] = y;
        }
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// A plugin effect stand-in: its input, unchanged.
struct Pass;

impl UnitProcessor for Pass {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: false, latency: 0 }
    }

    fn process(&mut self, _cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let [ol, or] = out;
        ol.copy_from_slice(input[0]);
        or.copy_from_slice(input[1]);
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// Every unit `rig`'s `slot` hands back from now, by id, the rig rendering one frame at a time until
/// `count` have come (each removed first, if `remove`).
fn take_back(rig: &mut Rig, slot: usize, ids: &[UnitId]) -> Vec<(UnitId, Box<dyn UnitProcessor>, Left)> {
    for &id in ids {
        rig.remove_id(slot, id);
    }
    let mut back = Vec::new();
    for _ in 0..4 * SR {
        while let Some(r) = rig.port(slot).returned_unit() {
            back.push((r.id, r.unit, r.left));
        }
        if ids.iter().all(|id| back.iter().any(|(b, ..)| b == id)) {
            break;
        }
        rig.advance(1);
    }
    back
}

fn spy_of<U: UnitProcessor + 'static>(unit: Box<dyn UnitProcessor>) -> Box<Spy<U>> {
    unit.into_any().downcast::<Spy<U>>().ok().expect("a spy of that unit")
}

/// The frame an `Event::Seat` applied `op` to `unit` on.
fn applied(rig: &Rig, unit: UnitId, op: SeatOp) -> Frame {
    rig.events
        .iter()
        .find_map(|e| match *e {
            Event::Seat { frame, unit: u, op: o, outcome: SeatOutcome::Applied, .. } if u == unit && o == op => Some(frame),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {op:?} applied to unit {unit}"))
}

// ── Two banks ───────────────────────────────────────────────────────────────────────────────────────

/// Slot 0 and slot 1 each hold a lead SYNTH (`which`: both, or only one); the target is slot 0 for a
/// chord with a bend, then slot 1 for another. Answers each SYNTH's spy and the bus.
fn two_banks(block: usize, which: [bool; 2]) -> (Vec<Option<Box<Spy<SynthUnit>>>>, [Vec<f32>; 2]) {
    let mut rig = rig(block);
    let mut ids = [0; 2];
    for slot in 0..2 {
        if which[slot] {
            ids[slot] = rig.install_id(slot, Box::new(Spy::new(picked(Instrument::Lead))));
        }
    }
    let script = [
        (0, Command::SelectInstrument(NoteTarget::Slot(0))),
        (1_000, Command::NoteOn(60, 0.8)),
        (1_000, Command::NoteOn(64, 0.7)),
        (3_011, Command::PitchBend(0.5)),
        (4_007, Command::Modulation(0.7)),
        (20_000, Command::SelectInstrument(NoteTarget::Slot(1))),
        (21_000, Command::NoteOn(67, 0.9)),
        (22_013, Command::PitchBend(-1.0)),
        (30_000, Command::NoteOff(67)),
    ];
    play(&mut rig, &script, S0 + 60_000);
    let bus = [rig.bus.clone(), rig.bus_right.clone()];
    let mut spies = vec![None, None];
    for slot in 0..2 {
        if which[slot] {
            let back = take_back(&mut rig, slot, &[ids[slot]]);
            spies[slot] = Some(spy_of::<SynthUnit>(back.into_iter().next().expect("the SYNTH back").1));
        }
    }
    (spies, bus)
}

#[test]
fn two_synth_slots_are_two_banks() {
    let mut first: Option<[Vec<f32>; 2]> = None;
    for block in BLOCKS {
        let (both, bus) = two_banks(block, [true, true]);
        let (a, b) = (both[0].as_ref().unwrap(), both[1].as_ref().unwrap());
        // Slot 0's SYNTH hears the first phrase and the select's release, never slot 1's notes or wheels.
        assert_eq!(a.keys(), vec![(S0 + 1_000, true, 60), (S0 + 1_000, true, 64), (S0 + 20_000, false, 60), (S0 + 20_000, false, 64)], "block {block}: slot 0's notes");
        assert_eq!(a.wheels(), 2, "block {block}: slot 0's wheels");
        assert_eq!(b.keys(), vec![(S0 + 21_000, true, 67), (S0 + 30_000, false, 67)], "block {block}: slot 1's notes");
        assert_eq!(b.wheels(), 1, "block {block}: slot 1's wheels");
        // Slot 1's bank is silent until its own note sounds, a lead's lead after it.
        assert!(b.silent(S0, S0 + 21_000 + LEAD), "block {block}: slot 1 sounds before its note");
        assert!(b.energy(S0 + 21_000 + LEAD, S0 + 30_000) > 1.0, "block {block}: slot 1's note sounds");
        assert!(a.energy(S0 + 1_000 + LEAD, S0 + 20_000) > 1.0, "block {block}: slot 0's chord sounds");
        // The bus is the two slots' sum: each alone renders what it renders beside the other.
        let (alone0, bus0) = two_banks(block, [true, false]);
        let (alone1, bus1) = two_banks(block, [false, true]);
        assert_eq!(alone0[0].as_ref().unwrap().keys(), a.keys(), "block {block}: slot 0 alone");
        assert_eq!(alone1[1].as_ref().unwrap().keys(), b.keys(), "block {block}: slot 1 alone");
        for c in 0..2 {
            let sum: Vec<f32> = bus0[c].iter().zip(&bus1[c]).map(|(x, y)| x + y).collect();
            assert_bits(&bus[c], &sum, &format!("block {block}: the bus against each slot alone, side {c}"));
        }
        match &first {
            None => first = Some(bus),
            Some(f) => {
                for c in 0..2 {
                    assert_bits(&bus[c], &f[c], &format!("block {block}: across block sizes, side {c}"));
                }
            }
        }
    }
}

// ── A pick mid-note ─────────────────────────────────────────────────────────────────────────────────

const HOLD: Frame = 2_000;
/// The pick: mid-quantum, the lead held since `HOLD`, never released by a NoteOff.
const PICK: Frame = 30_037;

/// Slot 0's SYNTH (or today's bank, `builtin`) on lead, the wheels set, note 60 held from `HOLD`; at
/// `PICK` (if any) a pick of `to`; then note 48 at `PICK + 9_000`. `same_frame`: a NoteOn 52 on the
/// pick's frame, sent before the pick (`true`) or after it (`false`).
fn pick_script(builtin: bool, to: Option<Instrument>, same_frame: Option<bool>, id: UnitId) -> Vec<(Frame, Command)> {
    let pick = |i: Instrument| if builtin { Command::SelectInstrument(NoteTarget::Builtin(i)) } else { Command::SetUnitParam(id, UnitParam::Instrument, ui_index(i)) };
    let mut s = vec![(0, pick(Instrument::Lead))];
    if !builtin {
        s.push((0, Command::SelectInstrument(NoteTarget::Slot(0))));
    }
    s.extend([(500, Command::PitchBend(1.0)), (700, Command::Modulation(0.6)), (HOLD, Command::NoteOn(60, 0.9))]);
    if let Some(to) = to {
        match same_frame {
            Some(true) => s.extend([(PICK, Command::NoteOn(52, 0.8)), (PICK, pick(to))]),
            Some(false) => s.extend([(PICK, pick(to)), (PICK, Command::NoteOn(52, 0.8))]),
            None => s.push((PICK, pick(to))),
        }
    }
    s.push((PICK + 9_000, Command::NoteOn(48, 0.7)));
    s
}

const PICK_END: Frame = S0 + 3 * SR as Frame;

fn pick_render(block: usize, builtin: bool, to: Option<Instrument>, same_frame: Option<bool>) -> (Taps, Option<Box<Spy<SynthUnit>>>) {
    let mut rig = rig(block);
    rig.input_latency = 256;
    let id = if builtin { 0 } else { rig.install_id(0, Box::new(Spy::new(synth()))) };
    play(&mut rig, &pick_script(builtin, to, same_frame, id), PICK_END);
    let taps = Taps::of(&rig);
    let spy = (!builtin).then(|| spy_of::<SynthUnit>(take_back(&mut rig, 0, &[id]).into_iter().next().expect("the SYNTH back").1));
    (taps, spy)
}

#[test]
fn a_pick_mid_note_releases_on_its_frame_and_hands_over_the_wheels() {
    let sounding = S0 + PICK + LEAD;
    for to in [Instrument::Bass, Instrument::Lead, Instrument::Pad] {
        let reference = pick_render(128, true, Some(to), None).0;
        let (held_on, _) = pick_render(128, false, None, None);
        for block in BLOCKS {
            let what = format!("pick {to:?}, block {block}");
            let (taps, spy) = pick_render(block, false, Some(to), None);
            // Today's `Instruments::select`, bit for bit: the release on the pick's frame, the ring-out,
            // the wheels handed to the picked instrument for its note.
            taps.assert_eq(&reference, &format!("{what}: against today's bank"));
            let spy = spy.unwrap();
            assert_eq!(spy.inner.instrument(), Some(to));
            // The lead rings out after the pick: not cut, and gone where an unpicked note still sounds.
            let ring = spy.energy(sounding, sounding + 480);
            assert!(ring > 1e-3, "{what}: the lead rings out after the pick ({ring})");
            let late = (S0 + PICK + 8_000, S0 + PICK + 9_000);
            let after = spy.energy(late.0, late.1);
            let r = (late.0 - S0) as usize..(late.1 - S0) as usize;
            let unpicked = energy(&held_on.bus[0][r.clone()]) + energy(&held_on.bus[1][r]);
            // The lead's own tail still sounds faintly there (measured: about 2.6e-3 of the held note's
            // energy for the bass pick at block 1); a note the pick did not release keeps all of it.
            assert!(unpicked > 1.0 && after < 1e-2 * unpicked, "{what}: the held note is released by the pick ({after} against {unpicked} unpicked)");
            // Nothing reached the unit for the release: the pick did it.
            let keys: Vec<_> = spy.keys().into_iter().filter(|&(f, ..)| f < PICK_END).collect();
            assert_eq!(keys, vec![(S0 + HOLD, true, 60), (S0 + PICK + 9_000, true, 48)], "{what}: the unit's notes");
        }
    }
    // A pick and a NoteOn on one frame: the pick first, in both ring orders (today's bank, sent pick
    // then note, is the reference).
    let reference = {
        let mut rig = rig(128);
        rig.input_latency = 256;
        let mut s = pick_script(true, Some(Instrument::Bass), Some(false), 0);
        s.sort_by_key(|&(f, _)| f);
        play(&mut rig, &s, PICK_END);
        Taps::of(&rig)
    };
    for block in BLOCKS {
        for note_first in [true, false] {
            let (taps, spy) = pick_render(block, false, Some(Instrument::Bass), Some(note_first));
            taps.assert_eq(&reference, &format!("block {block}, NoteOn sent {} the pick", if note_first { "before" } else { "after" }));
            assert!(spy.unwrap().keys().contains(&(S0 + PICK, true, 52)));
        }
    }
}

#[test]
fn a_pick_allocates_nothing_and_refuses_what_is_out_of_range() {
    let mut u = picked(Instrument::Lead);
    let mut out = [vec![0.0f32; 1024], vec![0.0f32; 1024]];
    let zeros = vec![0.0f32; 1024];
    let on = [SlotEvent { offset: 3, kind: SlotEventKind::NoteOn { key: 60, velocity: 1.0 } }, SlotEvent { offset: 5, kind: SlotEventKind::PitchBend { semitones: 0.5 } }];
    let [l, r] = &mut out;
    assert_no_alloc(|| u.process(&cx_at(0), [&zeros, &zeros], &on, [&mut l[..], &mut r[..]]));
    for (value, expect) in [(1.0, Some(1.0)), (1.4, Some(1.0)), (4.6, Some(5.0)), (5.0, Some(5.0)), (5.6, None), (-0.6, None), (6.0, None), (f64::NAN, None), (f64::INFINITY, None), (0.0, Some(0.0)), (0.0, Some(0.0))] {
        let before = u.instrument();
        let got = assert_no_alloc(|| u.set_param(&cx_at(1024), UnitParam::Instrument, value));
        assert_eq!(got, expect, "instrument {value}");
        // The index is the UI's `SYNTHS` order: 1 is the bass, 5 the drum kit (the enum's 1 is the pad).
        let want = match expect {
            Some(k) => Instrument::from_id(SYNTH_IDS[k as usize]),
            None => before,
        };
        assert_eq!(u.instrument(), want, "instrument {value}");
        let [l, r] = &mut out;
        assert_no_alloc(|| u.process(&cx_at(1024), [&zeros, &zeros], &[], [&mut l[..], &mut r[..]]));
    }
    assert_eq!(Instrument::from_id(SYNTH_IDS[1]), Some(Instrument::Bass));
    assert_eq!(Instrument::from_id(SYNTH_IDS[5]), Some(Instrument::Drums));
    for (value, expect) in [(0.5, Some(0.5)), (2.0, Some(1.5)), (-1.0, Some(0.0)), (f64::NAN, None)] {
        assert_eq!(assert_no_alloc(|| u.set_param(&cx_at(2048), UnitParam::Gain(Instrument::Pad), value)), expect, "gain {value}");
    }
    assert_eq!(u.set_param(&cx_at(2048), UnitParam::Send(lf_engine::InputSendParam::EchoLevel), 0.5), None, "an echo param is no SYNTH param");
    assert_eq!(u.info(), UnitInfo { kind: UnitKind::Synth, source: true, latency: LEAD });
    assert_eq!(synth().instrument(), None, "a new SYNTH starts with nothing picked");
}

// ── Source changes ──────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
enum Change {
    /// SYNTH → SYNTH (a pad), one crossfade.
    ToSynth,
    /// SYNTH → a fake source.
    ToFake,
    /// The SYNTH withdrawn, then reseated under its id.
    Withdraw,
    /// The SYNTH removed.
    Remove,
}

/// The frame the change is sent for (a block start at every size: the rig renders up to it first).
const CHANGE: Frame = S0 + 24_000;
const AFTER: Frame = CHANGE + 6_000;

/// Slot 0's SYNTH (lead) holds 60 (released after the change: a stray NoteOff there) and 64 (never
/// released: sustained); the change applies at `CHANGE`; the new source's own note 67 comes at `AFTER`.
/// `note_off_first`: a NoteOff 60 for the change's own block, sent before the change's message (`Some
/// (true)`) or after it (`Some(false)`); `None`: it comes 1 000 frames later.
fn source_change(block: usize, change: Change, note_off_first: Option<bool>) -> (Rig, UnitId, UnitId, Vec<(UnitId, Box<dyn UnitProcessor>, Left)>) {
    let mut rig = rig(block);
    let old = rig.install_id(0, Box::new(Spy::new(picked(Instrument::Lead))));
    let script = [(0, Command::SelectInstrument(NoteTarget::Slot(0))), (1_000, Command::NoteOn(60, 0.9)), (1_500, Command::NoteOn(64, 0.6))];
    play(&mut rig, &script, CHANGE);
    if note_off_first == Some(true) {
        rig.queue(Command::NoteOff(60));
    }
    let new = 900;
    match change {
        Change::ToSynth => {
            let unit: Box<dyn UnitProcessor> = Box::new(Spy::new(picked(Instrument::Pad)));
            assert!(rig.port(0).install_at(new, 7, 0, false, Some(old), unit).is_ok());
        }
        Change::ToFake => {
            let unit: Box<dyn UnitProcessor> = Box::new(Spy::new(FakeSource { held: [0; 2] }));
            assert!(rig.port(0).install_at(new, 7, 0, false, Some(old), unit).is_ok());
        }
        Change::Withdraw => assert!(rig.port(0).withdraw(old)),
        Change::Remove => rig.remove_id(0, old),
    }
    if note_off_first == Some(false) {
        rig.queue(Command::NoteOff(60));
    }
    let mut later = vec![];
    if note_off_first.is_none() {
        later.push((CHANGE + 1_000 - S0, Command::NoteOff(60)));
    }
    later.push((AFTER - S0, Command::NoteOn(67, 0.8)));
    let mut back = Vec::new();
    // The withdrawn unit comes back first; it is reseated under its id before the next note.
    if change == Change::Withdraw {
        play(&mut rig, &[], CHANGE + 2_000);
        while let Some(r) = rig.port(0).returned_unit() {
            back.push((r.id, r.unit, r.left));
        }
        let (_, unit, left) = back.pop().expect("the withdrawn SYNTH back");
        assert_eq!(left, Left::Withdrawn);
        assert!(rig.port(0).install_at(old, 8, 0, false, None, unit).is_ok());
        play(&mut rig, &[], CHANGE + 4_000);
    }
    let start = rig.frame;
    let later: Vec<(Frame, Command)> = later.into_iter().filter(|&(f, _)| S0 + f >= start).collect();
    play(&mut rig, &later, AFTER + 6_000);
    while let Some(r) = rig.port(0).returned_unit() {
        back.push((r.id, r.unit, r.left));
    }
    let seated: Vec<UnitId> = match change {
        Change::ToSynth | Change::ToFake => vec![new],
        Change::Withdraw => vec![old],
        Change::Remove => vec![],
    };
    back.extend(take_back(&mut rig, 0, &seated));
    (rig, old, new, back)
}

fn unit_back(back: &mut Vec<(UnitId, Box<dyn UnitProcessor>, Left)>, id: UnitId) -> (Box<dyn UnitProcessor>, Left) {
    let k = back.iter().position(|(i, ..)| *i == id).unwrap_or_else(|| panic!("unit {id} back"));
    let (_, unit, left) = back.remove(k);
    (unit, left)
}

#[test]
fn a_source_change_releases_on_its_frame() {
    for change in [Change::ToSynth, Change::ToFake, Change::Withdraw, Change::Remove] {
        let mut first: Option<Vec<f32>> = None;
        for block in BLOCKS {
            for note_off_first in [None, Some(true), Some(false)] {
                if note_off_first.is_some() && matches!(change, Change::Withdraw | Change::Remove) && block != 128 {
                    continue;
                }
                let what = format!("{change:?}, block {block}, NoteOff 60 {note_off_first:?}");
                let (rig, old, new, mut back) = source_change(block, change, note_off_first);
                let op = match change {
                    Change::ToSynth | Change::ToFake => SeatOp::Install,
                    Change::Withdraw => SeatOp::Withdraw,
                    Change::Remove => SeatOp::Remove,
                };
                let at = applied(&rig, if op == SeatOp::Install { new } else { old }, op);
                assert_eq!(at, CHANGE, "{what}: the change applies on the frame it was sent for");
                let (old_unit, left) = unit_back(&mut back, old);
                let old_spy = spy_of::<SynthUnit>(old_unit);
                // The old source gets its notes, then on the applied frame the releases of both, the
                // sustained one included, and nothing after (a reseat after a withdrawal is new).
                let released = vec![(S0 + 1_000, true, 60), (S0 + 1_500, true, 64), (CHANGE, false, 60), (CHANGE, false, 64)];
                match change {
                    Change::Withdraw => {
                        assert_eq!(left, Left::Removed, "{what}");
                        // The same unit, reseated: it holds nothing, so the stray NoteOff never reaches it;
                        // its own note does.
                        let mut want = released.clone();
                        want.push((AFTER, true, 67));
                        let keys: Vec<_> = old_spy.keys().into_iter().filter(|&(f, ..)| f <= AFTER + 5_000).collect();
                        assert_eq!(keys, want, "{what}: the withdrawn and reseated SYNTH's notes");
                    }
                    _ => {
                        assert_eq!(left, if change == Change::Remove { Left::Removed } else { Left::Replaced }, "{what}");
                        assert_eq!(old_spy.keys(), released, "{what}: the old source's notes");
                    }
                }
                // Once per time it leaves: a withdrawn and reseated unit leaves again when the rig takes it back.
                assert_eq!(old_spy.stops, if change == Change::Withdraw { 2 } else { 1 }, "{what}: stopped once per leaving");
                // The released lead rings out from the frame its release sounds.
                assert!(old_spy.energy(CHANGE, CHANGE + LEAD) > 1e-3, "{what}: the old source sounds up to its release");
                match change {
                    Change::ToSynth => {
                        let (unit, _) = unit_back(&mut back, new);
                        let spy = spy_of::<SynthUnit>(unit);
                        // (Its own note's release comes with the rig taking it back at the end.)
                        assert_eq!(spy.keys()[..1], [(AFTER, true, 67)], "{what}: the new SYNTH gets its own note alone");
                        assert!(spy.keys()[1..].iter().all(|&(f, on, k)| f > AFTER + 5_000 && !on && k == 67), "{what}: the new SYNTH's later notes");
                        assert!(spy.silent(CHANGE, AFTER + LEAD), "{what}: the new SYNTH is silent until its note");
                        assert!(spy.energy(AFTER + LEAD, AFTER + 4_000) > 1e-3, "{what}: the new SYNTH plays its note");
                    }
                    Change::ToFake => {
                        let (unit, _) = unit_back(&mut back, new);
                        let spy = spy_of::<FakeSource>(unit);
                        assert_eq!(spy.keys()[..1], [(AFTER, true, 67)], "{what}: the fake source gets its own note alone");
                        assert!(spy.keys()[1..].iter().all(|&(f, on, k)| f > AFTER + 5_000 && !on && k == 67), "{what}: the fake source's later notes");
                        assert!(spy.silent(CHANGE, AFTER), "{what}: the fake source is silent until its note");
                        assert!(!spy.silent(AFTER, AFTER + 1), "{what}: the fake source plays its note");
                    }
                    // A reseated SYNTH is the same bank: the tail of what its withdrawal released plays on
                    // from where it stopped (no reset while in one engine, contract §3), so no silence here.
                    Change::Withdraw => {}
                    Change::Remove => {}
                }
                assert!(back.is_empty(), "{what}: no other unit came back");
                if note_off_first.is_none() {
                    let bus: Vec<f32> = rig.bus.iter().chain(&rig.bus_right).copied().collect();
                    match &first {
                        None => first = Some(bus),
                        Some(f) => assert_bits(&bus, f, &format!("{what}: the bus across block sizes")),
                    }
                }
            }
        }
    }
}

// ── The wheels and a chain without a SYNTH ──────────────────────────────────────────────────────────

/// Slot 0 holds a plugin source (a fake) and a plugin effect after it, the note target; notes, and
/// `wheels` or not, mid-block. Answers each unit's calls and events.
fn plugins_only(block: usize, wheels: bool) -> (Vec<(Frame, usize)>, Vec<(Frame, SlotEventKind)>, Vec<(Frame, usize)>, Vec<(Frame, SlotEventKind)>) {
    let mut rig = rig(block);
    let src = rig.install_id(0, Box::new(Spy::new(FakeSource { held: [0; 2] })));
    let fx = rig.install_id(0, Box::new(Spy::new(Pass)));
    let mut script = vec![(0, Command::SelectInstrument(NoteTarget::Slot(0))), (1_003, Command::NoteOn(60, 0.9)), (9_001, Command::NoteOff(60))];
    if wheels {
        for j in 0..20 {
            script.push((500 + j * 397, Command::PitchBend(j as f64 / 10.0)));
            script.push((611 + j * 389, Command::Modulation(j as f64 / 19.0)));
        }
    }
    script.sort_by_key(|&(f, _)| f);
    play(&mut rig, &script, S0 + 12_000);
    let mut back = take_back(&mut rig, 0, &[fx, src]);
    let s = spy_of::<FakeSource>(unit_back(&mut back, src).0);
    let f = spy_of::<Pass>(unit_back(&mut back, fx).0);
    (s.calls.clone(), s.events.clone(), f.calls.clone(), f.events.clone())
}

#[test]
fn the_wheels_never_reach_or_split_a_chain_without_a_synth() {
    for block in BLOCKS {
        let without = plugins_only(block, false);
        let with = plugins_only(block, true);
        assert_eq!(with, without, "block {block}: the wheels changed a plugins-only chain's calls or events");
        assert!(with.1.iter().all(|(_, k)| matches!(k, SlotEventKind::NoteOn { .. } | SlotEventKind::NoteOff { .. })));
    }
    // A SYNTH chain gets them on their frames: a plugin effect after it sees them too, and ignores them.
    let mut rig = rig(480);
    let synth_id = rig.install_id(0, Box::new(Spy::new(picked(Instrument::Lead))));
    let fx = rig.install_id(0, Box::new(Spy::new(Pass)));
    let script = [(0, Command::SelectInstrument(NoteTarget::Slot(0))), (1_003, Command::NoteOn(60, 0.9)), (2_011, Command::PitchBend(1.5)), (2_777, Command::Modulation(0.25))];
    play(&mut rig, &script, S0 + 4_000);
    let mut back = take_back(&mut rig, 0, &[fx, synth_id]);
    let want = vec![(S0 + 1_003, SlotEventKind::NoteOn { key: 60, velocity: 0.9 }), (S0 + 2_011, SlotEventKind::PitchBend { semitones: 1.5 }), (S0 + 2_777, SlotEventKind::Modulation { depth: 0.25 })];
    let s = spy_of::<SynthUnit>(unit_back(&mut back, synth_id).0);
    let f = spy_of::<Pass>(unit_back(&mut back, fx).0);
    assert_eq!(s.events[..3], want[..], "the SYNTH's events");
    assert_eq!(f.events[..3], want[..], "the effect after it");
}
