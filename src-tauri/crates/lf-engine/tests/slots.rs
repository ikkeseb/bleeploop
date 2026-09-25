//! The plugin slot rack (`src/slots.rs`) driven through the engine: install and removal through the
//! ports with their bypass crossfades, the engine never dropping a unit, the notes a slot holds, one
//! plugin call per block, the live flag and gain, where each output goes and where it is recorded, and
//! the whole of it bit-identical at any block size. New with the engine: the web app's slots
//! (`plugin-bridge.ts`, `instrument-slots.ts`) had no rig guard to port.
//!
//! The fakes record what they saw in preallocated buffers and atomics, so every `process` still runs
//! under the rig's `assert_no_alloc`.

mod common;

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

use common::{Delay, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Command, Instrument, LaneState, NoteTarget, SlotEvent, SlotEventKind, SlotKind, SlotProcessor};

/// Frames of the bypass crossfade at 48 kHz: 10 ms.
const FADE: usize = 480;
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
    probe: Probe,
}

impl Fake {
    fn effect(level: f32, through: f32, probe: &Probe) -> Box<Fake> {
        Box::new(Fake { id: 0, kind: SlotKind::Effect, latency: 0, level, through, impulse: None, probe: probe.clone() })
    }

    fn instrument(level: f32, latency: Frame, probe: &Probe) -> Box<Fake> {
        Box::new(Fake { id: 0, kind: SlotKind::Instrument, latency, level, through: 0.0, impulse: None, probe: probe.clone() })
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

    rig.set(Command::SetSlotLive(0, false));
    rig.keep_output();
    rig.advance(1000);
    assert!(rig.monitor.iter().all(|&x| x == 0.0), "empty and not live: silent");

    let probe = Probe::new();
    rig.install(0, Fake::effect(0.0, 1.0, &probe));
    rig.advance(4800);
    assert_eq!(probe.input_peak(), 0.0, "an effect in a slot that is not live gets silence");
    rig.set(Command::SetSlotLive(0, true));
    rig.advance(1000);
    assert!(probe.input_peak() > 0.4, "live, it gets the input");
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

// ── Block-size independence ──────────────────────────────────────────────────────────────────────────

/// An effect (the rig's Delay) and an instrument fake installed and removed mid-run, notes and the
/// slots' live flags and gains stamped mid-block: the output, bit for bit.
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
        (21_011, Command::NoteOn(64, 1.0)),
        (30_011, Command::SetSlotLive(0, false)),
        (30_500, Command::SetSlotLive(1, true)),
        (40_009, Command::SetSlotLive(0, true)),
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
