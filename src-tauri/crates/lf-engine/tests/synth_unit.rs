//! SYNTH as a chain source (`lf_engine::units::SynthUnit`, `docs/unit-contract.md` §4, §9 E3):
//! - *SYNTH alone is today's bank:* one note script (chords, overlaps, a retrigger, velocities, a pitch
//!   bend and a mod wheel sweep) through `NoteTarget::Builtin(i)` and through a slot holding a SYNTH
//!   picked to `i` (the slot the note target, the input OTT off): the master bus, the heard output and
//!   the record tap, bit for bit per channel, for each of the six, with the record line delayed (an
//!   input side over the lead) and clamped (one under it); and both against the pin of today's bank
//!   (`tests/fixtures/builtin-bank.json`, its hashes per OS: the section "The pin of today's bank").
//! - Two SYNTH slots are two banks: notes and wheels to one never reach or sound in the other.
//! - A pick mid-note releases the held note on the pick's frame (it rings out, then falls silent where
//!   an unpicked note sustains), hands the new instrument the wheels, allocates nothing, and does all of
//!   it for the instrument already picked too, as `Instruments::select` does; a pick, a NoteOn and a bend
//!   on one frame apply in ring order, each order against today's bank sent in the same order.
//! - A source change (SYNTH → SYNTH, SYNTH → a fake source, a withdrawal and its reseat, a removal)
//!   under a held note and a note whose NoteOff never comes: each released on the applied frame, the
//!   new source silent until its own NoteOn, a stray NoteOff dropped, in both orders of a replacement
//!   and a NoteOff sent for the same block.
//! - The wheels never reach a chain without a SYNTH, nor split its render; a withdrawing SYNTH and a
//!   SYNTH slot the note target has left take none and split nothing; in a SYNTH's chain they reach the
//!   SYNTH alone, the handover on a target switch too (a plugin effect after it sees none, its calls as
//!   with no wheels).
//! - A departing SYNTH (replaced, removed) leaves the tail a pick's release of the same notes leaves, bit
//!   for bit (a mono bass never falls back to an older key on its way out), also after its last NoteOff
//!   (a pick releases a bass's tail again); a withdrawn and reseated one is silent until its next note.
//! - The note target leaving a SYNTH slot and `AllNotesOff` release as today's bank does (one release of
//!   the bank's own, in ring order with a note on the frame, held keys or not: also after the last
//!   NoteOff, the tail still sounding; once when the target is selected again), where a plugin keeps a
//!   NoteOff per key.
//! - A SYNTH's levels glide as `SetInstrumentGain`'s, its take records what today's bank's does, and a
//!   pick hands over the wheels it holds (against the same wheels sent after the pick).
//! - The engine keeps the last wheels whatever the target: the target landing on a SYNTH slot from
//!   another hands them over on its frame (bent on B, centred on A, B plays centred when the target comes
//!   back; a NoteOn or a bend on the switch's frame in both orders, against today's bank switching), and
//!   so does a SYNTH seated in the target slot (installed, replacing, reseated after wheels moved while
//!   it was out), in a call of no frames ahead of the slot's events; a plugin source gets none.
//!
//! Every render runs through the local `assert_no_alloc` wrapper (the rig's own bookkeeping given room
//! for it), bit-identical across block sizes 1, 32, 64, 127, 128, 480 and 1024 where it says so.

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

/// A render or a direct unit call under `assert_no_alloc` that fails on an allocation: with
/// `warn_debug` the crate only counts violations, so the count must not move. Every render here goes
/// through it (`advance`), the rig's own bookkeeping inside it too, so `rig` gives that room first.
fn assert_no_alloc<T, F: FnOnce() -> T>(f: F) -> T {
    let before = common::violation_count();
    let out = ::assert_no_alloc::assert_no_alloc(f);
    assert_eq!(common::violation_count(), before, "a render or a direct unit call allocated");
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

/// The wheels a SYNTH is handed on frame `f` (the note target landing on its slot, or its seat in it)
/// while the engine's are at rest.
fn rest(f: Frame) -> [(Frame, SlotEventKind); 2] {
    [(f, SlotEventKind::PitchBend { semitones: 0.0 }), (f, SlotEventKind::Modulation { depth: 0.0 })]
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

/// Frames of kept output a rig has room for: more than any render here keeps.
const ROOM: usize = 8 * SR as usize;

/// A rig keeping its output, with room for all of it and for its events, so its bookkeeping never
/// allocates under the wrapper.
fn rig(block: usize) -> Rig {
    let mut rig = Rig::with(Opts { block, ..Opts::default() });
    rig.keep_output();
    if let Some((_, out)) = rig.output.as_mut() {
        out.reserve(ROOM);
    }
    for v in [
        &mut rig.output_right,
        &mut rig.heard,
        &mut rig.heard_right,
        &mut rig.bus,
        &mut rig.bus_right,
        &mut rig.monitor,
        &mut rig.monitor_right,
        &mut rig.record,
        &mut rig.record_right,
        &mut rig.share,
        &mut rig.share_right,
    ] {
        v.reserve(ROOM);
    }
    rig.events.reserve(1 << 16);
    rig
}

/// Render `frames` under the local wrapper.
fn advance(rig: &mut Rig, frames: Frame) {
    assert_no_alloc(|| rig.advance(frames));
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
        advance(rig, step);
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

// ── The pin of today's bank ─────────────────────────────────────────────────────────────────────────
//
// `tests/fixtures/builtin-bank.json` holds the sha256 of today's bank's phrase (`builtin_phrase`): the
// bus, the heard output and the record tap (each the left side's frames, then the right's), per
// instrument × input side × block size, keyed by the OS that rendered it, so SYNTH keeps a reference
// once `NoteTarget::Builtin` is gone (`docs/unit-contract.md` §4). Keyed by OS because the synths'
// oscillators call the platform libm (`exp2` for a detune, the FFT's twiddles: `dsp::oscillator`),
// whose last bit can differ between platforms (`dsp::fdlibm`; the Windows entry, generated on CI's
// windows-latest at 64a82f1, came out equal to Linux's in all 84 keys). Only an OS of `UNPINNED` (macOS:
// no entry generated there yet) may lack its entry, and checks no pin and says so; any other OS
// without one fails, so a lost entry never turns the pin off. A deliberate change to what the bank sounds regenerates it, on each OS whose entry it
// keeps (the generator replaces only its own OS's):
// `cargo test -p lf-engine --test synth_unit -- --ignored write_builtin_pin`.

fn pin_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/builtin-bank.json")
}

fn pin_key(i: Instrument, input_latency: Frame, block: usize) -> String {
    format!("{}/{input_latency}/{block}", i.id())
}

/// The three taps' hashes, as the pin stores them.
fn pin_hashes(t: &Taps) -> serde_json::Value {
    let h = |x: &[Vec<f32>; 2]| serde_json::Value::String(common::refs::sha256_f32(&[&x[0], &x[1]]));
    serde_json::json!({ "bus": h(&t.bus), "heard": h(&t.heard), "record": h(&t.record) })
}

/// The OSes the pin has no entry for by design (nobody has generated one there yet). Any other OS
/// without its entry fails: an entry lost from the fixture must never turn the pin off silently.
const UNPINNED: [&str; 1] = ["macos"];

/// This OS's pins: `None` only on an OS of [`UNPINNED`] whose entry is missing.
fn pins() -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(pin_path()).expect("tests/fixtures/builtin-bank.json");
    let all: serde_json::Value = serde_json::from_str(&text).expect("the pin is JSON");
    let os = std::env::consts::OS;
    let mine = all["pins"].get(os).cloned();
    assert!(
        mine.is_some() || UNPINNED.contains(&os),
        "tests/fixtures/builtin-bank.json holds no pin for {os}, an OS it is generated on (only {UNPINNED:?} have none by design): regenerate it with `cargo test -p lf-engine --test synth_unit -- --ignored write_builtin_pin`"
    );
    mine
}

fn assert_pinned(pins: &serde_json::Value, key: &str, t: &Taps, what: &str) {
    let want = pins.get(key).unwrap_or_else(|| panic!("{what}: the pin has no {key}"));
    assert_eq!(&pin_hashes(t), want, "{what}: against the pin of today's bank ({key})");
}

/// Regenerates this OS's entry of `tests/fixtures/builtin-bank.json` from today's bank, keeping the
/// other OSes' entries.
#[test]
#[ignore = "regenerates tests/fixtures/builtin-bank.json from today's bank, for this OS"]
fn write_builtin_pin() {
    let mut mine = serde_json::Map::new();
    for i in Instrument::ALL {
        for input_latency in [256, 64] {
            for block in BLOCKS {
                mine.insert(pin_key(i, input_latency, block), pin_hashes(&builtin_phrase(i, block, input_latency)));
            }
        }
    }
    let mut all: serde_json::Value = std::fs::read_to_string(pin_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| serde_json::json!({ "pins": {} }));
    all["note"] = serde_json::Value::String(
        "sha256 of today's NoteTarget::Builtin bank rendering tests/synth_unit.rs's phrase (bus, heard, record: left frames then right, f32 LE), per instrument/input side/block size, keyed by the OS that rendered it. Regenerate: cargo test -p lf-engine --test synth_unit -- --ignored write_builtin_pin".to_string(),
    );
    all["frames"] = serde_json::json!(PHRASE_END - S0);
    all["pins"][std::env::consts::OS] = serde_json::Value::Object(mine);
    let json = serde_json::to_string_pretty(&all).expect("serialize the pin");
    std::fs::write(pin_path(), format!("{json}\n")).expect("write the pin");
}

#[test]
fn synth_alone_is_todays_bank() {
    let pins = pins();
    if pins.is_none() {
        eprintln!("tests/fixtures/builtin-bank.json holds no pin for {} by design ({UNPINNED:?}): SYNTH is checked against today's bank alone here", std::env::consts::OS);
    }
    for i in Instrument::ALL {
        // 256: the record line delays the bank by 256 - LEAD; 64: under the lead, the line clamps to 0.
        for input_latency in [256, 64] {
            let reference = builtin_phrase(i, 128, input_latency);
            assert!(energy(&reference.bus[0]) > 1.0 && energy(&reference.record[1]) > 1.0, "{i:?}: the phrase sounds and records");
            if let Some(pins) = &pins {
                assert_pinned(pins, &pin_key(i, input_latency, 128), &reference, &format!("{i:?}, input side {input_latency}: today's bank"));
            }
            for block in BLOCKS {
                let what = format!("{i:?}, input side {input_latency}, block {block}");
                if block != 128 {
                    builtin_phrase(i, block, input_latency).assert_eq(&reference, &format!("{what}: today's bank across block sizes"));
                }
                let unit = unit_phrase(i, block, input_latency);
                // The pin first: it outlives today's bank.
                if let Some(pins) = &pins {
                    assert_pinned(pins, &pin_key(i, input_latency, block), &unit, &format!("{what}: SYNTH"));
                }
                unit.assert_eq(&reference, &format!("{what}: SYNTH against today's bank"));
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

    fn wheels(&self) -> Vec<(Frame, SlotEventKind)> {
        self.events.iter().copied().filter(|(_, k)| matches!(k, SlotEventKind::PitchBend { .. } | SlotEventKind::Modulation { .. })).collect()
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
        advance(rig, 1);
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
        // Slot 0's SYNTH hears the first phrase and the select's release (its bank's own, as a pick's),
        // never slot 1's notes or wheels.
        assert_eq!(a.keys(), vec![(S0 + 1_000, true, 60), (S0 + 1_000, true, 64)], "block {block}: slot 0's notes");
        assert!(a.events.contains(&(S0 + 20_000, SlotEventKind::AllNotesOff)), "block {block}: slot 0's release");
        // Each is handed the engine's wheels as the target lands on it: slot 1 gets the bend and mod
        // wheel moved on slot 0, then its own bend.
        let wheel = |f: Frame, semitones: Option<f64>, depth: f64| (S0 + f, semitones.map_or(SlotEventKind::Modulation { depth }, |semitones| SlotEventKind::PitchBend { semitones }));
        let mut want = rest(S0).to_vec();
        want.extend([wheel(3_011, Some(0.5), 0.0), wheel(4_007, None, 0.7)]);
        assert_eq!(a.wheels(), want, "block {block}: slot 0's wheels");
        assert_eq!(b.keys(), vec![(S0 + 21_000, true, 67), (S0 + 30_000, false, 67)], "block {block}: slot 1's notes");
        assert_eq!(b.wheels(), vec![wheel(20_000, Some(0.5), 0.0), wheel(20_000, None, 0.7), wheel(22_013, Some(-1.0), 0.0)], "block {block}: slot 1's wheels");
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
/// `PICK` (if any) a pick of `to`; then note 48 at `PICK + 9_000`. `same_frame`: a NoteOn 52 and a bend
/// on the pick's frame, sent before the pick (`true`) or after it (`false`).
fn pick_script(builtin: bool, to: Option<Instrument>, same_frame: Option<bool>, id: UnitId) -> Vec<(Frame, Command)> {
    let pick = |i: Instrument| if builtin { Command::SelectInstrument(NoteTarget::Builtin(i)) } else { Command::SetUnitParam(id, UnitParam::Instrument, ui_index(i)) };
    let mut s = vec![(0, pick(Instrument::Lead))];
    if !builtin {
        s.push((0, Command::SelectInstrument(NoteTarget::Slot(0))));
    }
    s.extend([(500, Command::PitchBend(1.0)), (700, Command::Modulation(0.6)), (HOLD, Command::NoteOn(60, 0.9))]);
    if let Some(to) = to {
        match same_frame {
            Some(true) => s.extend([(PICK, Command::NoteOn(52, 0.8)), (PICK, Command::PitchBend(-0.5)), (PICK, pick(to))]),
            Some(false) => s.extend([(PICK, pick(to)), (PICK, Command::NoteOn(52, 0.8)), (PICK, Command::PitchBend(-0.5))]),
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
    // A pick, a NoteOn and a bend on one frame apply in ring order, as `Instruments::select` among
    // `note_on`s does: sent before the pick, the note reaches the lead and the pick releases it (the
    // bend bends the lead's tail, then is handed over); sent after it, the bass plays them. Today's bank,
    // sent in the same order, is each order's reference, and the two orders sound different.
    let references = [true, false].map(|note_first| pick_render(128, true, Some(Instrument::Bass), Some(note_first)).0);
    assert!(references[0].bus[0] != references[1].bus[0], "the two orders sound the same: the test cannot tell them apart");
    for block in BLOCKS {
        for (note_first, reference) in [true, false].into_iter().zip(&references) {
            let (taps, spy) = pick_render(block, false, Some(Instrument::Bass), Some(note_first));
            taps.assert_eq(reference, &format!("block {block}, NoteOn and bend sent {} the pick", if note_first { "before" } else { "after" }));
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
    // The range is checked before rounding: only 0..=5 is a pick, rounded to the nearest; the fringe
    // just outside it (which rounds into range) is refused.
    let values = [
        (1.0, Some(1.0)),
        (1.4, Some(1.0)),
        (4.6, Some(5.0)),
        (5.0, Some(5.0)),
        (-0.25, None),
        (-0.0001, None),
        (5.0001, None),
        (5.25, None),
        (5.6, None),
        (-0.6, None),
        (6.0, None),
        (f64::NAN, None),
        (f64::INFINITY, None),
        (f64::NEG_INFINITY, None),
        (0.0, Some(0.0)),
        (0.0, Some(0.0)),
    ];
    for (value, expect) in values {
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

/// Slot 0's SYNTH (on `i`) holds 60 (released after the change: a stray NoteOff there) and 64 (never
/// released: sustained); the change applies at `CHANGE`; the new source's own note 67 comes at `AFTER`.
/// `note_off_first`: a NoteOff 60 for the change's own block, sent before the change's message (`Some
/// (true)`) or after it (`Some(false)`); `None`: it comes 1 000 frames later (while a withdrawn SYNTH
/// is out, before its reseat).
fn source_change(block: usize, i: Instrument, change: Change, note_off_first: Option<bool>) -> (Rig, UnitId, UnitId, Vec<(UnitId, Box<dyn UnitProcessor>, Left)>) {
    let mut rig = rig(block);
    let old = rig.install_id(0, Box::new(Spy::new(picked(i))));
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
    // The withdrawn unit comes back first (the stray NoteOff reaching its slot meanwhile); it is reseated
    // under its id before the next note.
    if change == Change::Withdraw {
        let out: Vec<(Frame, Command)> = later.iter().copied().filter(|&(f, _)| S0 + f < CHANGE + 2_000).collect();
        later.retain(|&(f, _)| S0 + f >= CHANGE + 2_000);
        play(&mut rig, &out, CHANGE + 2_000);
        while let Some(r) = rig.port(0).returned_unit() {
            back.push((r.id, r.unit, r.left));
        }
        let (_, unit, left) = back.pop().expect("the withdrawn SYNTH back");
        assert_eq!(left, Left::Withdrawn);
        assert!(rig.port(0).install_at(old, 8, 0, false, None, unit).is_ok());
        play(&mut rig, &[], CHANGE + 4_000);
    }
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
    for i in [Instrument::Lead, Instrument::Bass] {
        for change in [Change::ToSynth, Change::ToFake, Change::Withdraw, Change::Remove] {
            let mut first: Option<Vec<f32>> = None;
            for block in BLOCKS {
                for note_off_first in [None, Some(true), Some(false)] {
                    if note_off_first.is_some() && matches!(change, Change::Withdraw | Change::Remove) && block != 128 {
                        continue;
                    }
                    let what = format!("{i:?}, {change:?}, block {block}, NoteOff 60 {note_off_first:?}");
                    source_change_case(block, i, change, note_off_first, &what, &mut first);
                }
            }
        }
    }
}

fn source_change_case(block: usize, i: Instrument, change: Change, note_off_first: Option<bool>, what: &str, first: &mut Option<Vec<f32>>) {
    let (rig, old, new, mut back) = source_change(block, i, change, note_off_first);
    let op = match change {
        Change::ToSynth | Change::ToFake => SeatOp::Install,
        Change::Withdraw => SeatOp::Withdraw,
        Change::Remove => SeatOp::Remove,
    };
    let at = applied(&rig, if op == SeatOp::Install { new } else { old }, op);
    assert_eq!(at, CHANGE, "{what}: the change applies on the frame it was sent for");
    let (old_unit, left) = unit_back(&mut back, old);
    let old_spy = spy_of::<SynthUnit>(old_unit);
    // The old SYNTH gets its notes, then on the applied frame one release of both, the sustained one
    // included, as its bank's own (not a NoteOff per key), and nothing after; the stray NoteOff 60 never
    // reaches it, before or after a reseat (a reseat after a withdrawal holds nothing).
    // (First the wheels at rest, handed as the target lands on its slot.)
    let mut want = rest(S0).to_vec();
    want.extend([(S0 + 1_000, SlotEventKind::NoteOn { key: 60, velocity: 0.9 }), (S0 + 1_500, SlotEventKind::NoteOn { key: 64, velocity: 0.6 }), (CHANGE, SlotEventKind::AllNotesOff)]);
    match change {
        Change::Withdraw => {
            assert_eq!(left, Left::Removed, "{what}");
            // The same unit, reseated: the wheels handed on its reseat, its own note, then (the rig
            // taking it back) its release.
            want.extend(rest(CHANGE + 2_000));
            want.push((AFTER, SlotEventKind::NoteOn { key: 67, velocity: 0.8 }));
            assert_eq!(old_spy.events[..want.len()], want[..], "{what}: the withdrawn and reseated SYNTH's events");
            assert!(old_spy.events[want.len()..].iter().all(|&(f, k)| f > AFTER + 5_000 && k == SlotEventKind::AllNotesOff), "{what}: the reseated SYNTH's later events");
            // `stop` silenced it: from its reseat it is silent until its own note sounds, never the tail
            // its withdrawal released spliced in after the gap.
            assert!(old_spy.silent(CHANGE + 2_000, AFTER + LEAD), "{what}: the reseated SYNTH is silent until its note");
            assert!(old_spy.energy(AFTER + LEAD, AFTER + 4_000) > 1e-3, "{what}: the reseated SYNTH plays its note");
        }
        _ => {
            assert_eq!(left, if change == Change::Remove { Left::Removed } else { Left::Replaced }, "{what}");
            assert_eq!(old_spy.events, want, "{what}: the old source's events");
        }
    }
    // Once per time it leaves: a withdrawn and reseated unit leaves again when the rig takes it back.
    assert_eq!(old_spy.stops, if change == Change::Withdraw { 2 } else { 1 }, "{what}: stopped once per leaving");
    // The old source sounds up to its release, and its released tail through the fade after it (its
    // shape against a pick's release: `a_departing_synth_releases_as_a_pick_does`).
    assert!(old_spy.energy(CHANGE, CHANGE + LEAD) > 1e-3, "{what}: the old source sounds up to its release");
    assert!(old_spy.energy(CHANGE + LEAD, CHANGE + 480) > 1e-4, "{what}: the old source's released tail sounds");
    match change {
        Change::ToSynth => {
            let (unit, _) = unit_back(&mut back, new);
            let spy = spy_of::<SynthUnit>(unit);
            // (Its own note's release comes with the rig taking it back at the end.)
            let mut want = rest(CHANGE).to_vec();
            want.push((AFTER, SlotEventKind::NoteOn { key: 67, velocity: 0.8 }));
            assert_eq!(spy.events[..3], want[..], "{what}: the new SYNTH gets the wheels on its seat and its own note alone");
            assert!(spy.events[3..].iter().all(|&(f, k)| f > AFTER + 5_000 && k == SlotEventKind::AllNotesOff), "{what}: the new SYNTH's later events");
            assert!(spy.silent(CHANGE, AFTER + LEAD), "{what}: the new SYNTH is silent until its note");
            assert!(spy.energy(AFTER + LEAD, AFTER + 4_000) > 1e-3, "{what}: the new SYNTH plays its note");
        }
        Change::ToFake => {
            let (unit, _) = unit_back(&mut back, new);
            let spy = spy_of::<FakeSource>(unit);
            assert_eq!(spy.keys()[..1], [(AFTER, true, 67)], "{what}: the fake source gets its own note alone");
            assert!(spy.keys()[1..].iter().all(|&(f, on, k)| f > AFTER + 5_000 && !on && k == 67), "{what}: the fake source's later notes");
            assert!(spy.events.iter().all(|&(_, k)| k != SlotEventKind::AllNotesOff), "{what}: a plugin never gets AllNotesOff");
            assert!(spy.silent(CHANGE, AFTER), "{what}: the fake source is silent until its note");
            assert!(!spy.silent(AFTER, AFTER + 1), "{what}: the fake source plays its note");
        }
        Change::Withdraw | Change::Remove => {}
    }
    assert!(back.is_empty(), "{what}: no other unit came back");
    if note_off_first.is_none() {
        let bus: Vec<f32> = rig.bus.iter().chain(&rig.bus_right).copied().collect();
        match first {
            None => *first = Some(bus),
            Some(f) => assert_bits(&bus, f, &format!("{what}: the bus across block sizes")),
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
    // A SYNTH chain gets them on their frames, the SYNTH alone: a plugin effect after it sees none (D12).
    let mut rig = rig(480);
    let synth_id = rig.install_id(0, Box::new(Spy::new(picked(Instrument::Lead))));
    let fx = rig.install_id(0, Box::new(Spy::new(Pass)));
    let script = [(0, Command::SelectInstrument(NoteTarget::Slot(0))), (1_003, Command::NoteOn(60, 0.9)), (2_011, Command::PitchBend(1.5)), (2_777, Command::Modulation(0.25))];
    play(&mut rig, &script, S0 + 4_000);
    let mut back = take_back(&mut rig, 0, &[fx, synth_id]);
    let mut want = rest(S0).to_vec();
    want.extend([(S0 + 1_003, SlotEventKind::NoteOn { key: 60, velocity: 0.9 }), (S0 + 2_011, SlotEventKind::PitchBend { semitones: 1.5 }), (S0 + 2_777, SlotEventKind::Modulation { depth: 0.25 })]);
    let s = spy_of::<SynthUnit>(unit_back(&mut back, synth_id).0);
    let f = spy_of::<Pass>(unit_back(&mut back, fx).0);
    assert_eq!(s.events[..5], want[..], "the SYNTH's events");
    let notes: Vec<_> = want.iter().copied().filter(|(_, k)| matches!(k, SlotEventKind::NoteOn { .. })).collect();
    assert_eq!(f.events[..1], notes[..], "the effect after it: its note, no wheel");
    assert!(f.wheels().is_empty(), "a wheel reached the plugin effect after the SYNTH: {:?}", f.wheels());
}

/// Slot 0 holds a lead SYNTH and a plugin effect after it, slot 1 a plugin source. The note target
/// starts on slot 1 and lands on slot 0 at `LAND`, a note played on each; `wheels`: wheels moved while
/// slot 1 is the target and a bend on the switch's frame (sent after the switch, `wheel_last`, or
/// before it). Answers the effect's calls and events and the SYNTH's wheels.
#[allow(clippy::type_complexity)]
fn land_on_synth_chain(block: usize, wheels: bool, wheel_last: bool) -> (Vec<(Frame, usize)>, Vec<(Frame, SlotEventKind)>, Vec<(Frame, SlotEventKind)>) {
    const LAND: Frame = 10_000;
    let mut rig = rig(block);
    let synth_id = rig.install_id(0, Box::new(Spy::new(picked(Instrument::Lead))));
    let fx = rig.install_id(0, Box::new(Spy::new(Pass)));
    let src = rig.install_id(1, Box::new(Spy::new(FakeSource { held: [0; 2] })));
    let mut script = vec![(0, Command::SelectInstrument(NoteTarget::Slot(1))), (1_000, Command::NoteOn(60, 0.9)), (3_000, Command::NoteOff(60))];
    if wheels {
        for j in 0..10 {
            script.push((101 + j * 853, Command::PitchBend(j as f64 / 5.0)));
            script.push((503 + j * 911, Command::Modulation(j as f64 / 9.0)));
        }
    }
    script.sort_by_key(|&(f, _)| f);
    let select = (LAND, Command::SelectInstrument(NoteTarget::Slot(0)));
    match (wheels, wheel_last) {
        (false, _) => script.push(select),
        (true, true) => script.extend([select, (LAND, Command::PitchBend(-0.5))]),
        (true, false) => script.extend([(LAND, Command::PitchBend(-0.5)), select]),
    }
    script.push((LAND + 1_000, Command::NoteOn(67, 0.7)));
    play(&mut rig, &script, S0 + 16_000);
    let mut back = take_back(&mut rig, 0, &[fx, synth_id]);
    let f = spy_of::<Pass>(unit_back(&mut back, fx).0);
    let synth = spy_of::<SynthUnit>(unit_back(&mut back, synth_id).0);
    let _ = take_back(&mut rig, 1, &[src]);
    (f.calls.clone(), f.events.clone(), synth.wheels())
}

#[test]
fn the_wheels_handed_to_a_synth_never_reach_a_plugin_after_it() {
    for block in BLOCKS {
        let (calls, events, synth) = land_on_synth_chain(block, false, true);
        assert_eq!(synth, rest(S0 + 10_000).to_vec(), "block {block}, no wheels: the SYNTH's wheels");
        let (bend, modulation) = (|semitones: f64| SlotEventKind::PitchBend { semitones }, |depth: f64| SlotEventKind::Modulation { depth });
        let land = S0 + 10_000;
        for wheel_last in [true, false] {
            let order = if wheel_last { "after" } else { "before" };
            let (c, e, synth) = land_on_synth_chain(block, true, wheel_last);
            // The handover reached the SYNTH (the last bend, 9/5 or the switch's, and mod wheel, 1)...
            let want = if wheel_last { vec![(land, bend(1.8)), (land, modulation(1.0)), (land, bend(-0.5))] } else { vec![(land, bend(-0.5)), (land, modulation(1.0))] };
            assert_eq!(synth, want, "block {block}, the switch's bend sent {order}: the SYNTH's wheels");
            // ...and nothing of it the plugin effect after it: no wheel, the calls and events as with none.
            assert!(!e.iter().any(|(_, k)| matches!(k, SlotEventKind::PitchBend { .. } | SlotEventKind::Modulation { .. })), "block {block}, the switch's bend sent {order}: a wheel reached the plugin effect after the SYNTH");
            assert_eq!((&c, &e), (&calls, &events), "block {block}, the switch's bend sent {order}: wheels changed the effect's calls or events");
        }
        assert!(events.iter().any(|&(f, k)| f == land + 1_000 && k == SlotEventKind::NoteOn { key: 67, velocity: 0.7 }), "block {block}: the effect sees the SYNTH chain's note");
    }
}

// ── A withdrawing SYNTH and a note-target switch: the wheels ────────────────────────────────────────

/// Slot 0 holds a lead SYNTH and a plugin effect after it, the note target, note 60 held; the SYNTH is
/// withdrawn at `CHANGE`; `wheels`: bends and mod wheel moves inside its fade and after it. Answers the
/// effect's calls and events and the SYNTH's events.
fn withdraw_wheels(block: usize, wheels: bool) -> (Vec<(Frame, usize)>, Vec<(Frame, SlotEventKind)>, Vec<(Frame, SlotEventKind)>) {
    let mut rig = rig(block);
    let synth_id = rig.install_id(0, Box::new(Spy::new(picked(Instrument::Lead))));
    let fx = rig.install_id(0, Box::new(Spy::new(Pass)));
    play(&mut rig, &[(0, Command::SelectInstrument(NoteTarget::Slot(0))), (1_000, Command::NoteOn(60, 0.9))], CHANGE);
    assert!(rig.port(0).withdraw(synth_id));
    let c = CHANGE - S0;
    let mut script = vec![(c + 2_000, Command::NoteOff(60))];
    if wheels {
        for j in 0..12 {
            script.push((c + 7 + j * 37, Command::PitchBend(j as f64 / 4.0)));
            script.push((c + 600 + j * 211, Command::Modulation(j as f64 / 11.0)));
        }
    }
    script.sort_by_key(|&(f, _)| f);
    play(&mut rig, &script, CHANGE + 4_000);
    assert_eq!(applied(&rig, synth_id, SeatOp::Withdraw), CHANGE);
    let mut back = take_back(&mut rig, 0, &[fx, synth_id]);
    let f = spy_of::<Pass>(unit_back(&mut back, fx).0);
    let s = spy_of::<SynthUnit>(unit_back(&mut back, synth_id).0);
    (f.calls.clone(), f.events.clone(), s.events.clone())
}

/// Slot 0 holds a lead SYNTH, slot 1 a plugin source and a plugin effect after it. The note target
/// starts on slot 0, moves to slot 1 at `AWAY` and back at `BACK`, a note played on each; `wheels`:
/// a bend on each switch's frame (sent after both switches, `wheel_last`, or before both) and wheels
/// while slot 1 is the target. Answers slot 1's two units' calls and events, then the SYNTH's wheels.
#[allow(clippy::type_complexity)]
fn switch_wheels(block: usize, wheels: bool, wheel_last: bool) -> ([Vec<(Frame, usize)>; 2], [Vec<(Frame, SlotEventKind)>; 2], Vec<(Frame, SlotEventKind)>) {
    const AWAY: Frame = 10_000;
    const BACK: Frame = 20_000;
    let mut rig = rig(block);
    let synth_id = rig.install_id(0, Box::new(Spy::new(picked(Instrument::Lead))));
    let src = rig.install_id(1, Box::new(Spy::new(FakeSource { held: [0; 2] })));
    let fx = rig.install_id(1, Box::new(Spy::new(Pass)));
    let mut script = vec![(0, Command::SelectInstrument(NoteTarget::Slot(0))), (1_000, Command::NoteOn(60, 0.9))];
    let switch = |at: Frame, to: u8, bend: f64| {
        let (select, wheel) = ((at, Command::SelectInstrument(NoteTarget::Slot(to))), (at, Command::PitchBend(bend)));
        match (wheels, wheel_last) {
            (false, _) => vec![select],
            (true, true) => vec![select, wheel],
            (true, false) => vec![wheel, select],
        }
    };
    script.extend(switch(AWAY, 1, 0.5));
    script.extend([(12_000, Command::NoteOn(64, 0.8)), (15_000, Command::NoteOff(64))]);
    if wheels {
        for j in 0..10 {
            script.push((AWAY + 101 + j * 853, Command::PitchBend(j as f64 / 5.0)));
            script.push((AWAY + 503 + j * 911, Command::Modulation(j as f64 / 9.0)));
        }
    }
    script.sort_by_key(|&(f, _)| f);
    script.extend(switch(BACK, 0, -0.5));
    script.push((BACK + 1_000, Command::NoteOn(67, 0.7)));
    play(&mut rig, &script, S0 + 30_000);
    let mut back = take_back(&mut rig, 1, &[fx, src]);
    let f = spy_of::<Pass>(unit_back(&mut back, fx).0);
    let s = spy_of::<FakeSource>(unit_back(&mut back, src).0);
    let mut back = take_back(&mut rig, 0, &[synth_id]);
    let synth = spy_of::<SynthUnit>(unit_back(&mut back, synth_id).0);
    let synth_wheels = synth.events.iter().copied().filter(|(_, k)| matches!(k, SlotEventKind::PitchBend { .. } | SlotEventKind::Modulation { .. })).collect();
    ([s.calls.clone(), f.calls.clone()], [s.events.clone(), f.events.clone()], synth_wheels)
}

#[test]
fn a_synth_on_its_way_out_or_off_target_takes_no_wheels() {
    for block in BLOCKS {
        // A withdrawing SYNTH no longer takes the wheels nor splits its chain for them: the effect after
        // it renders as without them, and the SYNTH gets only its notes' release.
        let (calls, events, synth) = withdraw_wheels(block, true);
        let (still_calls, still_events, _) = withdraw_wheels(block, false);
        assert_eq!((&calls, &events), (&still_calls, &still_events), "block {block}: wheels during a withdrawal reached or split the effect");
        let mut want = rest(S0).to_vec();
        want.extend([(S0 + 1_000, SlotEventKind::NoteOn { key: 60, velocity: 0.9 }), (CHANGE, SlotEventKind::AllNotesOff)]);
        assert_eq!(synth, want, "block {block}: the withdrawing SYNTH's events");
        // A note target on a plugin slot: wheels there reach and split nothing, whether a switch's frame
        // carries one before or after the switch; the SYNTH takes the one sent while it is the target,
        // and on the switch back it is handed the last wheels (the plugin slot's last bend, 9/5, and mod
        // wheel, 1), in ring order with the bend on that frame.
        let (calls, events, synth) = switch_wheels(block, false, true);
        let mut want = rest(S0).to_vec();
        want.extend(rest(S0 + 20_000));
        assert_eq!(synth, want, "block {block}, no wheels: the SYNTH's wheels");
        let (bend, modulation) = (|semitones: f64| SlotEventKind::PitchBend { semitones }, |depth: f64| SlotEventKind::Modulation { depth });
        for wheel_last in [true, false] {
            let (c, e, synth) = switch_wheels(block, true, wheel_last);
            assert_eq!((&c, &e), (&calls, &events), "block {block}, the switch's bend sent {}: wheels changed the plugin slot's calls or events", if wheel_last { "after" } else { "before" });
            let mut want = rest(S0).to_vec();
            let back = S0 + 20_000;
            if wheel_last {
                want.extend([(back, bend(1.8)), (back, modulation(1.0)), (back, bend(-0.5))]);
            } else {
                want.extend([(S0 + 10_000, bend(0.5)), (back, bend(-0.5)), (back, modulation(1.0))]);
            }
            assert_eq!(synth, want, "block {block}, the switch's bend sent {}: the SYNTH's wheels", if wheel_last { "after" } else { "before" });
        }
        assert!(events[0].iter().any(|&(f, k)| f == S0 + 12_000 && k == SlotEventKind::NoteOn { key: 64, velocity: 0.8 }), "block {block}: the plugin slot plays its note");
    }
}

// ── A departing SYNTH releases as a pick does ───────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
enum Depart {
    /// The reference: the SYNTH stays and picks its own instrument again (a pick releases every note).
    Pick,
    /// Replaced by another SYNTH.
    Replace,
    Remove,
}

/// Slot 0's SYNTH on `i` holds 67, then 60 over it (a mono bass sounds 60), neither ever released by a
/// NoteOff (or, `released`, both released `RELEASED` frames before, their tail still sounding); at
/// `CHANGE` it departs `how`. Answers the SYNTH.
fn departure(block: usize, i: Instrument, how: Depart, released: bool) -> Box<Spy<SynthUnit>> {
    let mut rig = rig(block);
    let old = rig.install_id(0, Box::new(Spy::new(picked(i))));
    let mut s = vec![(0, Command::SelectInstrument(NoteTarget::Slot(0))), (1_000, Command::NoteOn(67, 0.9)), (3_000, Command::NoteOn(60, 0.8))];
    if released {
        s.extend([(CHANGE - S0 - RELEASED - 200, Command::NoteOff(67)), (CHANGE - S0 - RELEASED, Command::NoteOff(60))]);
    }
    play(&mut rig, &s, CHANGE);
    let new = 900;
    match how {
        Depart::Pick => rig.send_at(CHANGE, Command::SetUnitParam(old, UnitParam::Instrument, ui_index(i))),
        Depart::Replace => assert!(rig.port(0).install_at(new, 7, 0, false, Some(old), Box::new(picked(Instrument::Pad))).is_ok()),
        Depart::Remove => rig.remove_id(0, old),
    }
    play(&mut rig, &[], CHANGE + 2_000);
    let mut back = Vec::new();
    while let Some(r) = rig.port(0).returned_unit() {
        back.push((r.id, r.unit, r.left));
    }
    let ids: &[UnitId] = match how {
        Depart::Pick => &[old],
        Depart::Replace => &[new],
        Depart::Remove => &[],
    };
    back.extend(take_back(&mut rig, 0, ids));
    spy_of::<SynthUnit>(unit_back(&mut back, old).0)
}

#[test]
fn a_departing_synth_releases_as_a_pick_does() {
    // Its fade: every frame the departing unit is still called for.
    let end = CHANGE + 480;
    let r = 0..(end - S0) as usize;
    // `released`: the last NoteOff came before the departure, the tail still sounding (a pick releases
    // a bass's tail again, held keys or not).
    for (i, released) in [(Instrument::Bass, false), (Instrument::Lead, false), (Instrument::Bass, true), (Instrument::Lead, true)] {
        let pick = departure(128, i, Depart::Pick, released);
        assert!(pick.energy(CHANGE + LEAD, end) > 1e-4, "{i:?}: the released tail sounds");
        for block in BLOCKS {
            for how in [Depart::Replace, Depart::Remove] {
                let what = format!("{i:?}, {how:?}, released {released}, block {block}");
                let gone = departure(block, i, how, released);
                // The tail of the notes it held, bit for bit a pick's release of them: a mono bass
                // released at the pitch it sounds, never falling back to 67 on its way out.
                for c in 0..2 {
                    assert_bits(&gone.out[c][r.clone()], &pick.out[c][r.clone()], &format!("{what}: the departing tail against a pick's release, side {c}"));
                }
            }
        }
    }
}

// ── The note target leaving a SYNTH, and AllNotesOff ────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
enum Leave {
    /// `SelectInstrument(Off)`.
    Off,
    /// `SelectInstrument` to the other (empty) slot.
    OtherSlot,
    /// `Command::AllNotesOff`, the target staying.
    AllNotesOff,
    /// `SelectInstrument` to the same slot (today's bank: the same instrument), the target staying: one
    /// release, as a pick of the same instrument.
    Again,
}

const LEAVE: Frame = 30_037;
/// How long before the leave (or the departure) a `released` script's last NoteOff comes: its tail
/// still sounds there.
const RELEASED: Frame = 700;

/// Slot 0's SYNTH (or today's bank, `builtin`) on `i`, a bend held, 67 then 60 over it held from
/// `HOLD` and never released by a NoteOff; at `LEAVE` the note target leaves it `how` (today's bank:
/// `SelectInstrument(Off)` for both selects), a NoteOn 64 on that frame sent before (`note_first`) or
/// after it; then note 48 at `LEAVE + 9_000`. `released`: 67 and then 60 get their NoteOffs ahead of
/// the leave, the last `RELEASED` frames before it (the tail still sounding, no key held). `fx`: a
/// plugin effect after the SYNTH.
#[allow(clippy::too_many_arguments)]
fn leave_render(block: usize, builtin: bool, i: Instrument, how: Leave, note_first: bool, released: bool, fx: bool) -> (Taps, Option<Box<Spy<SynthUnit>>>, Option<Box<Spy<Pass>>>) {
    let mut rig = rig(block);
    rig.input_latency = 256;
    let id = if builtin { 0 } else { rig.install_id(0, Box::new(Spy::new(synth()))) };
    let fx_id = (fx && !builtin).then(|| rig.install_id(0, Box::new(Spy::new(Pass))));
    let mut s = if builtin {
        vec![(0, Command::SelectInstrument(NoteTarget::Builtin(i)))]
    } else {
        vec![(0, Command::SetUnitParam(id, UnitParam::Instrument, ui_index(i))), (0, Command::SelectInstrument(NoteTarget::Slot(0)))]
    };
    s.extend([(500, Command::PitchBend(0.5)), (HOLD, Command::NoteOn(67, 0.9)), (HOLD + 3_000, Command::NoteOn(60, 0.8))]);
    if released {
        s.extend([(LEAVE - RELEASED - 200, Command::NoteOff(67)), (LEAVE - RELEASED, Command::NoteOff(60))]);
    }
    let leave = match (how, builtin) {
        (Leave::AllNotesOff, _) => Command::AllNotesOff,
        (Leave::Again, true) => Command::SelectInstrument(NoteTarget::Builtin(i)),
        (Leave::Again, false) => Command::SelectInstrument(NoteTarget::Slot(0)),
        (_, true) | (Leave::Off, false) => Command::SelectInstrument(NoteTarget::Off),
        (Leave::OtherSlot, false) => Command::SelectInstrument(NoteTarget::Slot(1)),
    };
    let note = (LEAVE, Command::NoteOn(64, 0.7));
    s.extend(if note_first { [note, (LEAVE, leave)] } else { [(LEAVE, leave), note] });
    s.push((LEAVE + 9_000, Command::NoteOn(48, 0.7)));
    play(&mut rig, &s, PICK_END);
    let taps = Taps::of(&rig);
    if builtin {
        return (taps, None, None);
    }
    let ids: Vec<UnitId> = fx_id.into_iter().chain([id]).collect();
    let mut back = take_back(&mut rig, 0, &ids);
    let spy = spy_of::<SynthUnit>(unit_back(&mut back, id).0);
    let fx = fx_id.map(|f| spy_of::<Pass>(unit_back(&mut back, f).0));
    (taps, Some(spy), fx)
}

/// Slot 0 holds a plugin source and a plugin effect after it, the note target, 67 then 60 held; at
/// `LEAVE` the target leaves `how`, a NoteOn 64 sent on that frame before it (or after). Answers both
/// units' events.
fn leave_plugins(block: usize, how: Leave, note_first: bool) -> [Vec<(Frame, SlotEventKind)>; 2] {
    let mut rig = rig(block);
    let src = rig.install_id(0, Box::new(Spy::new(FakeSource { held: [0; 2] })));
    let fx = rig.install_id(0, Box::new(Spy::new(Pass)));
    let leave = match how {
        Leave::Off => Command::SelectInstrument(NoteTarget::Off),
        Leave::OtherSlot => Command::SelectInstrument(NoteTarget::Slot(1)),
        Leave::AllNotesOff => Command::AllNotesOff,
        Leave::Again => Command::SelectInstrument(NoteTarget::Slot(0)),
    };
    let note = (LEAVE, Command::NoteOn(64, 0.7));
    let mut s = vec![(0, Command::SelectInstrument(NoteTarget::Slot(0))), (HOLD, Command::NoteOn(67, 0.9)), (HOLD + 3_000, Command::NoteOn(60, 0.8))];
    s.extend(if note_first { [note, (LEAVE, leave)] } else { [(LEAVE, leave), note] });
    play(&mut rig, &s, S0 + LEAVE + 2_000);
    let mut back = take_back(&mut rig, 0, &[fx, src]);
    let s = spy_of::<FakeSource>(unit_back(&mut back, src).0);
    let f = spy_of::<Pass>(unit_back(&mut back, fx).0);
    [s.events.clone(), f.events.clone()]
}

#[test]
fn the_target_leaving_a_synth_releases_as_todays_bank_does() {
    let at = S0 + LEAVE;
    let on = |key: u8, velocity: f32| SlotEventKind::NoteOn { key, velocity };
    // `released`: the last NoteOff came before the leave, the tail still sounding: today's bank
    // releases again all the same (a bass's tail re-releases on it).
    for (i, released) in [(Instrument::Bass, false), (Instrument::Lead, false), (Instrument::Bass, true), (Instrument::Lead, true)] {
        for how in [Leave::Off, Leave::OtherSlot, Leave::AllNotesOff, Leave::Again] {
            for note_first in [true, false] {
                let what = format!("{i:?}, {how:?}, released {released}, NoteOn 64 sent {} it", if note_first { "before" } else { "after" });
                let reference = leave_render(128, true, i, how, note_first, released, false).0;
                for block in BLOCKS {
                    let (taps, spy, _) = leave_render(block, false, i, how, note_first, released, false);
                    // Today's bank, bit for bit: the release on the frame, at the sounding pitch.
                    taps.assert_eq(&reference, &format!("{what}, block {block}: against today's bank"));
                    // One release of the bank's own, in ring order with the note on its frame, and no
                    // NoteOff per key.
                    let spy = spy.unwrap();
                    let there: Vec<SlotEventKind> = spy.events.iter().filter(|&&(f, _)| f == at).map(|&(_, k)| k).collect();
                    let want = match (note_first, how) {
                        (true, _) => vec![on(64, 0.7), SlotEventKind::AllNotesOff],
                        (false, Leave::AllNotesOff | Leave::Again) => vec![SlotEventKind::AllNotesOff, on(64, 0.7)],
                        (false, _) => vec![SlotEventKind::AllNotesOff],
                    };
                    assert_eq!(there, want, "{what}, block {block}: the SYNTH's events on the frame");
                    assert!(spy.keys().iter().all(|&(f, on, _)| on || f >= PICK_END || (released && f < at)), "{what}, block {block}: a NoteOff reached the SYNTH");
                }
            }
        }
    }
    // A plugin keeps a NoteOff per held key, ascending: alone in a slot (whose queue is what it was) and
    // as an effect after a SYNTH, which takes the one release in their place.
    for block in [1, 480] {
        for how in [Leave::Off, Leave::OtherSlot, Leave::AllNotesOff, Leave::Again] {
            for note_first in [true, false] {
                let what = format!("{how:?}, NoteOn 64 sent {}, block {block}", if note_first { "before" } else { "after" });
                let mut released: Vec<(Frame, SlotEventKind)> = [60, 64, 67].into_iter().filter(|&k| k != 64 || note_first).map(|key| (at, SlotEventKind::NoteOff { key })).collect();
                if !note_first && matches!(how, Leave::AllNotesOff | Leave::Again) {
                    released.push((at, on(64, 0.7)));
                }
                for (u, events) in leave_plugins(block, how, note_first).iter().enumerate() {
                    let there: Vec<_> = events.iter().copied().filter(|&(f, _)| f == at).collect();
                    let mut want = released.clone();
                    if note_first {
                        want.insert(0, (at, on(64, 0.7)));
                    }
                    assert_eq!(there, want, "{what}: plugins-only slot, unit {u}");
                    assert!(events.iter().all(|&(_, k)| k != SlotEventKind::AllNotesOff), "{what}: AllNotesOff reached a plugin");
                }
                let (_, synth, fx) = leave_render(block, false, Instrument::Bass, how, note_first, false, true);
                let fx = fx.unwrap();
                let there: Vec<_> = fx.events.iter().copied().filter(|&(f, _)| f == at).collect();
                let mut want = released.clone();
                if note_first {
                    want.insert(0, (at, on(64, 0.7)));
                }
                assert_eq!(there, want, "{what}: the effect after a SYNTH");
                assert!(fx.events.iter().all(|&(_, k)| k != SlotEventKind::AllNotesOff), "{what}: AllNotesOff reached the effect after a SYNTH");
                assert!(synth.unwrap().events.iter().any(|&(f, k)| f == at && k == SlotEventKind::AllNotesOff), "{what}: the SYNTH before the effect");
            }
        }
    }
}

// ── A level change, and a take ──────────────────────────────────────────────────────────────────────

/// A lead chord whose level moves twice mid-note, a level set for the pad before it is picked, then
/// the pad played: through today's bank (`SetInstrumentGain`) or slot 0's SYNTH (`UnitParam::Gain`).
/// Every level is exact in f32, as the wire's f32 and the param's f64 then agree.
fn gain_render(block: usize, builtin: bool) -> Taps {
    let mut rig = rig(block);
    rig.input_latency = 256;
    let id = if builtin { 0 } else { rig.install_id(0, Box::new(synth())) };
    let pick = |i: Instrument| if builtin { Command::SelectInstrument(NoteTarget::Builtin(i)) } else { Command::SetUnitParam(id, UnitParam::Instrument, ui_index(i)) };
    let gain = |i: Instrument, g: f64| if builtin { Command::SetInstrumentGain(i, g as f32) } else { Command::SetUnitParam(id, UnitParam::Gain(i), g) };
    let mut s = vec![(0, pick(Instrument::Lead))];
    if !builtin {
        s.push((0, Command::SelectInstrument(NoteTarget::Slot(0))));
    }
    s.extend([
        (1_000, Command::NoteOn(60, 0.8)),
        (1_500, Command::NoteOn(64, 0.7)),
        (5_011, gain(Instrument::Lead, 0.375)),
        (9_100, gain(Instrument::Lead, 1.25)),
        (12_000, gain(Instrument::Pad, 0.5)),
        (14_000, Command::NoteOff(60)),
        (14_000, Command::NoteOff(64)),
        (16_000, pick(Instrument::Pad)),
        (16_500, Command::NoteOn(67, 0.9)),
        (20_333, gain(Instrument::Pad, 0.0)),
        (26_000, Command::NoteOff(67)),
    ]);
    play(&mut rig, &s, S0 + 30_000);
    Taps::of(&rig)
}

#[test]
fn a_synth_level_glides_as_todays_bank_does() {
    let reference = gain_render(128, true);
    let still = {
        let r = (S0 + 10_000 - S0) as usize..(S0 + 13_000 - S0) as usize;
        energy(&reference.bus[0][r])
    };
    assert!(still > 1.0, "the lead sounds at its raised level ({still})");
    for block in BLOCKS {
        gain_render(block, false).assert_eq(&reference, &format!("block {block}: SYNTH's levels against SetInstrumentGain"));
    }
}

/// The phrase on `i` recorded as a first take (its count-in first), through today's bank or slot 0's
/// SYNTH: the committed loop.
fn take_render(i: Instrument, builtin: bool, block: usize, input_latency: Frame) -> Vec<lf_engine::Pair> {
    const AT: Frame = 100_000;
    let mut rig = rig(block);
    rig.input_latency = input_latency;
    let id = if builtin { 0 } else { rig.install_id(0, Box::new(synth())) };
    let mut s = if builtin {
        vec![(0, Command::SelectInstrument(NoteTarget::Builtin(i)))]
    } else {
        vec![(0, Command::SetUnitParam(id, UnitParam::Instrument, ui_index(i))), (0, Command::SelectInstrument(NoteTarget::Slot(0)))]
    };
    s.push((0, Command::RecDub(0)));
    s.extend(phrase(i).into_iter().map(|(f, c)| (AT + f, c)));
    s.push((AT + 2 * 96_000, Command::RecDub(0)));
    play(&mut rig, &s, S0 + AT + 2 * 96_000 + 24_000);
    assert!(rig.master() > 0, "{i:?}: the take committed");
    rig.pcm2(0)
}

#[test]
fn a_take_records_what_todays_bank_records() {
    for i in Instrument::ALL {
        for input_latency in [256, 64] {
            let reference = take_render(i, true, 128, input_latency);
            let side = |c: usize| reference.iter().map(|p| p[c]).collect::<Vec<f32>>();
            assert!(energy(&side(0)) > 1.0 && energy(&side(1)) > 1.0, "{i:?}: the take holds the phrase");
            for block in [128, 127] {
                let got = take_render(i, false, block, input_latency);
                for c in 0..2 {
                    let mine: Vec<f32> = got.iter().map(|p| p[c]).collect();
                    assert_bits(&mine, &side(c), &format!("{i:?}, input side {input_latency}, block {block}: the SYNTH's take against today's bank's, side {c}"));
                }
            }
        }
    }
}

// ── The wheels a pick hands over, against an independent expectation ───────────────────────────────

/// Today's bank or slot 0's SYNTH on lead, a bend and a mod wheel set (`wheels`), then a pick of `to` at
/// `PICK` and a note on it. `resend`: the same bend and mod wheel sent again on the pick's frame, after
/// it, so the picked instrument gets them without the pick's own handover.
fn wheel_pick(block: usize, builtin: bool, to: Instrument, wheels: bool, resend: bool) -> Taps {
    let mut rig = rig(block);
    rig.input_latency = 256;
    let id = if builtin { 0 } else { rig.install_id(0, Box::new(synth())) };
    let pick = |i: Instrument| if builtin { Command::SelectInstrument(NoteTarget::Builtin(i)) } else { Command::SetUnitParam(id, UnitParam::Instrument, ui_index(i)) };
    let mut s = vec![(0, pick(Instrument::Lead))];
    if !builtin {
        s.push((0, Command::SelectInstrument(NoteTarget::Slot(0))));
    }
    if wheels {
        s.extend([(500, Command::PitchBend(1.0)), (700, Command::Modulation(0.6))]);
    }
    s.push((PICK, pick(to)));
    if resend {
        s.extend([(PICK, Command::PitchBend(1.0)), (PICK, Command::Modulation(0.6))]);
    }
    s.extend([(PICK + 1_000, Command::NoteOn(48, 0.8)), (PICK + 20_000, Command::NoteOff(48))]);
    play(&mut rig, &s, S0 + PICK + 30_000);
    Taps::of(&rig)
}

/// The handover is `Bank::select`'s, which today's bank and the SYNTH share, so a reference through
/// either moves with it: here a pick under held wheels must sound as the same pick with the same wheels
/// sent after it, and unlike the pick with the wheels at rest.
#[test]
fn a_pick_hands_over_the_wheels_it_holds() {
    for builtin in [true, false] {
        for to in [Instrument::Bass, Instrument::Pad] {
            let what = format!("{} picks {to:?}", if builtin { "today's bank" } else { "SYNTH" });
            let resent = wheel_pick(128, builtin, to, true, true);
            let at_rest = wheel_pick(128, builtin, to, false, false);
            assert!(resent.bus[0] != at_rest.bus[0], "{what}: the wheels are not heard on the picked instrument");
            for block in [1, 128, 480] {
                wheel_pick(block, builtin, to, true, false).assert_eq(&resent, &format!("{what}, block {block}: the pick hands over the wheels it holds"));
            }
        }
    }
}

// ── The wheels a target switch and a seat hand a SYNTH ──────────────────────────────────────────────

/// The note target's return to slot 1.
const BACK: Frame = 20_000;

/// Slot 0's SYNTH on lead (A) and slot 1's on pad (B), or today's bank switching between the two:
/// the target on B, a note bent and mod-wheeled there; then on A, the bend centred under a note; then
/// Off, the mod wheel moved; then back on B at `BACK` and a note on it. `same_frame`: a NoteOn 67 and a
/// bend on `BACK`, sent before the switch back (`Some(true)`) or after it (`Some(false)`). Answers the
/// taps and B's SYNTH.
fn switch_back(block: usize, builtin: bool, same_frame: Option<bool>) -> (Taps, Option<Box<Spy<SynthUnit>>>) {
    let mut rig = rig(block);
    rig.input_latency = 256;
    let ids = if builtin {
        [0, 0]
    } else {
        [rig.install_id(0, Box::new(Spy::new(picked(Instrument::Lead)))), rig.install_id(1, Box::new(Spy::new(picked(Instrument::Pad))))]
    };
    let to = |slot: u8| if builtin { Command::SelectInstrument(NoteTarget::Builtin([Instrument::Lead, Instrument::Pad][slot as usize])) } else { Command::SelectInstrument(NoteTarget::Slot(slot)) };
    let mut s = vec![
        (0, to(1)),
        (1_000, Command::NoteOn(60, 0.8)),
        (2_011, Command::PitchBend(1.0)),
        (2_500, Command::Modulation(0.8)),
        (8_000, Command::NoteOff(60)),
        (10_000, to(0)),
        (11_000, Command::NoteOn(64, 0.7)),
        (12_007, Command::PitchBend(0.0)),
        (13_000, Command::NoteOff(64)),
        (15_000, Command::SelectInstrument(NoteTarget::Off)),
        (16_003, Command::Modulation(0.3)),
    ];
    let extra = [(BACK, Command::NoteOn(67, 0.9)), (BACK, Command::PitchBend(-0.25))];
    match same_frame {
        Some(true) => s.extend(extra.into_iter().chain([(BACK, to(1))])),
        Some(false) => s.extend([(BACK, to(1))].into_iter().chain(extra)),
        None => s.push((BACK, to(1))),
    }
    s.extend([(BACK + 1_000, Command::NoteOn(72, 0.8)), (BACK + 15_000, Command::NoteOff(72)), (BACK + 16_000, Command::NoteOff(67))]);
    play(&mut rig, &s, S0 + BACK + 24_000);
    let taps = Taps::of(&rig);
    if builtin {
        return (taps, None);
    }
    take_back(&mut rig, 0, &[ids[0]]);
    let b = spy_of::<SynthUnit>(take_back(&mut rig, 1, &[ids[1]]).into_iter().next().expect("B's SYNTH back").1);
    (taps, Some(b))
}

#[test]
fn a_target_switch_hands_the_synth_the_last_wheels() {
    let at = S0 + BACK;
    for same_frame in [None, Some(true), Some(false)] {
        let what = format!("same frame {same_frame:?}");
        // Today's bank, one bank switching between the two: bent on the pad, centred on the lead, the mod
        // wheel moved with no target, the pad plays centred with that mod wheel when it is picked again.
        let reference = switch_back(128, true, same_frame).0;
        for block in BLOCKS {
            let (taps, b) = switch_back(block, false, same_frame);
            taps.assert_eq(&reference, &format!("{what}, block {block}: two SYNTH slots against today's bank"));
            // B gets the last wheels on the switch's frame, in ring order with what shares it: the
            // centred bend and the mod wheel moved while it was not the target (a bend sent before the
            // switch is the last, so it is the one handed; one sent after follows the handover).
            let there: Vec<SlotEventKind> = b.unwrap().events.iter().filter(|&&(f, _)| f == at).map(|&(_, k)| k).collect();
            let (bend, modulation) = (|semitones: f64| SlotEventKind::PitchBend { semitones }, SlotEventKind::Modulation { depth: 0.3 });
            let want = match same_frame {
                None => vec![bend(0.0), modulation],
                Some(true) => vec![bend(-0.25), modulation],
                Some(false) => vec![bend(0.0), modulation, SlotEventKind::NoteOn { key: 67, velocity: 0.9 }, bend(-0.25)],
            };
            assert_eq!(there, want, "{what}, block {block}: B's events on the switch back");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Seated {
    /// A pad SYNTH replaces the lead SYNTH in the target slot.
    Replace,
    /// A pad SYNTH is installed in the target slot, empty until then.
    Fresh,
    /// The pad SYNTH is withdrawn, the wheels move while it is out, and it is reseated.
    Reseat,
    /// A plugin source replaces the lead SYNTH: it gets no wheel.
    Plugin,
}

/// Frames after `CHANGE` the reseat comes (a block start at every size).
const RESEAT: Frame = 2_000;

/// Slot 0 the note target, the wheels set (bend 1, mod 0.6) while it holds the lead SYNTH (or nothing,
/// `Fresh`; or the pad SYNTH, `Reseat`), a note held; at `CHANGE` a source is seated `how` (`Reseat`:
/// withdrawn there, the wheels moved to -1 and 0.2 while it is out, reseated `RESEAT` later), note 67
/// at `AFTER`. `resend`: the wheels the engine holds sent again on the seat's frame, after it (the
/// reference), or (`Some(false)`) wheels at rest sent there instead. Answers the taps, the seated unit's
/// events and calls, and the seat's frame.
#[allow(clippy::type_complexity)]
fn seat_wheels(block: usize, how: Seated, resend: Option<bool>) -> (Taps, Vec<(Frame, SlotEventKind)>, Vec<(Frame, usize)>, Frame) {
    let mut rig = rig(block);
    rig.input_latency = 256;
    let first = match how {
        Seated::Replace | Seated::Plugin => Some(rig.install_id(0, Box::new(Spy::new(picked(Instrument::Lead))))),
        Seated::Fresh => None,
        Seated::Reseat => Some(rig.install_id(0, Box::new(Spy::new(picked(Instrument::Pad))))),
    };
    let script = [(0, Command::SelectInstrument(NoteTarget::Slot(0))), (500, Command::PitchBend(1.0)), (700, Command::Modulation(0.6)), (1_000, Command::NoteOn(60, 0.9))];
    play(&mut rig, &script, CHANGE);
    let new = 900;
    let mut wheels = (1.0, 0.6);
    let seat = match how {
        Seated::Replace => {
            assert!(rig.port(0).install_at(new, 7, 0, false, first, Box::new(Spy::new(picked(Instrument::Pad)))).is_ok());
            CHANGE
        }
        Seated::Plugin => {
            assert!(rig.port(0).install_at(new, 7, 0, false, first, Box::new(Spy::new(FakeSource { held: [0; 2] }))).is_ok());
            CHANGE
        }
        Seated::Fresh => {
            assert!(rig.port(0).install_at(new, 7, 0, false, None, Box::new(Spy::new(picked(Instrument::Pad)))).is_ok());
            CHANGE
        }
        Seated::Reseat => {
            let old = first.unwrap();
            assert!(rig.port(0).withdraw(old));
            wheels = (-1.0, 0.2);
            let c = CHANGE - S0;
            play(&mut rig, &[(c + 1_000, Command::PitchBend(wheels.0)), (c + 1_200, Command::Modulation(wheels.1))], CHANGE + RESEAT);
            let (_, unit) = rig.port(0).returned().expect("the withdrawn SYNTH back");
            assert!(rig.port(0).install_at(old, 8, 0, false, None, unit).is_ok());
            CHANGE + RESEAT
        }
    };
    match resend {
        Some(true) => {
            rig.send_at(seat, Command::PitchBend(wheels.0));
            rig.send_at(seat, Command::Modulation(wheels.1));
        }
        Some(false) => {
            rig.send_at(seat, Command::PitchBend(0.0));
            rig.send_at(seat, Command::Modulation(0.0));
        }
        None => {}
    }
    play(&mut rig, &[(AFTER - S0, Command::NoteOn(67, 0.8)), (AFTER + 8_000 - S0, Command::NoteOff(67))], AFTER + 12_000);
    let taps = Taps::of(&rig);
    let id = if how == Seated::Reseat { first.unwrap() } else { new };
    let mut back = take_back(&mut rig, 0, &[id]);
    let (unit, _) = unit_back(&mut back, id);
    let (events, calls) = if how == Seated::Plugin {
        let spy = spy_of::<FakeSource>(unit);
        (spy.events.clone(), spy.calls.clone())
    } else {
        let spy = spy_of::<SynthUnit>(unit);
        (spy.events.clone(), spy.calls.clone())
    };
    (taps, events, calls, seat)
}

#[test]
fn a_synth_seated_in_the_target_slot_gets_the_last_wheels() {
    for how in [Seated::Replace, Seated::Fresh, Seated::Reseat] {
        // The same wheels sent on the seat's frame after it: the handover must sound as that, and the
        // wheels must be heard (unlike wheels at rest sent there).
        let resent = seat_wheels(128, how, Some(true)).0;
        let at_rest = seat_wheels(128, how, Some(false)).0;
        assert!(resent.bus[0] != at_rest.bus[0], "{how:?}: the wheels are not heard on the seated SYNTH");
        for block in BLOCKS {
            let what = format!("{how:?}, block {block}");
            let (taps, events, calls, seat) = seat_wheels(block, how, None);
            taps.assert_eq(&resent, &format!("{what}: the seat hands over the wheels the engine holds"));
            let wheels = if how == Seated::Reseat { (-1.0, 0.2) } else { (1.0, 0.6) };
            // In a call of no frames on the seat's frame, ahead of everything else it gets there.
            let first: Vec<_> = events.iter().copied().skip_while(|&(f, _)| f < seat).take(2).collect();
            assert_eq!(first, vec![(seat, SlotEventKind::PitchBend { semitones: wheels.0 }), (seat, SlotEventKind::Modulation { depth: wheels.1 })], "{what}: the seated SYNTH's first events");
            assert!(calls.contains(&(seat, 0)), "{what}: the handover's call of no frames");
            assert!(!events.iter().any(|&(f, k)| f > seat && f < AFTER + 20_000 && matches!(k, SlotEventKind::PitchBend { .. } | SlotEventKind::Modulation { .. })), "{what}: a wheel after the handover");
        }
    }
    // A plugin seated there gets no wheel and no call of no frames.
    for block in [1, 480] {
        let (_, events, calls, _) = seat_wheels(block, Seated::Plugin, None);
        assert!(events.iter().all(|&(_, k)| matches!(k, SlotEventKind::NoteOn { .. } | SlotEventKind::NoteOff { .. })), "block {block}: a wheel reached a plugin source");
        assert!(calls.iter().all(|&(_, n)| n > 0), "block {block}: a plugin source got a call of no frames");
    }
}
