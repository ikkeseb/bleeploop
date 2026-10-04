//! The plugin slot rack (`src/slots.rs`) driven through the engine: install and removal through the
//! ports with their bypass crossfades, the engine never dropping a unit, the notes a slot holds, one
//! plugin call per block, the live flag and gain, where each output goes and where it is recorded, and
//! the whole of it bit-identical at any block size. New with the engine: the web app's slots
//! (`plugin-bridge.ts`, `instrument-slots.ts`) had no rig guard to port.
//!
//! The fakes record what they saw in preallocated buffers and atomics, so every `process` still runs
//! under the rig's `assert_no_alloc`.
//!
//! § Continuity under a sustained tone holds GO LIVE, an empty slot's live toggle, and an install and a
//! removal to `tests/seam_continuity.rs`'s click criterion on what is heard.

mod common;

use std::f64::consts::PI;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

use common::{code, Delay, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Command, InputSend, InputSendParam, Instrument, LaneState, NoteTarget, ProcessContext, SlotEvent, SlotEventKind, SlotKind, SlotProcessor};

/// Frames of the bypass crossfade at 48 kHz: 10 ms.
const FADE: usize = 480;
/// Frames of the live gate's ramp at 48 kHz: 5 ms (STATUS D23).
const LIVE: Frame = 240;
/// What a fake logs before it stops logging (a long run at block size 1 would outgrow it).
const LOG: usize = 1 << 16;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Seen {
    Call { frame: Frame, len: usize },
    Event(SlotEvent),
    Stop,
}

/// The test's view of a fake while the engine holds it.
#[derive(Clone)]
struct Probe {
    log: Arc<Mutex<Vec<Seen>>>,
    drops: Arc<AtomicUsize>,
    /// The largest |input| the fake was given, as f32 bits.
    input_peak: Arc<AtomicU32>,
}

impl Probe {
    fn new() -> Probe {
        Probe { log: Arc::new(Mutex::new(Vec::with_capacity(LOG))), drops: Arc::new(AtomicUsize::new(0)), input_peak: Arc::new(AtomicU32::new(0)) }
    }

    fn seen(&self) -> Vec<Seen> {
        let log = self.log.lock().unwrap();
        assert!(log.len() < LOG, "the fake's log filled up");
        log.clone()
    }

    fn calls(&self) -> Vec<(Frame, usize)> {
        self.seen().into_iter().filter_map(|s| if let Seen::Call { frame, len } = s { Some((frame, len)) } else { None }).collect()
    }

    /// Every event: the frame it lands on, its offset into its call, and what it is.
    fn events(&self) -> Vec<(Frame, u32, SlotEventKind)> {
        let mut at: (Frame, usize) = (0, 0);
        let mut out = Vec::new();
        for s in self.seen() {
            match s {
                Seen::Call { frame, len } => at = (frame, len),
                Seen::Event(e) => {
                    assert!((e.offset as usize) < at.1, "an event's offset lies inside its call");
                    out.push((at.0 + e.offset as Frame, e.offset, e.kind));
                }
                Seen::Stop => {}
            }
        }
        out
    }

    fn keys(&self) -> Vec<(bool, u8)> {
        self.events()
            .into_iter()
            .map(|(_, _, k)| match k {
                SlotEventKind::NoteOn { key, .. } => (true, key),
                SlotEventKind::NoteOff { key } => (false, key),
            })
            .collect()
    }

    fn stops(&self) -> usize {
        self.seen().iter().filter(|s| **s == Seen::Stop).count()
    }

    fn input_peak(&self) -> f32 {
        f32::from_bits(self.input_peak.load(SeqCst))
    }
}

/// A unit stand-in. Its output is `level` plus `through` × its input; an instrument also plays a 1.0
/// impulse `latency` frames after each NoteOn.
struct Fake {
    id: u32,
    kind: SlotKind,
    latency: Frame,
    level: f32,
    through: f32,
    impulse: Option<Frame>,
    /// Added to the last frame of every call (a value gone bad late in an otherwise sound block).
    last: f32,
    probe: Probe,
}

impl Fake {
    fn effect(level: f32, through: f32, probe: &Probe) -> Box<Fake> {
        Box::new(Fake { id: 0, kind: SlotKind::Effect, latency: 0, level, through, impulse: None, last: 0.0, probe: probe.clone() })
    }

    fn instrument(level: f32, latency: Frame, probe: &Probe) -> Box<Fake> {
        Box::new(Fake { id: 0, kind: SlotKind::Instrument, latency, level, through: 0.0, impulse: None, last: 0.0, probe: probe.clone() })
    }

    fn with_last(mut self: Box<Self>, last: f32) -> Box<Fake> {
        self.last = last;
        self
    }

    fn with_id(mut self: Box<Self>, id: u32) -> Box<Fake> {
        self.id = id;
        self
    }
}

impl SlotProcessor for Fake {
    fn kind(&self) -> SlotKind {
        self.kind
    }

    fn latency(&self) -> Frame {
        self.latency
    }

    fn process(&mut self, frame: Frame, input: &[f32], events: &[SlotEvent], out: &mut [f32]) {
        let mut log = self.probe.log.lock().unwrap();
        if log.len() + 1 + events.len() < LOG {
            log.push(Seen::Call { frame, len: out.len() });
            log.extend(events.iter().map(|&e| Seen::Event(e)));
        }
        let peak = input.iter().fold(self.probe.input_peak(), |m, x| m.max(x.abs()));
        self.probe.input_peak.store(peak.to_bits(), SeqCst);
        let mut next = events.iter().peekable();
        for (k, (y, &x)) in out.iter_mut().zip(input).enumerate() {
            while let Some(e) = next.next_if(|e| e.offset as usize == k) {
                if matches!(e.kind, SlotEventKind::NoteOn { .. }) {
                    self.impulse = Some(frame + k as Frame + self.latency);
                }
            }
            let hit = if self.impulse == Some(frame + k as Frame) { 1.0 } else { 0.0 };
            *y = self.level + self.through * x + hit;
        }
        if let Some(y) = out.last_mut() {
            *y += self.last;
        }
    }

    fn stop(&mut self) {
        self.probe.log.lock().unwrap().push(Seen::Stop);
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.probe.drops.fetch_add(1, SeqCst);
    }
}

/// A unit the engine handed back, as the fake it is.
fn fake(unit: Box<dyn SlotProcessor>) -> Box<Fake> {
    unit.into_any().downcast::<Fake>().ok().expect("the unit is the fake")
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, v| m.max(v.abs()))
}

fn assert_bits(got: &[f32], want: impl Fn(usize) -> f32, what: &str) {
    for (k, &y) in got.iter().enumerate() {
        assert_eq!(y.to_bits(), want(k).to_bits(), "{what}: frame {k}: {y} vs {}", want(k));
    }
}

/// A crossfade `fade` frames (of `FADE`) from the bypass signal towards the unit's output.
fn faded(bypass: f32, unit: f32, fade: usize) -> f32 {
    match fade {
        0 => bypass,
        FADE => unit,
        k => bypass + (k as f32 / FADE as f32) * (unit - bypass),
    }
}

/// A live gate's ramp in frames at `sr`: 5 ms, rounded, at least one.
fn live_frames(sr: u32) -> Frame {
    ((sr as f64 * 0.005).round() as Frame).max(1)
}

/// The live gate as STATUS D23 specifies it, at frame `f`: settled at `initial` before the first toggle,
/// then each `(frame, on)` toggle in order. A toggle to the state it heads for already changes nothing;
/// any other ramps linearly over `n` frames from the gain reached at its frame, `from + (to - from) * u`
/// with `u = (f - frame) / n`, and from `frame + n` on the gate is settled. The gain, or `None` once
/// settled (on: the input itself; off: silence).
fn gate_at(initial: bool, toggles: &[(Frame, bool)], n: Frame, f: Frame) -> (bool, Option<f32>) {
    let level = |on: bool| if on { 1.0f32 } else { 0.0 };
    let gain = |on: bool, ramp: Option<(Frame, f32)>, t: Frame| match ramp {
        Some((at, from)) if t - at < n => Some(from + (level(on) - from) * ((t - at) as f32 / n as f32)),
        _ => None,
    };
    let (mut on, mut ramp) = (initial, None);
    for &(at, to) in toggles.iter().filter(|&&(at, _)| at <= f) {
        if to == on {
            continue;
        }
        let from = gain(on, ramp, at).unwrap_or(level(on));
        on = to;
        ramp = if from == level(to) { None } else { Some((at, from)) };
    }
    (on, gain(on, ramp, f))
}

/// `x` through a live gate in `state` ([`gate_at`]).
fn through(x: f32, state: (bool, Option<f32>)) -> f32 {
    match state {
        (_, Some(g)) => x * g,
        (true, None) => x,
        (false, None) => 0.0,
    }
}

/// `x` through the live gate at frame `f` ([`gate_at`] at 48 kHz).
fn live(x: f32, initial: bool, toggles: &[(Frame, bool)], f: Frame) -> f32 {
    through(x, gate_at(initial, toggles, LIVE, f))
}

// ── Install and removal ──────────────────────────────────────────────────────────────────────────────

#[test]
fn a_unit_installed_before_the_first_block_is_engaged_at_once() {
    let probe = Probe::new();
    let mut rig = Rig::new();
    rig.set_level(1.0);
    rig.install(0, Fake::effect(0.25, 0.0, &probe));
    rig.keep_output();
    rig.advance(256);
    assert_bits(&rig.monitor, |_| 0.25, "the effect's output from the first frame");
    assert_eq!(rig.engine.slot(0), Some((SlotKind::Effect, 0)));
}

#[test]
fn a_later_effect_crossfades_in_from_its_dry_input_and_out_again_then_stops_and_comes_back() {
    let probe = Probe::new();
    let mut rig = Rig::new();
    rig.set_level(1.0);
    rig.advance(256);
    rig.install(0, Fake::effect(0.25, 0.0, &probe).with_id(7));
    rig.keep_output();
    rig.advance(1024);
    assert_bits(&rig.monitor, |k| faded(1.0, 0.25, k.min(FADE)), "fading in");

    let removed_at = rig.frame;
    rig.remove(0);
    rig.keep_output();
    rig.advance(1024);
    assert_bits(&rig.monitor, |k| faded(1.0, 0.25, FADE.saturating_sub(k)), "fading out");
    let seen = probe.seen();
    assert_eq!(probe.stops(), 1, "stopped exactly once");
    assert_eq!(seen.last(), Some(&Seen::Stop), "after its last process");
    let (frame, len) = *probe.calls().last().unwrap();
    assert!(frame + len as Frame >= removed_at + FADE as Frame, "processed through the fade");
    assert_eq!(rig.engine.slot(0), None);
    let unit = fake(rig.returned(0).expect("the unit comes back on the port"));
    assert_eq!(unit.id, 7);
    assert!(rig.returned(0).is_none());
}

#[test]
fn a_later_instrument_crossfades_in_from_silence_and_out_to_silence() {
    let probe = Probe::new();
    let mut rig = Rig::new();
    rig.advance(256);
    rig.install(1, Fake::instrument(0.25, 0, &probe));
    rig.keep_output();
    rig.advance(1024);
    assert_bits(&rig.bus, |k| faded(0.0, 0.25, k.min(FADE)), "fading in");
    rig.remove(1);
    rig.keep_output();
    rig.advance(1024);
    assert_bits(&rig.bus, |k| faded(0.0, 0.25, FADE.saturating_sub(k)), "fading out");
    assert_eq!(probe.stops(), 1);
    assert!(rig.returned(1).is_some());
}

#[test]
fn the_engine_never_drops_a_unit() {
    let (a, b, c) = (Probe::new(), Probe::new(), Probe::new());
    let mut rig = Rig::new();
    rig.install(0, Fake::effect(0.1, 0.0, &a));
    rig.advance(512);
    rig.install(0, Fake::effect(0.1, 0.0, &b)); // occupied: handed back
    rig.install(1, Fake::instrument(0.1, 0, &c));
    rig.advance(512);
    rig.remove(0);
    rig.advance(2048);
    rig.engine.evict_slots();
    let back: Vec<_> = [0, 0, 1].into_iter().map(|s| rig.returned(s).expect("every unit comes back")).collect();
    assert_eq!([&a, &b, &c].map(|p| p.drops.load(SeqCst)), [0, 0, 0], "no drop while the engine held them");
    drop(back);
    assert_eq!([&a, &b, &c].map(|p| p.drops.load(SeqCst)), [1, 1, 1], "the test dropped each once");
}

#[test]
fn an_install_into_an_occupied_slot_hands_the_new_unit_back_and_counts_it() {
    let (a, b) = (Probe::new(), Probe::new());
    let mut rig = Rig::new();
    rig.install(0, Fake::effect(0.1, 0.0, &a).with_id(1));
    rig.advance(128);
    rig.install(0, Fake::effect(0.2, 0.0, &b).with_id(2));
    rig.advance(128);
    assert_eq!(fake(rig.returned(0).expect("handed back")).id, 2);
    assert_eq!(rig.engine.diag().slot_protocol_errors, 1);
    assert_eq!((b.calls().len(), b.stops()), (0, 0), "the refused unit never ran");
    assert_eq!(rig.engine.slot(0), Some((SlotKind::Effect, 0)), "the first one stays");
    assert_eq!(a.calls().len(), 2);
}

/// A host that never reads its port: the return ring fills, two more units park, and the next is
/// leaked and counted rather than dropped. Parked units move on once the host reads.
#[test]
fn a_unit_with_nowhere_to_go_is_leaked_never_dropped_and_parked_ones_follow_once_read() {
    let refused = Probe::new();
    let mut rig = Rig::new();
    rig.install(0, Fake::effect(0.0, 0.0, &Probe::new()));
    rig.advance(128);
    for burst in [4, 3] {
        for _ in 0..burst {
            rig.install(0, Fake::effect(0.0, 0.0, &refused));
        }
        rig.advance(128);
    }
    assert_eq!(rig.engine.diag().slot_protocol_errors, 7 + 1, "seven refused, one of them leaked");
    let mut back = Vec::new();
    while let Some(unit) = rig.returned(0) {
        back.push(unit);
    }
    assert_eq!(back.len(), 4, "the return ring's four");
    rig.advance(128);
    while let Some(unit) = rig.returned(0) {
        back.push(unit);
    }
    assert_eq!(back.len(), 6, "and the two parked ones");
    drop(back);
    assert_eq!(refused.drops.load(SeqCst), 6, "the leaked one is never dropped");
}

/// Where a [`Panicky`] unit panics.
#[derive(Clone, Copy, PartialEq)]
enum PanicIn {
    Kind,
    Stop,
}

/// A unit that panics in one method, as a plugin can, and counts its drops.
struct Panicky {
    at: PanicIn,
    drops: Arc<AtomicUsize>,
}

impl SlotProcessor for Panicky {
    fn kind(&self) -> SlotKind {
        assert!(self.at != PanicIn::Kind, "the unit panics in kind()");
        SlotKind::Effect
    }

    fn latency(&self) -> Frame {
        0
    }

    fn process(&mut self, _frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
        out.fill(0.0);
    }

    fn stop(&mut self) {
        assert!(self.at != PanicIn::Stop, "the unit panics in stop()");
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

impl Drop for Panicky {
    fn drop(&mut self) {
        self.drops.fetch_add(1, SeqCst);
    }
}

/// One block as the device callback renders it: under `catch_unwind`, its guard. False on a panic.
fn guarded_block(rig: &mut Rig) -> bool {
    let n = rig.block;
    let ctx = ProcessContext { frame: rig.frame, xrun: false, damaged: false, align_frames: rig.align - rig.engine.limiter_latency(), input_frames: 0 };
    let (input, mut left, mut right) = (vec![0.0; n], vec![0.0; n], vec![0.0; n]);
    let engine = &mut rig.engine;
    let ok = catch_unwind(AssertUnwindSafe(|| engine.process(&ctx, &input, &mut left, &mut right))).is_ok();
    rig.frame += n as Frame;
    ok
}

/// A unit that panics while the engine calls it at install (`kind`) or at the end of a removal (`stop`)
/// unwinds out of the callback still held by its slot: the unwind drops nothing on the audio thread.
#[test]
fn a_unit_that_panics_is_never_dropped_by_the_unwind() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut rig = Rig::new();
    rig.advance(128);
    rig.install(0, Box::new(Panicky { at: PanicIn::Kind, drops: drops.clone() }));
    assert!(!guarded_block(&mut rig), "kind() panicked inside the block");
    assert_eq!(drops.load(SeqCst), 0, "the unwind dropped nothing");
    // The owner evicts the faulted engine's units, under its own guard.
    assert!(catch_unwind(AssertUnwindSafe(|| rig.engine.evict_slots())).is_ok());
    let back = rig.returned(0).expect("the unit comes back");
    assert_eq!(drops.load(SeqCst), 0);
    drop(back);
    assert_eq!(drops.load(SeqCst), 1);

    let drops = Arc::new(AtomicUsize::new(0));
    let mut rig = Rig::new();
    rig.install(0, Box::new(Panicky { at: PanicIn::Stop, drops: drops.clone() }));
    rig.advance(256);
    rig.remove(0);
    let blocks = (0..FADE / 128 + 2).take_while(|_| guarded_block(&mut rig)).count();
    // The removal starts with the first block; the fade reaches bypass in block FADE.div_ceil(128).
    assert_eq!(blocks, FADE.div_ceil(128) - 1, "stop() panicked once the fade reached bypass, not before");
    assert_eq!(drops.load(SeqCst), 0, "the unwind dropped nothing");
    assert!(rig.returned(0).is_none() && rig.engine.slot(0).is_some(), "the unit stays in its slot");
    drop(rig); // the engine's drop, off the audio thread
    assert_eq!(drops.load(SeqCst), 1);
}

// ── Notes ────────────────────────────────────────────────────────────────────────────────────────────

/// Two instrument fakes, one per slot.
fn two_instruments() -> (Rig, Probe, Probe) {
    let (a, b) = (Probe::new(), Probe::new());
    let mut rig = Rig::new();
    rig.install(0, Fake::instrument(0.0, 0, &a));
    rig.install(1, Fake::instrument(0.0, 0, &b));
    (rig, a, b)
}

#[test]
fn notes_reach_only_the_target_slot_on_their_frame() {
    let (mut rig, a, b) = two_instruments();
    let f = rig.frame;
    rig.send_at(f + 10, Command::SelectInstrument(NoteTarget::Slot(1)));
    rig.send_at(f + 20, Command::NoteOn(60, 0.5));
    rig.send_at(f + 30, Command::NoteOff(60));
    rig.advance(256);
    assert!(a.events().is_empty());
    let on = SlotEventKind::NoteOn { key: 60, velocity: 0.5 };
    assert_eq!(b.events(), vec![(f + 20, 0, on), (f + 30, 0, SlotEventKind::NoteOff { key: 60 })]);
}

#[test]
fn moving_the_target_away_or_reselecting_it_releases_the_slots_held_notes() {
    let (mut rig, a, b) = two_instruments();
    let f = rig.frame;
    let script = [
        (0, Command::SelectInstrument(NoteTarget::Slot(1))),
        (10, Command::NoteOn(64, 0.8)),
        (11, Command::NoteOn(60, 0.8)),
        (50, Command::SelectInstrument(NoteTarget::Slot(0))),
        (60, Command::NoteOn(62, 0.8)),
        (70, Command::SelectInstrument(NoteTarget::Slot(0))),
        (80, Command::NoteOn(65, 0.8)),
        (90, Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead))),
        (100, Command::NoteOff(65)),
    ];
    for (at, command) in script {
        rig.send_at(f + at, command);
    }
    rig.advance(256);
    assert_eq!(b.keys(), vec![(true, 64), (true, 60), (false, 60), (false, 64)]);
    assert_eq!(b.events()[2].0, f + 50, "released where the target moved");
    assert_eq!(a.keys(), vec![(true, 62), (false, 62), (true, 65), (false, 65)]);
    assert_eq!(a.events()[1].0, f + 70, "re-selecting the slot releases it too");
    assert_eq!(a.events()[3].0, f + 90, "a built-in target releases the slot, and its NoteOff goes nowhere");
}

#[test]
fn a_note_off_the_slot_does_not_hold_is_not_delivered_and_all_notes_off_releases_the_rest() {
    let (mut rig, _, b) = two_instruments();
    let f = rig.frame;
    let script = [
        (0, Command::SelectInstrument(NoteTarget::Slot(1))),
        (5, Command::NoteOff(61)),
        (10, Command::NoteOn(60, 1.0)),
        (11, Command::NoteOn(67, 1.0)),
        (20, Command::NoteOff(60)),
        (21, Command::NoteOff(60)),
        (30, Command::AllNotesOff),
        (40, Command::NoteOff(67)),
        (50, Command::AllNotesOff),
        (60, Command::NoteOn(200, 1.0)),
        (61, Command::NoteOff(200)),
        (62, Command::NoteOn(127, 1.0)),
        (63, Command::NoteOff(127)),
    ];
    for (at, command) in script {
        rig.send_at(f + at, command);
    }
    rig.advance(256);
    assert_eq!(b.keys(), vec![(true, 60), (true, 67), (false, 60), (false, 67), (true, 127), (false, 127)], "a key past 127 goes nowhere");
    assert_eq!(b.events()[3].0, f + 30);
}

#[test]
fn notes_to_an_empty_or_a_removing_slot_go_nowhere_and_a_removal_releases_first() {
    let b = Probe::new();
    let mut rig = Rig::new();
    rig.press(Command::SelectInstrument(NoteTarget::Slot(2))); // no such slot
    rig.press(Command::NoteOn(61, 1.0));
    rig.press(Command::SelectInstrument(NoteTarget::Slot(1)));
    rig.press(Command::NoteOn(60, 1.0)); // the slot is empty
    rig.install(1, Fake::instrument(0.0, 0, &b));
    rig.advance(256);
    assert!(b.events().is_empty(), "no note waited for the unit");

    rig.press(Command::NoteOn(62, 1.0));
    rig.advance(128);
    rig.remove(1);
    let removed_at = rig.frame;
    rig.press(Command::NoteOn(64, 1.0)); // the removal's first block: the slot is removing
    rig.advance(64);
    rig.press(Command::NoteOff(62));
    rig.advance(2048);
    assert_eq!(b.keys(), vec![(true, 62), (false, 62)], "only the removal's release reached it");
    assert_eq!(b.events()[1].0, removed_at, "released at the removal's block start");
    let seen = b.seen();
    let off = seen.iter().position(|s| matches!(s, Seen::Event(SlotEvent { kind: SlotEventKind::NoteOff { .. }, .. }))).unwrap();
    assert!(matches!(seen[off - 1], Seen::Call { .. }), "inside a process call");
    assert_eq!(seen.iter().position(|s| *s == Seen::Stop), Some(seen.len() - 1), "before the unit stops");
    assert!(rig.returned(1).is_some());
}

#[test]
fn a_built_in_target_and_a_slot_target_never_both_sound_a_note() {
    let b = Probe::new();
    let mut rig = Rig::new();
    rig.install(1, Fake::instrument(0.0, 0, &b));
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)));
    rig.keep_output();
    rig.press(Command::NoteOn(60, 1.0));
    rig.advance(4800);
    assert!(peak(&rig.bus) > 0.05, "the lead sounds");
    assert!(b.events().is_empty(), "the slot does not");

    rig.set(Command::SelectInstrument(NoteTarget::Slot(1)));
    assert_eq!(rig.engine.instruments().selected(), None);
    rig.advance(rig.seconds(3.0)); // the lead's released note rings out
    rig.keep_output();
    rig.press(Command::NoteOn(64, 1.0));
    rig.advance(4800);
    let sounding = rig.bus.iter().filter(|x| x.abs() > 1e-4).count();
    assert_eq!(sounding, 1, "only the slot's impulse: no built-in instrument takes the note");
    assert_eq!(b.keys(), vec![(true, 64)]);
}

// ── Plugin calls ─────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_plugin_is_called_once_per_block_unless_a_slot_command_splits_it() {
    for block in [64usize, 256, 480] {
        let (a, b) = (Probe::new(), Probe::new());
        let mut rig = Rig::with(Opts { block, ..Default::default() });
        rig.install(0, Fake::effect(0.0, 1.0, &a));
        rig.install(1, Fake::instrument(0.0, 0, &b));
        let f = rig.frame;
        // A count-in's beats, the metronome and the FX quanta all split the engine's chunks.
        for command in [Command::SetMetronome(true), Command::RecDub(0), Command::SelectInstrument(NoteTarget::Slot(1))] {
            rig.send_at(f, command);
        }
        rig.advance(40 * block as Frame);
        for (frame, len) in a.calls().into_iter().chain(b.calls()) {
            assert_eq!(len, block, "block {block}: one call per block");
            assert_eq!((frame - f) % block as Frame, 0);
        }
        assert_eq!(a.calls().len(), 40);

        let from = rig.frame;
        let mid = from + 3 * block as Frame + 17;
        rig.send_at(mid, Command::NoteOn(60, 1.0));
        rig.advance(6 * block as Frame);
        let calls: Vec<_> = b.calls().into_iter().filter(|c| c.0 >= from).collect();
        let split = mid - 17;
        let want: Vec<(Frame, usize)> = (0..6)
            .flat_map(|k| {
                let at = from + k * block as Frame;
                if at == split { vec![(at, 17), (mid, block - 17)] } else { vec![(at, block)] }
            })
            .collect();
        assert_eq!(calls, want, "block {block}: split exactly at the note");
        assert_eq!(b.events().last().map(|e| (e.0, e.1)), Some((mid, 0)), "the note opens its call");
        assert_eq!(a.calls().len(), 40 + 7, "both slots render the split range");
    }
}

// ── Alignment and the record path ─────────────────────────────────────────────────────────────────────

const PHYS: Frame = 1123;
const PLUGIN: Frame = 57;
const LIMITER: Frame = 288;

/// A one-bar take after `setup`, with the player hitting the heard downbeat (`tests/align.rs`): how
/// far its window starts after the downbeat, and the loop's first frame.
fn aligned_take(setup: impl FnOnce(&mut Rig)) -> (Frame, f32) {
    let mut rig = Rig::with(Opts { align: PHYS + LIMITER, ..Default::default() });
    setup(&mut rig);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let downbeat = rig.count_one(mark) + 4 * 24_000;
    rig.set_input(move |f| if f == downbeat + LIMITER + PHYS { 1.0 } else { 0.0 });
    let offset = rig.start_frame() - downbeat;
    rig.advance_to(rig.end_frame() + 1);
    assert_eq!(rig.state(0), LaneState::Playing);
    (offset, rig.pcm(0)[0])
}

#[test]
fn an_effects_latency_moves_the_take_only_while_its_slot_is_live() {
    let live = aligned_take(|rig| rig.install(0, Box::new(Delay::new(PLUGIN))));
    assert_eq!(live, (PHYS + PLUGIN + LIMITER, 1.0), "a live effect: its latency joins the alignment");
    let idle = aligned_take(|rig| {
        rig.install(0, Box::new(Delay::new(PLUGIN)));
        rig.set(Command::SetSlotLive(0, false));
        rig.set(Command::SetSlotLive(1, true));
    });
    assert_eq!(idle, (PHYS + LIMITER, 1.0), "the effect's slot off, the dry slot live: no latency");
    let other = aligned_take(|rig| rig.install(1, Box::new(Delay::new(PLUGIN))));
    assert_eq!(other.0, PHYS + LIMITER, "an effect in a slot that is not live");
}

#[test]
fn a_live_flag_reaches_the_alignment_from_the_next_block() {
    let take_offset = |early: bool| {
        let mut rig = Rig::with(Opts { align: PHYS + LIMITER, ..Default::default() });
        rig.install(1, Box::new(Delay::new(PLUGIN)));
        rig.advance(128);
        rig.send_at(rig.frame, Command::SetSlotLive(1, true));
        if early {
            rig.advance(1);
        }
        let mark = rig.events.len();
        rig.press(Command::RecDub(0)); // the same block as the flag, or the next
        rig.start_frame() - (rig.count_one(mark) + 4 * 24_000)
    };
    assert_eq!(take_offset(false), PHYS + LIMITER);
    assert_eq!(take_offset(true), PHYS + PLUGIN + LIMITER);
}

#[test]
fn an_instrument_installed_into_a_live_slot_silences_its_dry_signal_from_the_install() {
    let probe = Probe::new();
    let mut rig = Rig::new();
    rig.set_level(1.0);
    rig.advance(256);
    rig.install(0, Fake::instrument(0.0, 0, &probe));
    rig.keep_output();
    rig.advance(256);
    assert!(rig.monitor.iter().all(|&x| x == 0.0), "no dry frame once the instrument is in");
}

#[test]
fn an_instrument_slot_is_heard_on_both_sides_of_the_bus_and_not_on_the_monitor() {
    let probe = Probe::new();
    let mut rig = Rig::new();
    rig.install(1, Fake::instrument(0.25, 0, &probe));
    rig.keep_output();
    rig.advance(1024);
    assert_bits(&rig.bus, |_| 0.25, "left");
    assert_eq!(rig.bus, rig.bus_right);
    assert!(rig.monitor.iter().all(|&m| m == 0.0), "the dry slot passes the silent input");
    assert_eq!(probe.input_peak(), 0.0, "an instrument takes no input");
}

/// `tests/sound.rs`'s note on the heard click, played on an instrument slot whose output lags its note
/// by `latency`: the record path lags it by the input side plus the live effect's latency less its
/// own, so the note lands on the loop's frame 0. A latency beyond that lands late by the difference.
#[test]
fn an_instrument_slots_note_played_on_the_heard_click_lands_on_the_grid() {
    const IN: Frame = 480;
    const OUT: Frame = 1000;
    let take = |latency: Frame| {
        let probe = Probe::new();
        let mut rig = Rig::with(Opts { align: IN + OUT + LIMITER, ..Default::default() });
        rig.input_latency = IN;
        rig.install(0, Box::new(Delay::new(PLUGIN)));
        rig.install(1, Fake::instrument(0.0, latency, &probe));
        rig.set(Command::SelectInstrument(NoteTarget::Slot(1)));
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(1.0));
        let mark = rig.events.len();
        rig.press(Command::RecDub(0));
        let downbeat = rig.count_one(mark) + 4 * 24_000;
        rig.send_at(downbeat + LIMITER + OUT, Command::NoteOn(69, 1.0));
        rig.advance_to(rig.end_frame() + 1);
        assert_eq!(rig.state(0), LaneState::Playing);
        let pcm = rig.pcm(0);
        let hits: Vec<usize> = pcm.iter().enumerate().filter(|(_, &x)| x != 0.0).map(|(k, _)| k).collect();
        assert_eq!(hits.len(), 1, "latency {latency}: one impulse");
        assert_eq!(pcm[hits[0]], 1.0);
        hits[0] as Frame
    };
    for latency in [0, 200, IN + PLUGIN] {
        assert_eq!(take(latency), 0, "latency {latency}: on the loop's frame 0");
    }
    assert_eq!(take(IN + PLUGIN + 100), 100, "100 frames more than the input side: 100 late");
}

/// The record path lags an instrument slot's output: a removal that completes while its note is still
/// on the way leaves the note where it was headed, played once.
#[test]
fn an_instrument_slot_removed_while_its_note_is_on_the_record_path_still_lands_it_on_the_grid() {
    const IN: Frame = 2000;
    const OUT: Frame = 1000;
    const LATENCY: Frame = 200;
    let probe = Probe::new();
    let mut rig = Rig::with(Opts { align: IN + OUT + LIMITER, ..Default::default() });
    rig.input_latency = IN;
    rig.install(1, Fake::instrument(0.0, LATENCY, &probe));
    rig.set(Command::SelectInstrument(NoteTarget::Slot(1)));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let note = rig.count_one(mark) + 4 * 24_000 + LIMITER + OUT;
    rig.send_at(note, Command::NoteOn(69, 1.0));
    rig.advance_to(note + LATENCY + 1); // the note has sounded
    rig.remove(1);
    rig.advance(FADE as Frame + 128);
    assert!(rig.returned(1).is_some(), "removed before the note reaches the take");
    rig.advance_to(rig.end_frame() + 1);
    let pcm = rig.pcm(0);
    let hits: Vec<usize> = pcm.iter().enumerate().filter(|(_, &x)| x != 0.0).map(|(k, _)| k).collect();
    assert_eq!(hits, vec![0], "once, on the loop's frame 0");
}

/// An input side past the record line's length (over a second) holds the line's longest delay; the
/// line never wraps round to no delay at all.
#[test]
fn a_record_delay_past_the_line_holds_its_longest() {
    const IN: Frame = 60_000;
    let probe = Probe::new();
    let mut rig = Rig::with(Opts { align: IN + LIMITER, ..Default::default() });
    rig.input_latency = IN;
    rig.install(1, Fake::instrument(0.0, 0, &probe));
    rig.set(Command::SelectInstrument(NoteTarget::Slot(1)));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(2.0));
    rig.press(Command::RecDub(0));
    let note = rig.start_frame() + 1000;
    rig.send_at(note, Command::NoteOn(69, 1.0));
    rig.advance_to(rig.end_frame() + 1);
    let pcm = rig.pcm(0);
    let hit = pcm.iter().position(|&x| x != 0.0).expect("the note is in the take") as Frame;
    assert!(hit - 1000 > rig.seconds(1.0), "delayed by the line's second and more: {}", hit - 1000);
}

#[test]
fn a_slots_gain_scales_what_is_heard_and_what_is_recorded_smoothly() {
    let probe = Probe::new();
    let mut rig = Rig::new();
    rig.set_level(1.0);
    rig.install(1, Fake::instrument(0.25, 0, &probe));
    rig.advance(4800);
    rig.keep_output();
    rig.press(Command::SetSlotGain(0, 0.5));
    rig.press(Command::SetSlotGain(1, 2.0));
    rig.advance(9600);
    assert!(rig.monitor[0] < 1.0 && rig.monitor[0] > 0.99, "no jump: {}", rig.monitor[0]);
    assert!(rig.monitor.windows(2).all(|w| w[1] <= w[0]), "the dry slot's level glides down");
    assert!(rig.bus[2..].windows(2).all(|w| w[1] >= w[0]), "the instrument's glides up");
    assert!((rig.monitor[rig.monitor.len() - 1] - 0.5).abs() < 1e-6);
    assert!((rig.bus[rig.bus.len() - 1] - 0.5).abs() < 1e-6);

    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    rig.press(Command::RecDub(0));
    rig.advance_to(rig.end_frame() + 1);
    let pcm = rig.pcm(0);
    assert!(pcm.iter().all(|&x| (x - 1.0).abs() < 1e-6), "recorded: 0.5 dry + 0.5 instrument");
}

#[test]
fn a_slot_passes_the_input_only_while_live() {
    let mut rig = Rig::new();
    let input = |f: Frame| 0.5 * (f as f32 * 0.01).sin();
    rig.set_input(input);
    rig.keep_output();
    rig.advance(1000);
    let start = rig.output.as_ref().unwrap().0;
    assert_bits(&rig.monitor, |k| input(start + k as Frame), "empty and live: dry");

    let off = rig.frame;
    rig.set(Command::SetSlotLive(0, false));
    rig.keep_output();
    rig.advance(1000);
    let start = rig.output.as_ref().unwrap().0;
    // The mix sums from silence: `0.0 +` keeps a gated -0.0 as the mix has it.
    assert_bits(&rig.monitor, |k| 0.0 + live(input(start + k as Frame), true, &[(off, false)], start + k as Frame), "empty and going off: ramped out");
    assert!(rig.monitor[(off + LIVE - start) as usize..].iter().all(|&x| x.to_bits() == 0), "empty and not live: silent once the ramp ends");

    let probe = Probe::new();
    rig.install(0, Fake::effect(0.0, 1.0, &probe));
    rig.advance(4800);
    assert_eq!(probe.input_peak(), 0.0, "an effect in a slot that is not live gets silence");
    rig.set(Command::SetSlotLive(0, true));
    rig.advance(1000);
    assert!(probe.input_peak() > 0.4, "live, it gets the input");
}

// ── Each slot its own input ──────────────────────────────────────────────────────────────────────────

/// Two capture channels, one per live slot (distinct levels, so a slot reading the other's, or both
/// reading one, shows): each slot takes only its own, and both are heard and recorded, each once.
#[test]
fn two_live_slots_each_monitor_and_record_only_their_own_input() {
    let mut rig = Rig::new();
    rig.set_inputs(|_| 0.25, |_| 0.5);
    rig.set(Command::SetSlotLive(1, true));
    rig.keep_output();
    rig.advance(1000);
    assert_bits(&rig.monitor, |_| 0.75, "both live: each input heard once");
    // Each toggle ramps its own slot's input from the command's frame (`live`), the other slot's
    // untouched: the mix is slot 0's gated input plus slot 1's, summed from silence.
    let off0 = rig.frame;
    rig.set(Command::SetSlotLive(0, false));
    rig.keep_output();
    rig.advance(1000);
    let start = rig.output.as_ref().unwrap().0;
    assert_bits(&rig.monitor, |k| 0.0 + live(0.25, true, &[(off0, false)], start + k as Frame) + 0.5, "slot 0 ramping out, slot 1 its own input");
    assert_bits(&rig.monitor[(off0 + LIVE - start) as usize..], |_| 0.5, "slot 1 alone: its own input");
    let on0 = rig.frame;
    rig.set(Command::SetSlotLive(0, true));
    let off1 = rig.frame;
    rig.set(Command::SetSlotLive(1, false));
    rig.keep_output();
    rig.advance(1000);
    let start = rig.output.as_ref().unwrap().0;
    let both = |k: usize| {
        let f = start + k as Frame;
        0.0 + live(0.25, false, &[(on0, true)], f) + live(0.5, true, &[(off1, false)], f)
    };
    assert_bits(&rig.monitor, both, "slot 0 ramping in, slot 1 out, each from its own command's frame");
    assert_bits(&rig.monitor[(off1 + LIVE - start) as usize..], |_| 0.25, "slot 0 alone: its own input");

    rig.set(Command::SetSlotLive(1, true));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    rig.press(Command::RecDub(0));
    rig.advance_to(rig.end_frame() + 1);
    assert_eq!(rig.state(0), LaneState::Playing);
    assert!(rig.pcm(0).iter().all(|&x| (x - 0.75).abs() < 1e-6), "recorded: both inputs, each once");

    let (a, b) = (Probe::new(), Probe::new());
    rig.install(0, Fake::effect(0.0, 1.0, &a));
    rig.install(1, Fake::effect(0.0, 1.0, &b));
    rig.advance(1024);
    assert_eq!((a.input_peak(), b.input_peak()), (0.25, 0.5), "each effect takes its own slot's input");
}

/// A dry slot and a live effect with latency, each on its own input, played at once (on the heard
/// downbeat): each is heard as soon as it can be, the dry one at once and the effect its latency
/// later, and both land together on the take's first frame.
#[test]
fn a_dry_and_a_latent_live_slot_land_together_in_the_take() {
    let mut rig = Rig::with(Opts { align: PHYS + LIMITER, ..Default::default() });
    rig.install(0, Box::new(Delay::new(PLUGIN)));
    rig.set(Command::SetSlotLive(1, true));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let downbeat = rig.count_one(mark) + 4 * 24_000;
    let played = downbeat + LIMITER + PHYS;
    rig.set_inputs(move |f| if f == played { 1.0 } else { 0.0 }, move |f| if f == played { 0.5 } else { 0.0 });
    assert_eq!(rig.start_frame() - downbeat, PHYS + PLUGIN + LIMITER, "the live effect's latency joins the alignment");
    rig.keep_output();
    let from = rig.frame;
    rig.advance_to(rig.end_frame() + 1);
    let heard: Vec<(Frame, f32)> = rig.monitor.iter().enumerate().filter(|(_, &x)| x != 0.0).map(|(k, &x)| (from + k as Frame, x)).collect();
    assert_eq!(heard, [(played, 0.5), (played + PLUGIN, 1.0)], "heard: the dry note at once, the effect's its latency later");
    assert_eq!(rig.state(0), LaneState::Playing);
    let pcm = rig.pcm(0);
    let hits: Vec<(usize, f32)> = pcm.iter().enumerate().filter(|(_, &x)| x != 0.0).map(|(k, &x)| (k, x)).collect();
    assert_eq!(hits, [(0, 1.5)], "recorded: both notes, together, on the loop's frame 0");
}

/// A mid-take change to the live slots, at frame `at` of the take.
#[derive(Clone, Copy, Debug)]
enum MidTake {
    /// Slot 0 (the effect's) goes live or off.
    EffectLive(bool),
    /// Slot 1 (the dry one) goes off.
    DryOff,
    /// Slot 0's effect leaves (its slot stays live, empty).
    RemoveEffect,
}

/// A two-bar take with a dry voice (a frame code) on slot 1 and an effect (`LATENCY` frames of delay)
/// on slot 0 fed `fx` times the code; one [`MidTake`] change a bar in. The take's alignment is fixed at
/// its arm, and the record compensation holds with it: each voice lands on the loop as its latched delay
/// puts it, frame for frame, with nothing skipped or repeated at the change, and what was on its way
/// when a slot went off still lands. `expected(t, at)` is the loop's frame at input frame `t` given the
/// change at `at`.
fn take_through(effect_live: bool, change: MidTake, fx: f32, expected: impl Fn(Frame, Frame) -> f32) {
    const LATENCY: Frame = 480;
    let mut rig = Rig::with(Opts { align: PHYS + LIMITER, ..Default::default() });
    rig.install(0, Box::new(Delay::new(LATENCY)));
    rig.set(Command::SetSlotLive(0, effect_live));
    rig.set(Command::SetSlotLive(1, true));
    rig.set_inputs(move |f| fx * common::code(f), common::code);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(2.0));
    rig.press(Command::RecDub(0));
    let start = rig.start_frame();
    rig.advance_to(start + rig.fpb() + 77);
    let at = rig.frame;
    match change {
        MidTake::EffectLive(on) => rig.set(Command::SetSlotLive(0, on)),
        MidTake::DryOff => rig.set(Command::SetSlotLive(1, false)),
        MidTake::RemoveEffect => rig.remove(0),
    }
    rig.advance_to(rig.end_frame() + 1);
    assert_eq!(rig.state(0), LaneState::Playing, "{change:?}");
    let pcm = rig.pcm(0);
    assert_eq!(pcm.len() as Frame, 2 * rig.fpb());
    let wrong = pcm.iter().enumerate().find(|&(k, &x)| x.to_bits() != expected(start + k as Frame, at).to_bits());
    assert_eq!(wrong.map(|(k, &x)| (k, x, expected(start + k as Frame, at))), None, "{change:?}: the first frame off its latched delay (loop frame, got, want)");
}

#[test]
fn the_record_compensation_holds_through_a_take_whatever_the_slots_do_mid_take() {
    const L: Frame = 480;
    let code = common::code;
    // Each voice is its input gated at its own input frame (the ramp from `at`, `live`), then delayed as
    // its latch puts it.
    // Latched with the effect live: both voices L late, the effect's ramped out from its input at `at`.
    take_through(true, MidTake::EffectLive(false), 2.0, |t, at| code(t - L) + live(2.0 * code(t - L), true, &[(at, false)], t - L));
    // Latched with it off: the dry voice stays on time, the effect's lands L late, ramped in from `at`.
    take_through(false, MidTake::EffectLive(true), 2.0, |t, at| code(t) + live(2.0 * code(t - L), false, &[(at, true)], t - L));
    // The dry slot off: what was on its way (L frames) still lands, ramped out from its input at `at`.
    take_through(true, MidTake::DryOff, 2.0, |t, at| 2.0 * code(t - L) + live(code(t - L), true, &[(at, false)], t - L));
    // The effect leaves: the dry voice keeps its delay.
    take_through(true, MidTake::RemoveEffect, 0.0, |t, _| code(t - L));
}

/// `NoteTarget::Off` takes a slot's notes nowhere: switching to it releases what the slot holds, and a
/// note after it reaches no slot.
#[test]
fn off_releases_a_slots_held_note_and_sends_it_no_more() {
    let probe = Probe::new();
    let mut rig = Rig::new();
    rig.install(1, Fake::instrument(0.0, 0, &probe));
    rig.set(Command::SelectInstrument(NoteTarget::Slot(1)));
    rig.press(Command::NoteOn(60, 1.0));
    rig.press(Command::SelectInstrument(NoteTarget::Off));
    rig.keep_output();
    rig.press(Command::NoteOn(62, 1.0));
    rig.press(Command::NoteOff(60));
    rig.advance(1024);
    assert_eq!(probe.keys(), vec![(true, 60), (false, 60)], "the held note released at the switch, nothing after it");
    assert!(rig.bus.iter().all(|&x| x == 0.0), "no note sounds");
}

// ── Continuity under a sustained tone ─────────────────────────────────────────────────────────────────

// `tests/seam_continuity.rs`'s criterion on the heard output, the bus silent so it is the slots' wet
// alone: within +-10 ms of a join, the largest |x[n] - x[n-1]| stays within `K` times the steady step
// (the largest one away from every join: the tone's own slope) plus `EPS`, and the window's RMS shows the
// tone is there. The tone is 240 Hz (200 frames a cycle), and every toggle and each install and removal
// starts on its crest, so a cut there steps by the tone's whole amplitude; the fades' ends are
// continuity windows only (the lifecycle tests above hold their frames). What this cannot see: a real plugin's answer to a step at
// its input (an amp sim's gain magnifies it, its filters ring), and whether a step this size is audible
// through one; it is a regression bound, as there.

const TONE_HZ: f64 = 240.0;
const TONE_AMP: f64 = 0.5;
/// +-10 ms at 48 kHz.
const SEAM_WINDOW: Frame = 480;
const K: f64 = 2.0;
const EPS: f64 = 1e-6;
/// The window's RMS floor: the tone over half the window is about 0.25.
const MIN_RMS: f64 = 0.1;

/// The sustained tone at frame `f` (48 kHz).
fn tone(f: Frame) -> f32 {
    (TONE_AMP * (2.0 * PI * TONE_HZ * f as f64 / 48_000.0 + PI / 4.0).sin()) as f32
}

/// The first frame from `f` on the tone's crest: 25 frames into its 200-frame cycle.
fn crest(f: Frame) -> Frame {
    f + (25 - f).rem_euclid(200)
}

/// Each join's largest step and RMS over the heard output kept since `keep_output`, printed, against the
/// steady step away from every join; a failure names each join over its limit.
fn assert_seamless(rig: &Rig, joins: &[(&str, Frame)]) {
    let start = rig.output.as_ref().expect("keep_output first").0;
    let end = start + rig.heard.len() as Frame;
    assert!(rig.bus.iter().all(|&x| x == 0.0), "the bus is silent: what is heard is the slots' wet alone");
    let x = |f: Frame| rig.heard[(f - start) as usize] as f64;
    let near = |f: Frame| joins.iter().any(|&(_, j)| (f - j).abs() <= SEAM_WINDOW);
    let (mut steady, mut peak) = (0.0f64, 0.0f64);
    for f in (start + 1..end).filter(|&f| !near(f)) {
        steady = steady.max((x(f) - x(f - 1)).abs());
        peak = peak.max(x(f).abs());
    }
    let slope = 2.0 * PI * TONE_HZ / rig.sr as f64 * peak;
    assert!(steady > 0.0 && steady <= 1.05 * slope, "steady step {steady:.5} is the tone's (at most {slope:.5})");
    let limit = K * steady + EPS;
    let mut over = Vec::new();
    for &(name, frame) in joins {
        let (lo, hi) = (frame - SEAM_WINDOW, frame + SEAM_WINDOW);
        assert!(lo > start && hi < end, "{name}: the window [{lo}, {hi}] lies in the kept output [{start}, {end})");
        let (mut max_step, mut at, mut sum) = (0.0f64, lo, 0.0f64);
        for f in lo..=hi {
            let step = (x(f) - x(f - 1)).abs();
            if step > max_step {
                (max_step, at) = (step, f);
            }
            sum += x(f) * x(f);
        }
        let rms = (sum / (hi - lo + 1) as f64).sqrt();
        println!("{name}: join frame {frame}, window [{lo}, {hi}], steady step {steady:.5}, window max step {max_step:.5} at frame {at} ({:+}), rms {rms:.3}, limit {limit:.5}", at - frame);
        assert!(rms >= MIN_RMS, "{name}: the window's RMS {rms:.3} is the tone's");
        if max_step > limit {
            over.push(format!("{name}: a step of {max_step:.5} at frame {at} ({:+} from the join at {frame}) over the limit {limit:.5}", at - frame));
        }
    }
    assert!(over.is_empty(), "{}", over.join("; "));
}

/// The tone on slot 0's input, the slot not live and holding `unit` (or nothing), then live on one crest
/// and off on another 24 cycles later: the heard output across both toggles.
fn toggled_live(unit: Option<Box<Fake>>) {
    let mut rig = Rig::new();
    if let Some(unit) = unit {
        rig.install(0, unit);
    }
    rig.set(Command::SetSlotLive(0, false));
    rig.set_input(tone);
    rig.advance(1024);
    rig.keep_output();
    let on = crest(rig.frame + 2 * SEAM_WINDOW);
    let off = on + 4800;
    rig.send_at(on, Command::SetSlotLive(0, true));
    rig.send_at(off, Command::SetSlotLive(0, false));
    rig.advance_to(off + 2 * SEAM_WINDOW);
    assert_seamless(&rig, &[("live on", on), ("live off", off)]);
}

/// GO LIVE on the amp sim while the guitar sustains (STATUS: the next jam opens with it): the live gate
/// ramps the unit's input over 5 ms (STATUS D23), so the effect's output follows it in and out.
#[test]
fn go_live_on_and_off_a_loaded_effect_mid_tone_is_click_free() {
    let probe = Probe::new();
    toggled_live(Some(Fake::effect(0.0, 1.0, &probe)));
    assert!(probe.input_peak() > 0.49, "the effect took the tone while live");
}

/// An empty slot's live flag gates its dry pass-through through the same 5 ms ramp.
#[test]
fn an_empty_slot_toggled_live_mid_tone_is_click_free() {
    toggled_live(None);
}

/// A unit installed into a live slot while the tone sustains crossfades in from the dry input over
/// `FADE`, and its removal crossfades back (`src/slots.rs`: lifecycle never stops the audio). The unit
/// halves its input, so a cut in place of either fade steps by a quarter.
#[test]
fn an_effect_installed_and_removed_mid_tone_crossfades_click_free() {
    let probe = Probe::new();
    let mut rig = Rig::new();
    rig.set_input(tone);
    rig.advance(1024);
    rig.keep_output();
    let installed = crest(rig.frame + 2 * SEAM_WINDOW);
    rig.advance_to(installed);
    rig.install(0, Fake::effect(0.0, 0.5, &probe));
    let removed = installed + 4800;
    rig.advance_to(removed);
    rig.remove(0);
    rig.advance_to(removed + FADE as Frame + 2 * SEAM_WINDOW);
    assert!(rig.returned(0).is_some(), "the unit came back");
    assert_eq!(probe.calls().first().map(|c| c.0), Some(installed), "the install lands on the crest");
    let fade = FADE as Frame;
    assert_seamless(&rig, &[("install", installed), ("install engaged", installed + fade), ("removal", removed), ("removal done", removed + fade)]);
    // The crossfades lead somewhere: the effect's half level is heard between them, the dry tone after.
    let start = rig.output.as_ref().expect("keep_output first").0;
    let heard = |f: Frame| rig.heard[(f - start) as usize];
    for f in installed + fade + 1..removed {
        assert!((heard(f) - 0.5 * tone(f)).abs() < 1e-6, "the effect is heard at {f}: {} for {}", heard(f), 0.5 * tone(f));
    }
    for f in removed + fade + 1..rig.frame {
        assert!((heard(f) - tone(f)).abs() < 1e-6, "the dry tone again at {f}: {} for {}", heard(f), tone(f));
    }
}

// ── GO LIVE's ramp ───────────────────────────────────────────────────────────────────────────────────

/// GO LIVE reversed twice inside its ramp and sent again: each reversal ramps from the gain the gate had
/// reached, over a whole ramp, and a repeat restarts nothing. Slot 0 (input 1.0, so what is heard is its
/// gate) and then slot 1 (input 0.5), at 48 and 44.1 kHz (240 and 221 frames) and block sizes 1, 128 and
/// 1024, bit for bit against [`gate_at`]; no frame steps more than one ramp frame's worth.
#[test]
fn a_live_toggle_reversed_mid_ramp_turns_from_the_gain_it_reached_and_a_repeat_restarts_nothing() {
    for sr in [48_000, 44_100] {
        let n = live_frames(sr);
        for block in [1usize, 128, 1024] {
            let mut rig = Rig::with(Opts { sr, start: sr as Frame, block, ..Default::default() });
            rig.set_inputs(|_| 1.0, |_| 0.5);
            rig.advance(1000);
            let a = rig.frame + 7;
            let slot0 = [(a, false), (a + 100, true), (a + 150, false), (a + 160, false)];
            let b = a + 1000;
            let slot1 = [(b, true), (b + 70, true), (b + 100, false), (b + 130, true)];
            for (at, on) in slot0 {
                rig.send_at(at, Command::SetSlotLive(0, on));
            }
            for (at, on) in slot1 {
                rig.send_at(at, Command::SetSlotLive(1, on));
            }
            rig.keep_output();
            let start = rig.frame;
            rig.advance_to(b + 1000);
            let what = format!("{sr} Hz, block {block}");
            let want = |k: usize| {
                let f = start + k as Frame;
                0.0 + through(1.0, gate_at(true, &slot0, n, f)) + through(0.5, gate_at(false, &slot1, n, f))
            };
            assert_bits(&rig.monitor, want, &what);
            let heard = |f: Frame| rig.monitor[(f - start) as usize];
            let step = rig.monitor.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
            assert!(step <= 1.0 / n as f32 + 1e-6, "{what}: a step of {step}, over one ramp frame's");
            assert_eq!(heard(a), 1.0, "{what}: the ramp's first frame is the outgoing state");
            assert!(heard(a + 100) < heard(a + 99) && heard(a + 101) > heard(a + 100), "{what}: the reversal turns where the gate was");
            assert!(heard(a + 150 + n - 1) > 0.0 && heard(a + 150 + n).to_bits() == 0, "{what}: the repeat at +160 restarted nothing");
            assert!(heard(b + 130 + n - 1) < 0.5 && heard(b + 130 + n) == 0.5, "{what}: slot 1 settles a ramp after its last reversal");
        }
    }
}

/// A stateful effect: it logs each input sample with its frame (into a preallocated log) and rings, its
/// output its state, which decays by [`DECAY`] a frame and takes the input.
struct Ringing {
    log: Arc<Mutex<Vec<(Frame, f32)>>>,
    state: f32,
}

const DECAY: f32 = 0.99;

impl SlotProcessor for Ringing {
    fn kind(&self) -> SlotKind {
        SlotKind::Effect
    }

    fn latency(&self) -> Frame {
        0
    }

    fn process(&mut self, frame: Frame, input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
        let mut log = self.log.lock().unwrap();
        for (k, (y, &x)) in out.iter_mut().zip(input).enumerate() {
            if log.len() < LOG {
                log.push((frame + k as Frame, x));
            }
            *y = self.state;
            self.state = self.state * DECAY + x;
        }
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// GO LIVE off gates an effect's input, never its output: the effect is fed its input ramped down and
/// then exact silence from a ramp after the command's frame, and what it was ringing with rings on,
/// decaying by its own law.
#[test]
fn going_off_live_ramps_an_effects_input_to_silence_and_leaves_its_tail_ringing() {
    for block in [1usize, 128] {
        let log = Arc::new(Mutex::new(Vec::with_capacity(LOG)));
        let mut rig = Rig::with(Opts { block, ..Default::default() });
        rig.set_level(0.01);
        rig.install(0, Box::new(Ringing { log: log.clone(), state: 0.0 }));
        rig.advance(4800);
        let off = rig.frame + 3;
        rig.send_at(off, Command::SetSlotLive(0, false));
        rig.keep_output();
        let start = rig.frame;
        rig.advance(2000);
        let fed: Vec<(Frame, f32)> = log.lock().unwrap().iter().copied().filter(|&(f, _)| f >= start).collect();
        assert_eq!(fed.len(), 2000, "block {block}: every frame reached the effect once");
        for (f, x) in fed {
            let want = live(0.01, true, &[(off, false)], f);
            assert_eq!(x.to_bits(), want.to_bits(), "block {block}: the effect's input at {f}: {x} for {want}");
            if f >= off + LIVE {
                assert_eq!(x.to_bits(), 0, "block {block}: exact silence from the ramp's end");
            }
        }
        let heard = |f: Frame| rig.monitor[(f - start) as usize];
        for f in off + LIVE + 1..start + 2000 {
            assert_eq!(heard(f).to_bits(), (heard(f - 1) * DECAY).to_bits(), "block {block}: the tail decays by its own law at {f}");
        }
        assert!(heard(off + LIVE + 500) > 1e-3, "block {block}: the tail still rings: {}", heard(off + LIVE + 500));
    }
}

/// A live toggle inside each 10 ms bypass crossfade: the crossfade runs its frames as ever, its dry side
/// the gated input, and the gate splits the plugin's calls only at the commands, never where a ramp
/// ends. The removal completes as ever.
#[test]
fn a_live_toggle_inside_each_bypass_crossfade_composes_with_it_and_adds_no_plugin_call() {
    let probe = Probe::new();
    let mut rig = Rig::new();
    rig.set_level(1.0);
    rig.advance(256);
    let installed = rig.frame;
    rig.install(0, Fake::effect(0.25, 0.0, &probe));
    let off = installed + 100;
    rig.send_at(off, Command::SetSlotLive(0, false));
    rig.keep_output();
    rig.advance(1024);
    let toggles = [(off, false), (installed + 1024 + 50, true)];
    let dry = |f: Frame| live(1.0, true, &toggles, f);
    assert_bits(&rig.monitor, |k| faded(dry(installed + k as Frame), 0.25, k.min(FADE)), "fading in, the dry side ramping out");

    let removed = rig.frame;
    assert_eq!(removed + 50, toggles[1].0);
    rig.remove(0);
    rig.send_at(toggles[1].0, Command::SetSlotLive(0, true));
    rig.keep_output();
    rig.advance(1024);
    assert_bits(&rig.monitor, |k| faded(dry(removed + k as Frame), 0.25, FADE.saturating_sub(k)), "fading out, the dry side ramping in");
    assert!(rig.returned(0).is_some(), "the unit came back");
    assert_eq!(probe.stops(), 1, "stopped once");

    let splits = [off, toggles[1].0];
    let mut want = Vec::new();
    let mut at = installed;
    while at < removed + FADE as Frame {
        let end = at + 128;
        let mut from = at;
        for &x in splits.iter().filter(|&&x| x > at && x < end) {
            want.push((from, (x - from) as usize));
            from = x;
        }
        want.push((from, (end - from) as usize));
        at = end;
    }
    assert_eq!(probe.calls(), want, "one call per block, split at each command alone");
}

/// GO LIVE off on a dry slot mid-take, beside a latent live effect: heard, the ramp is on its own
/// frames; recorded, it is the input gated at its input frame and delayed by the latched latency.
#[test]
fn a_live_ramp_in_a_take_is_heard_on_its_frames_and_recorded_at_its_latched_delay() {
    const L: Frame = 480;
    let mut rig = Rig::with(Opts { align: PHYS + LIMITER, ..Default::default() });
    rig.install(0, Box::new(Delay::new(L)));
    rig.set(Command::SetSlotLive(1, true));
    rig.set_inputs(|_| 0.0, code);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    rig.press(Command::RecDub(0));
    let start = rig.start_frame();
    rig.advance_to(start + 1000);
    let off = rig.frame + 3;
    rig.send_at(off, Command::SetSlotLive(1, false));
    rig.keep_output();
    let from = rig.frame;
    rig.advance_to(rig.end_frame() + 1);
    assert_eq!(rig.state(0), LaneState::Playing);
    let toggles = [(off, false)];
    assert_bits(&rig.monitor, |k| 0.0 + live(code(from + k as Frame), true, &toggles, from + k as Frame), "heard: the dry slot gated on its own frames");
    let pcm = rig.pcm(0);
    assert_bits(&pcm, |k| 0.0 + live(code(start + k as Frame - L), true, &toggles, start + k as Frame - L), "recorded: gated at the input frame, then L late");
    assert!(pcm[(off + L - start) as usize] > 0.0 && pcm[(off + L + LIVE - start) as usize] == 0.0, "the ramp lands L late in the take");
}

/// GO LIVE sent before the engine renders its first frame sets the starting state, settled; from the
/// first rendered frame on a toggle ramps, one inside the first block too. Block sizes 1 and 1024.
#[test]
fn a_live_state_set_before_the_first_frame_is_settled_and_a_toggle_in_the_first_block_ramps() {
    let mut runs = Vec::new();
    for block in [1usize, 1024] {
        let mut rig = Rig::with(Opts { block, ..Default::default() });
        let t = rig.frame;
        rig.set_inputs(|_| 1.0, |_| 0.5);
        rig.send_at(t, Command::SetSlotLive(1, true));
        rig.send_at(t + 10, Command::SetSlotLive(0, false));
        rig.send_at(t + 500, Command::SetSlotLive(1, false));
        rig.keep_output();
        rig.advance(1024);
        assert_eq!(rig.monitor[0], 1.5, "block {block}: both slots live from the first frame");
        assert_eq!((rig.monitor[10], rig.monitor[10 + LIVE as usize]), (1.5, 0.5), "block {block}: slot 0 ramps out from frame 10");
        let want = |k: usize| {
            let f = t + k as Frame;
            0.0 + live(1.0, true, &[(t + 10, false)], f) + live(0.5, true, &[(t + 500, false)], f)
        };
        assert_bits(&rig.monitor, want, &format!("block {block}"));
        runs.push(std::mem::take(&mut rig.monitor));
    }
    assert!(runs[0].iter().zip(&runs[1]).all(|(a, b)| a.to_bits() == b.to_bits()));
}

// ── With no device running ────────────────────────────────────────────────────────────────────────────

#[test]
fn idle_servicing_installs_engaged_and_removes_at_once_releasing_its_notes() {
    let (a, b) = (Probe::new(), Probe::new());
    let mut rig = Rig::new();
    rig.set_level(1.0);
    rig.advance(256);
    rig.install(0, Fake::effect(0.25, 0.0, &a));
    rig.install(1, Fake::instrument(0.0, 0, &b));
    rig.engine.service_slots_idle();
    assert_eq!((rig.engine.slot(0), rig.engine.slot(1)), (Some((SlotKind::Effect, 0)), Some((SlotKind::Instrument, 0))));
    rig.keep_output();
    rig.press(Command::SelectInstrument(NoteTarget::Slot(1)));
    rig.press(Command::NoteOn(60, 1.0));
    rig.advance(254);
    assert_bits(&rig.monitor, |_| 0.25, "engaged without a fade");

    rig.remove(0);
    rig.remove(1);
    let next = rig.frame;
    rig.engine.service_slots_idle();
    assert!(rig.returned(0).is_some() && rig.returned(1).is_some(), "back at once");
    assert_eq!((a.stops(), b.stops()), (1, 1));
    let seen = b.seen();
    let n = seen.len();
    let off = Seen::Event(SlotEvent { offset: 0, kind: SlotEventKind::NoteOff { key: 60 } });
    assert_eq!(seen[n - 3..], [Seen::Call { frame: next, len: 1 }, off, Seen::Stop], "one silent frame carries the release");
    rig.keep_output();
    rig.advance(128);
    assert_bits(&rig.monitor, |_| 1.0, "bypassed without a fade");
}

#[test]
fn eviction_hands_every_unit_back_stopped_a_waiting_install_too() {
    let (a, b) = (Probe::new(), Probe::new());
    let mut rig = Rig::new();
    rig.install(0, Fake::instrument(0.0, 0, &a));
    rig.press(Command::SelectInstrument(NoteTarget::Slot(0)));
    rig.press(Command::NoteOn(60, 1.0));
    rig.advance(128);
    rig.install(1, Fake::effect(0.0, 1.0, &b)); // no block serviced it yet
    rig.engine.evict_slots();
    assert!(rig.returned(0).is_some() && rig.returned(1).is_some());
    assert_eq!((a.stops(), b.stops()), (1, 1));
    assert_eq!(a.keys(), vec![(true, 60), (false, 60)], "the held note released before the stop");
    assert_eq!(a.seen().last(), Some(&Seen::Stop));
    assert_eq!((rig.engine.slot(0), rig.engine.slot(1)), (None, None));
}

// ── Non-finite output ────────────────────────────────────────────────────────────────────────────────

const NON_FINITE: [f32; 3] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

fn finite(x: &[f32], what: &str) {
    assert_eq!(x.iter().position(|v| !v.is_finite()), None, "{what}: a non-finite frame");
}

/// A live effect that renders NaN or an infinity while lane 0 overdubs, with the ECHO send on: what is
/// heard and recorded stays finite, the dub adds nothing to the loop, and once the unit is gone the echo
/// and the loop play on finite.
#[test]
fn an_effects_non_finite_output_reaches_neither_the_mix_nor_the_echo_nor_a_loop() {
    for bad in NON_FINITE {
        let probe = Probe::new();
        let mut rig = Rig::with(Opts { sr: 8000, start: 8000, ..Default::default() });
        rig.set(Command::SetBpm(200.0));
        rig.set_input(code);
        let master = rig.record_first_take(0, 1, 240);
        rig.set_level(0.0);
        rig.idle();
        // On from here, with nothing to echo: whatever the dub records is the unit's.
        rig.set(Command::SetInputSendParam(InputSendParam::EchoLevel, 0.5));
        rig.set(Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.5));
        rig.set(Command::SetInputSend(InputSend::Echo, true));
        let before = rig.pcm(0);
        rig.install(0, Fake::effect(bad, 0.0, &probe));
        rig.advance(1000);
        rig.keep_output();
        rig.advance_to(rig.next_boundary() + master / 4);
        rig.press(Command::RecDub(0));
        assert_eq!(rig.state(0), LaneState::Overdubbing);
        rig.advance(2 * master);
        rig.press(Command::RecDub(0));
        assert_eq!(rig.state(0), LaneState::Playing);
        rig.idle();
        for (x, what) in [(&rig.heard, "heard"), (&rig.heard_right, "heard right"), (&rig.monitor, "monitor"), (&rig.record, "record tap")] {
            finite(x, &format!("{bad}: {what}"));
        }
        assert_eq!(rig.pcm(0), before, "{bad}: the dub adds nothing to the loop");
        assert!(rig.engine.diag().slot_protocol_errors > 0, "{bad}: counted");

        rig.remove(0);
        rig.advance(1000);
        assert!(rig.returned(0).is_some());
        rig.set_level(0.25);
        rig.keep_output();
        rig.advance(master);
        finite(&rig.heard, &format!("{bad}: heard after the unit left"));
        assert!(peak(&rig.monitor) > 0.2, "{bad}: the dry input is heard again");
        assert!(peak(&rig.heard) > 0.0, "{bad}: the loop plays on");
    }
}

/// A live effect whose block is sound but for its last frame: that one frame is caught as the whole block
/// would be, so the scan reads every sample, not the first.
#[test]
fn a_non_finite_frame_late_in_a_sound_block_is_caught_too() {
    for bad in NON_FINITE {
        let probe = Probe::new();
        let mut rig = Rig::new();
        rig.set_input(code);
        rig.set_level(0.25);
        rig.install(0, Fake::effect(0.0, 1.0, &probe).with_last(bad));
        rig.advance(1000);
        rig.keep_output();
        rig.advance(4000);
        for (x, what) in [(&rig.heard, "heard"), (&rig.monitor, "monitor"), (&rig.record, "record tap")] {
            finite(x, &format!("{bad}: {what}"));
        }
        assert!(rig.engine.diag().slot_protocol_errors > 0, "{bad}: counted");
    }
}

/// An instrument that renders NaN or an infinity into the master bus: the limiter and the output stay
/// finite, and once the unit is gone the dry input is heard again.
#[test]
fn an_instruments_non_finite_output_never_reaches_the_bus_or_the_limiter() {
    for bad in NON_FINITE {
        let probe = Probe::new();
        let mut rig = Rig::new();
        rig.install(1, Fake::instrument(bad, 0, &probe));
        rig.keep_output();
        rig.advance(4800);
        finite(&rig.bus, &format!("{bad}: bus"));
        finite(&rig.heard, &format!("{bad}: heard"));
        rig.remove(1);
        rig.advance(1000);
        assert!(rig.returned(1).is_some());
        rig.set_level(0.25);
        rig.keep_output();
        rig.advance(4800);
        finite(&rig.heard, &format!("{bad}: heard after the unit left"));
        assert!(peak(&rig.heard) > 0.2, "{bad}: the dry input is heard again");
    }
}

// ── Block-size independence ──────────────────────────────────────────────────────────────────────────

/// An effect (the rig's Delay) and an instrument fake installed and removed mid-run, notes and the
/// slots' live flags and gains stamped mid-block: the output, bit for bit. The live toggles ramp: one
/// inside the effect's install fade, one inside the instrument's removal fade that the slot's emptying
/// crosses, and one reversed twice inside its ramp.
fn session(block: usize) -> [Vec<f32>; 2] {
    let probe = Probe::new();
    let mut rig = Rig::with(Opts { block, ..Default::default() });
    rig.set_input(|f| 0.4 * (f as f32 * 0.013).sin());
    let t = rig.frame;
    let script = [
        (1_000, Command::SelectInstrument(NoteTarget::Slot(1))),
        (3_001, Command::NoteOn(60, 1.0)),
        (7_777, Command::NoteOn(62, 0.5)),
        (9_000, Command::NoteOff(60)),
        (12_345, Command::SetSlotGain(0, 0.7)),
        (13_579, Command::SetSlotGain(1, 1.5)),
        (5_101, Command::SetSlotLive(0, false)),
        (5_299, Command::SetSlotLive(0, true)),
        (21_011, Command::NoteOn(64, 1.0)),
        (30_011, Command::SetSlotLive(0, false)),
        (30_500, Command::SetSlotLive(1, true)),
        (35_300, Command::SetSlotLive(1, false)),
        (40_009, Command::SetSlotLive(0, true)),
        (40_100, Command::SetSlotLive(0, false)),
        (40_157, Command::SetSlotLive(0, true)),
    ];
    for (at, command) in script {
        rig.send_at(t + at, command);
    }
    rig.keep_output();
    rig.advance_to(t + 5_000);
    rig.install(0, Box::new(Delay::new(PLUGIN)));
    rig.install(1, Fake::instrument(0.1, 33, &probe));
    rig.advance_to(t + 20_000);
    rig.remove(0);
    rig.advance_to(t + 35_000);
    rig.remove(1);
    rig.advance_to(t + 50_000);
    assert!(rig.returned(0).is_some() && rig.returned(1).is_some());
    assert!(peak(&rig.heard) > 0.1);
    [std::mem::take(&mut rig.heard), std::mem::take(&mut rig.heard_right)]
}

#[test]
fn the_slots_render_bit_identical_across_block_sizes() {
    let reference = session(128);
    for block in [1, 64, 127, 480] {
        for (side, (got, want)) in ["left", "right"].iter().zip(session(block).iter().zip(&reference)) {
            let first = got.iter().zip(want).position(|(a, b)| a.to_bits() != b.to_bits());
            assert_eq!(first, None, "block {block}: the {side} output differs from block 128");
        }
    }
}
