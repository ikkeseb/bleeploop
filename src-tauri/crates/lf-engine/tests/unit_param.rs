//! Unit parameters (`Command::SetUnitParam`, `Event::UnitParam`, `UnitParam`,
//! `Engine::unsent_unit_params`, `Engine::unapplied_unit_commands`): sample-accurate application and
//! clamping across block sizes, unknown-param and unseated-id drops, full-ring folding into the fixed
//! per-seat owed table (commuting past waiting bypasses) and delivery on drain, peek of unapplied unit
//! commands in send order, and zero allocations on the audio thread.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;

use assert_no_alloc::assert_no_alloc;
use common::{violation_count, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{
    Action, Command, Engine, EngineConfig, Event, InputSendParam, Instrument, ProcessContext,
    SlotEvent, TimedCommand, UnitCx, UnitId, UnitInfo, UnitKind, UnitParam, UnitProcessor,
    MAX_UNITS,
};

const BLOCKS: [usize; 7] = [1, 32, 64, 127, 128, 480, 1024];
const F0: Frame = 48_000;

/// A clamping test unit: takes `UnitParam::Send(InputSendParam::EchoLevel)`, clamps it to `0.0..=1.0`
/// (so 2.0 answers 1.0), multiplies its input by that level, and answers `None` for any other param.
struct ClampGain {
    level: f64,
    drops: Arc<AtomicUsize>,
}

impl ClampGain {
    fn new(level: f64, drops: &Arc<AtomicUsize>) -> Box<Self> {
        Box::new(ClampGain { level, drops: drops.clone() })
    }
}

impl UnitProcessor for ClampGain {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Echo, source: false, latency: 0 }
    }

    fn process(&mut self, _cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let g = self.level as f32;
        for (o, i) in out.into_iter().zip(input) {
            for (y, &x) in o.iter_mut().zip(i) {
                *y = x * g;
            }
        }
    }

    fn set_param(&mut self, _cx: &UnitCx, param: UnitParam, value: f64) -> Option<f64> {
        if param == UnitParam::Send(InputSendParam::EchoLevel) {
            let clamped = value.clamp(0.0, 1.0);
            self.level = clamped;
            Some(clamped)
        } else {
            None
        }
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

impl Drop for ClampGain {
    fn drop(&mut self) {
        self.drops.fetch_add(1, SeqCst);
    }
}

fn param_events(rig: &Rig) -> Vec<(Frame, UnitId, UnitParam, f64)> {
    rig.events
        .iter()
        .filter_map(|e| match *e {
            Event::UnitParam { frame, unit, param, value } => Some((frame, unit, param, value)),
            _ => None,
        })
        .collect()
}

/// Every `UnitParam` key round-trips through `key()` / `from_key()`, including `instrument` and the
/// six `gain<Name>` keys (`gainDrum` matches `docs/unit-contract.md` line 60), and `index()` matches `ALL`.
#[test]
fn unit_param_keys_and_indices_round_trip() {
    assert_eq!(UnitParam::COUNT, 17);
    assert_eq!(UnitParam::ALL.len(), 17);
    for (i, p) in UnitParam::ALL.into_iter().enumerate() {
        assert_eq!(p.index(), i, "{p:?}");
        assert_eq!(UnitParam::from_key(p.key()), Some(p), "round-trip {}", p.key());
    }
    assert_eq!(UnitParam::Instrument.key(), "instrument");
    assert_eq!(UnitParam::Gain(Instrument::Lead).key(), "gainLead");
    assert_eq!(UnitParam::Gain(Instrument::Bass).key(), "gainBass");
    assert_eq!(UnitParam::Gain(Instrument::Pad).key(), "gainPad");
    assert_eq!(UnitParam::Gain(Instrument::Piano).key(), "gainPiano");
    assert_eq!(UnitParam::Gain(Instrument::Organ).key(), "gainOrgan");
    assert_eq!(UnitParam::Gain(Instrument::Drums).key(), "gainDrum");
    assert_eq!(UnitParam::from_key("gainDrums"), None);
    assert_eq!(UnitParam::from_key("nope"), None);
}

/// `SetUnitParam` applies on its exact frame, answers with the clamped value (2.0 -> 1.0) on that
/// frame, and changes the audio output starting on that frame, bit-identical across every block size.
#[test]
fn set_unit_param_clamps_and_applies_sample_accurately_across_block_sizes() {
    let mut first: Option<Vec<f32>> = None;
    for block in BLOCKS {
        let what = format!("block {block}");
        let drops = Arc::new(AtomicUsize::new(0));
        let mut rig = Rig::with(Opts { block, ..Opts::default() });
        rig.set_level(0.5);
        assert!(rig.port(0).install_at(1, 1, 0, false, None, ClampGain::new(0.25, &drops)).is_ok());
        rig.advance(1000);
        let from = rig.frame;
        let t = from + 137;
        rig.keep_output();
        rig.send_at(t, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 2.0));
        rig.advance(1024);

        let evs = param_events(&rig);
        assert_eq!(evs, vec![(t, 1, UnitParam::Send(InputSendParam::EchoLevel), 1.0)], "{what}: clamped answer on exact frame");
        assert_eq!(rig.engine.unsent_unit_params().count(), 0, "{what}: delivered, nothing owed");

        // Every frame before `t` hears 0.5 * 0.25 = 0.125; from `t` on, 0.5 * 1.0 = 0.5.
        for (k, &y) in rig.monitor.iter().enumerate() {
            let f = from + k as Frame;
            let want: f32 = if f < t { 0.125 } else { 0.5 };
            assert_eq!(y.to_bits(), want.to_bits(), "{what}: frame {f} (t={t})");
        }
        let baseline = first.get_or_insert_with(|| rig.monitor.clone());
        assert_eq!(&rig.monitor, baseline, "{what}: bit-identical across block sizes");

        rig.engine.evict_slots();
        let back = rig.port(0).returned_unit().expect("evicted");
        assert_eq!(drops.load(SeqCst), 0);
        drop(back);
        assert_eq!(drops.load(SeqCst), 1);
    }
}

/// Another kind's param (`set_param` answers `None`) and an unseated id (including a withdrawn seat)
/// are dropped without emitting `Event::UnitParam`, each counted on its own counter.
#[test]
fn unknown_param_and_unseated_id_are_dropped_and_counted() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut rig = Rig::with(Opts { block: 64, ..Opts::default() });
    assert!(rig.port(0).install_at(1, 1, 0, false, None, ClampGain::new(0.5, &drops)).is_ok());
    rig.advance(1000);

    let t = rig.frame + 13;
    // Another kind's params -> unit_params_dropped.
    rig.send_at(t, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::ReverbLevel), 0.7));
    rig.send_at(t + 1, Command::SetUnitParam(1, UnitParam::Gain(Instrument::Drums), 0.8));
    // Unseated id -> unit_commands_dropped.
    rig.send_at(t + 2, Command::SetUnitParam(99, UnitParam::Send(InputSendParam::EchoLevel), 0.5));
    rig.advance(256);

    assert!(param_events(&rig).is_empty(), "dropped commands emit no Event::UnitParam");
    assert_eq!(rig.engine.unsent_unit_params().count(), 0);
    let d = rig.engine.diag();
    assert_eq!(d.unit_params_dropped, 2, "both wrong-kind params counted");
    assert_eq!(d.unit_commands_dropped, 1, "unseated id counted");

    // Withdrawn seat has no unit to take the param -> counted as unseated.
    assert!(rig.port(0).withdraw(1));
    rig.advance(1000);
    let back = rig.port(0).returned_unit().expect("withdrawn");
    rig.send_at(rig.frame + 5, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.5));
    rig.advance(256);
    assert!(param_events(&rig).is_empty());
    assert_eq!(rig.engine.diag().unit_commands_dropped, 2, "withdrawn seat has no unit");
    assert_eq!(rig.engine.diag().unit_params_dropped, 2);
    drop(back);
}

/// A full event ring folds owed param answers per seat and param: sending two values keeps only the
/// later clamped value in `unsent_unit_params` (and a param command does not wait behind a waiting
/// bypass command); once the ring frees, the owed answer is delivered and `unsent_unit_params` empties.
#[test]
fn full_event_ring_folds_param_in_unsent_unit_params_and_delivers_when_free() {
    for block in [64, 1024] {
        let what = format!("block {block}");
        let drops = Arc::new(AtomicUsize::new(0));
        let mut rig = Rig::with_event_capacity(Opts { block, ..Opts::default() }, 4);
        rig.set_level(1.0);
        assert!(rig.port(0).install_at(1, 1, 0, false, None, ClampGain::new(0.25, &drops)).is_ok());
        rig.advance(1000);

        // Stall the event ring and fill the slot's owed queue with bypasses so bypass commands wait.
        rig.hold_events = true;
        let t0 = rig.frame + 5;
        for k in 0..40 {
            rig.send_at(t0, Command::SetUnitBypass(1, k % 2 == 0));
        }
        // Send two param values after those waiting bypasses: they commute (apply immediately) and
        // fold in the per-seat owed table to the later clamped value (2.0 -> 1.0).
        let t1 = t0 + 10;
        let t2 = t0 + 20;
        rig.send_at(t1, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.4));
        rig.send_at(t2, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 2.0));
        rig.advance(block as Frame * 2);

        let owed: Vec<_> = rig.engine.unsent_unit_params().collect();
        assert_eq!(
            owed,
            vec![(1, UnitParam::Send(InputSendParam::EchoLevel), 1.0)],
            "{what}: later clamped value owed while ring is full (commuted past waiting bypasses)"
        );

        // Free the ring and drain: the owed answer arrives with the later frame and clamped value.
        rig.hold_events = false;
        rig.read_events();
        for _ in 0..64 {
            rig.advance(block as Frame);
        }
        assert_eq!(rig.engine.unsent_unit_params().count(), 0, "{what}: drained");
        let evs = param_events(&rig);
        assert_eq!(
            evs,
            vec![(t2, 1, UnitParam::Send(InputSendParam::EchoLevel), 1.0)],
            "{what}: folded answer delivered once ring freed"
        );

        rig.engine.evict_slots();
        let back = rig.port(0).returned_unit().expect("evicted");
        drop(back);
    }
}

/// `unapplied_unit_commands` covers `SetUnitParam` among `SetUnitBypass` and `ToggleUnit` (both forms)
/// in send order, across both the pending table and the command ring, without consuming any command.
#[test]
fn unapplied_unit_commands_includes_params_among_bypasses_in_send_order() {
    for block in [64, 1024] {
        let what = format!("block {block}");
        let drops = Arc::new(AtomicUsize::new(0));
        let mut rig = Rig::with_event_capacity(Opts { block, ..Opts::default() }, 4);
        assert!(rig.port(0).install_at(1, 1, 0, false, None, ClampGain::new(0.25, &drops)).is_ok());
        rig.advance(1000);

        // Queue commands stamped ahead (in `pending`) mixed with waiting bypasses and commands in the ring.
        rig.hold_events = true;
        let t_now = rig.frame + 7;
        for k in 0..40 {
            rig.send_at(t_now, Command::SetUnitBypass(1, k % 2 == 0));
        }
        // Stamped in the future so these stay in `pending` in send order.
        let future = rig.frame + 10_000;
        let in_table = [
            Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.3),
            Command::SetUnitBypass(1, true),
            Command::SetMetronome(false), // non-unit: must be excluded
            Command::Action(Action::ToggleUnit(1)),
            Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.6),
            Command::ActionOn(2, Action::ToggleUnit(1)),
        ];
        for c in in_table {
            rig.send_at(future, c);
        }
        rig.advance(block as Frame);

        // Now queue more directly into the command ring (not yet popped into `pending`).
        let in_ring = [
            Command::SetMetronome(true), // non-unit: must be excluded
            Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.9),
            Command::SetUnitBypass(1, false),
            Command::ActionOn(0, Action::ToggleUnit(1)),
        ];
        for c in in_ring {
            rig.queue(c);
        }

        let peeked: Vec<Command> = rig.engine.unapplied_unit_commands().collect();
        let tail_want = vec![
            in_table[0],
            in_table[1],
            in_table[3],
            in_table[4],
            in_table[5],
            in_ring[1],
            in_ring[2],
            in_ring[3],
        ];
        assert!(peeked.len() > tail_want.len(), "{what}: waiting bypasses precede the tail: {peeked:?}");
        assert_eq!(&peeked[peeked.len() - tail_want.len()..], &tail_want[..], "{what}: exact send order");
        assert!(peeked.iter().all(|c| c.unit() == Some(1)), "{what}: only unit commands");
        assert_eq!(rig.engine.unapplied_unit_commands().count(), peeked.len(), "{what}: peek consumes nothing");

        // Free the ring and advance past `future`: everything applies.
        rig.hold_events = false;
        rig.read_events();
        rig.advance_to(future + 4 * block as Frame);
        for _ in 0..32 {
            rig.advance(block as Frame);
        }
        assert_eq!(rig.engine.unapplied_unit_commands().count(), 0, "{what}: all applied");
        assert_eq!(param_events(&rig).last().map(|e| e.3), Some(0.6), "{what}: stamped future param applied last");

        rig.engine.evict_slots();
        let back = rig.port(0).returned_unit().expect("evicted");
        drop(back);
    }
}

/// Applying `SetUnitParam` (both delivered and folded on a full ring) and offering its owed answer
/// perform zero heap allocations on the audio thread across every block size.
#[test]
fn set_unit_param_and_owed_offer_do_not_allocate_at_all_block_sizes() {
    for block in BLOCKS {
        let what = format!("block {block}");
        let drops = Arc::new(AtomicUsize::new(0));
        let mut cfg = EngineConfig::new(48_000);
        cfg.max_loop_seconds = 1.0;
        cfg.event_capacity = 4;
        let (mut engine, mut handle) = Engine::new(cfg);
        assert!(handle.slots[0].install_at(1, 1, MAX_UNITS, false, None, ClampGain::new(0.25, &drops)).is_ok());

        let input = vec![0.5f32; block];
        let (mut left, mut right) = (vec![0.0f32; block], vec![0.0f32; block]);
        let mut frame = F0;
        let align = -engine.limiter_latency();

        // Seat the unit and drain initial events.
        for _ in 0..4 {
            let ctx = ProcessContext { frame, xrun: false, damaged: false, align_frames: align, input_frames: 0 };
            let v = violation_count();
            assert_no_alloc(|| engine.process(&ctx, &input, &mut left, &mut right));
            assert_eq!(violation_count(), v, "{what}: seat");
            frame += block as Frame;
            while handle.events.pop().is_ok() {}
        }

        // Fill the 4-slot event ring without reading it, then send two SetUnitParam commands so they
        // fold into the fixed per-seat table under `assert_no_alloc`.
        for k in 0..6 {
            let _ = handle.commands.push(TimedCommand { frame: None, command: Command::SetUnitBypass(1, k % 2 == 0) });
        }
        let _ = handle.commands.push(TimedCommand { frame: Some(frame), command: Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.3) });
        let _ = handle.commands.push(TimedCommand { frame: Some(frame), command: Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 2.0) });

        for _ in 0..4 {
            let ctx = ProcessContext { frame, xrun: false, damaged: false, align_frames: align, input_frames: 0 };
            let v = violation_count();
            assert_no_alloc(|| engine.process(&ctx, &input, &mut left, &mut right));
            assert_eq!(violation_count(), v, "{what}: apply & fold param under full ring");
            frame += block as Frame;
        }
        assert_eq!(engine.unsent_unit_params().collect::<Vec<_>>(), vec![(1, UnitParam::Send(InputSendParam::EchoLevel), 1.0)], "{what}");

        // Drain the ring and offer the owed param answer under `assert_no_alloc`.
        let mut got_param = Vec::new();
        for _ in 0..16 {
            while let Ok(ev) = handle.events.pop() {
                if let Event::UnitParam { unit, param, value, .. } = ev {
                    got_param.push((unit, param, value));
                }
            }
            let ctx = ProcessContext { frame, xrun: false, damaged: false, align_frames: align, input_frames: 0 };
            let v = violation_count();
            assert_no_alloc(|| engine.process(&ctx, &input, &mut left, &mut right));
            assert_eq!(violation_count(), v, "{what}: offer owed param");
            frame += block as Frame;
        }
        while let Ok(ev) = handle.events.pop() {
            if let Event::UnitParam { unit, param, value, .. } = ev {
                got_param.push((unit, param, value));
            }
        }
        assert_eq!(got_param, vec![(1, UnitParam::Send(InputSendParam::EchoLevel), 1.0)], "{what}");
        assert_eq!(engine.unsent_unit_params().count(), 0, "{what}");

        engine.evict_slots();
        let back = handle.slots[0].returned_unit().expect("evicted");
        drop(back);
    }
}

/// A same-id reload while the event ring is full keeps the seat's owed `UnitParam` answer:
/// `unsent_unit_params` still lists it after the reload applies, and `Event::UnitParam` arrives once
/// the ring frees.
#[test]
fn same_id_reload_keeps_owed_unit_param_until_ring_frees() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut rig = Rig::with_event_capacity(Opts { block: 64, ..Opts::default() }, 4);
    assert!(rig.port(0).install_at(1, 10, 0, false, None, ClampGain::new(0.25, &drops)).is_ok());
    rig.advance(1000);

    // Fill the 4-slot event ring while leaving room in `s.owed` so the reload can apply.
    rig.hold_events = true;
    let t0 = rig.frame + 5;
    for k in 0..6 {
        rig.send_at(t0, Command::SetUnitBypass(1, k % 2 == 0));
    }
    let t_param = t0 + 10;
    rig.send_at(t_param, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 2.0));
    rig.advance(128);
    assert_eq!(
        rig.engine.unsent_unit_params().collect::<Vec<_>>(),
        vec![(1, UnitParam::Send(InputSendParam::EchoLevel), 1.0)],
        "param answer owed while event ring is full"
    );

    // Apply a same-id reload while the event ring is still full.
    let ticket = rig
        .port(0)
        .install_at(1, 11, 0, false, Some(1), ClampGain::new(0.5, &drops))
        .ok()
        .expect("ring has room");
    rig.advance(1000);
    assert!(ticket.answered(), "same-id reload applied");
    let old = rig.port(0).returned_unit().expect("old unit returned");
    assert_eq!((old.id, old.token, old.left), (1, 10, lf_engine::Left::Replaced));

    // Owed param answer must survive the same-id reload.
    assert_eq!(
        rig.engine.unsent_unit_params().collect::<Vec<_>>(),
        vec![(1, UnitParam::Send(InputSendParam::EchoLevel), 1.0)],
        "same-id reload preserves owed param answers"
    );

    // Free the ring and drain: the owed answer arrives.
    rig.hold_events = false;
    rig.read_events();
    for _ in 0..32 {
        rig.advance(64);
    }
    assert_eq!(rig.engine.unsent_unit_params().count(), 0, "drained");
    assert_eq!(
        param_events(&rig),
        vec![(t_param, 1, UnitParam::Send(InputSendParam::EchoLevel), 1.0)],
        "owed answer arrives once the ring frees"
    );

    rig.engine.evict_slots();
    let evicted = rig.port(0).returned_unit().expect("evicted");
    assert_eq!((evicted.id, evicted.token, evicted.left), (1, 11, lf_engine::Left::Evicted));
    drop((old, evicted));
    assert_eq!(drops.load(SeqCst), 2);
}
