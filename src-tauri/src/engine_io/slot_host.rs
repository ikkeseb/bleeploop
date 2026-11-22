//! OWNS: a plugin owner's handshake with the engine for one slot: install a unit, take it back, and
//! pick up a unit the device side evicted (a sample-rate change built a new engine).
//!
//! Legacy adapter over the E2 install table (`unit_host.rs`, `Spec::Legacy`), kept with its exact
//! public API until activation deletes the legacy path.
//!
//! The owner thread builds and activates a unit at [`SlotHost::rate`], then [`SlotHost::install`]s it
//! with that rate (a unit activated for an engine a rebuild has since replaced comes straight back as
//! an eviction, to be re-activated); lifecycle work (a restart, a teardown) first [`SlotHost::remove`]s
//! it, which blocks until the engine has crossfaded it out and stopped it. While a device runs the
//! engine services the slot's port at each block start. While none runs nothing would, so the host
//! takes the engine lock itself and services the port at once (no crossfade: nothing is sounding).
//!
//! A slot seats one plugin unit here, under a device-minted id per install, so a reinstall (a
//! restart, a swap) never meets its own removal's id in the engine. An install settles on the
//! engine's outcome: a refusal after [`SETTLE`] now returns to its owner via [`SlotHost::take_evicted`]
//! instead of going unseen, and a held removal during a take waits for the take to end instead of
//! timing out and leaking the plugin.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lf_engine::{SeatOp, SeatRefusal, UnitProcessor, MAX_UNITS};

use super::chain::{PendingKind, PendingOp};
use super::unit_host::{
    service_if_idle, wait_op_inner, Back, Leave, OpState, Row, RowEngineState, Spec, UnitHost,
};
use super::Core;

/// How often `remove` looks for its unit after `Applied`.
const POLL: Duration = Duration::from_millis(1);
/// How long `install` waits for the engine to take its unit: a running device services the port at its
/// next block, an idle one at once. Past it (a stalled device) the install stands as sent: a later
/// eviction or refusal reaches its owner through [`SlotHost::take_evicted`].
const SETTLE: Duration = Duration::from_secs(2);

/// One slot's handshake with the engine (`EngineHost::slot`). Cheap to clone (a clone is the same
/// owner); one plugin owner uses a slot at a time.
#[derive(Clone)]
pub struct SlotHost {
    core: Arc<Core>,
    slot: usize,
    token: u64,
}

impl SlotHost {
    pub(crate) fn new(core: Arc<Core>, slot: usize) -> SlotHost {
        let token = core.state.lock().unwrap_or_else(|e| e.into_inner()).table.mint_legacy_owner();
        SlotHost { core, slot, token }
    }

    pub fn slot(&self) -> usize {
        self.slot
    }

    /// The engine's sample rate: what a unit is activated at. `None` before a device first opened.
    pub fn rate(&self) -> Option<u32> {
        self.core.rate()
    }

    /// The largest block the engine hands a unit.
    pub fn max_block(&self) -> usize {
        self.core.max_block.load(Relaxed) as usize
    }

    /// Hand a unit activated at `rate` to the engine; it crossfades in at the next block (engaged at
    /// once while no device runs). An engine at another rate has replaced the one `rate` was read from:
    /// the unit goes to [`SlotHost::take_evicted`] instead, for its owner to re-activate. Gives the unit
    /// back with the reason when no engine exists yet, the slot already holds a unit (an orphan still
    /// in the engine included: try again once it is out), the port is full, or the engine refused it.
    pub fn install(&self, unit: Box<dyn UnitProcessor>, rate: u32) -> Result<(), (Box<dyn UnitProcessor>, String)> {
        let (unit_host, id, op) = {
            // Under the port lock: a rebuild holds every port while it publishes the new rate and
            // replaces the ports, so the rate and the port read here belong to one engine.
            let mut port_guard = match self.core.ports[self.slot].lock() {
                Ok(port) => port,
                Err(_) => return Err((unit, "slot port poisoned".to_string())),
            };
            let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(port) = port_guard.as_mut() {
                state.table.reap_port(self.slot, port);
            }
            let Some(port) = port_guard.as_mut() else {
                return Err((unit, "no audio device is open".to_string()));
            };
            if state.table.has_any_chain() || state.model.has_non_legacy_plate() {
                return Err((unit, "chain units are active in this run".to_string()));
            }
            if state.table.legacy_slot_occupied(self.slot) {
                return Err((unit, format!("slot {} already holds a unit", self.slot)));
            }
            let cur_rate = self.core.rate.load(Relaxed);
            if cur_rate != rate {
                let id = state.table.mint_legacy_id();
                let install = state.table.mint_token();
                let mut mailbox = VecDeque::new();
                mailbox.push_back(Back::Evicted { unit, bypassed: false });
                state.table.insert(Row {
                    token: install,
                    id,
                    slot: self.slot,
                    spec: Spec::Legacy,
                    is_reload: false,
                    superseded: false,
                    abandoned: false,
                    released: false,
                    id_freed: false,
                    leave: Leave::Staying,
                    in_engine: RowEngineState::Out,
                    install_op: None,
                    mailbox,
                    settled: BTreeMap::new(),
                    last_remove_settled: None,
                    legacy_owner: self.token,
                });
                return Ok(());
            }
            let id = state.table.mint_legacy_id();
            let install = state.table.mint_token();
            let gen = self.core.engine_gen.load(Acquire);
            let unit = match port.install_at(id, install, MAX_UNITS, false, None, unit) {
                Ok(_) => None,
                Err(unit) => Some(unit),
            };
            if let Some(unit) = unit {
                return Err((unit, format!("slot {}'s port is full", self.slot)));
            }
            state.model.add_plate(self.slot, id, &Spec::Legacy, MAX_UNITS, false);
            let op = state.model.mint_op(install, gen, self.slot);
            state.table.insert(Row {
                token: install,
                id,
                slot: self.slot,
                spec: Spec::Legacy,
                is_reload: false,
                superseded: false,
                abandoned: false,
                released: false,
                id_freed: false,
                leave: Leave::Staying,
                in_engine: RowEngineState::Queued { generation: gen },
                install_op: Some(op.n()),
                mailbox: VecDeque::new(),
                settled: BTreeMap::new(),
                last_remove_settled: None,
                legacy_owner: self.token,
            });
            state.model.push_pending(
                self.slot,
                PendingOp {
                    op,
                    unit: id,
                    seat_op: SeatOp::Install,
                    state: OpState::Pending,
                    kind: PendingKind::Install {
                        token: install,
                        is_reload: false,
                        replaces: None,
                        at: MAX_UNITS,
                        source: false,
                    },
                },
            );
            (UnitHost::new(self.core.clone(), self.slot, id, install), id, op)
        };

        match wait_op_inner(&self.core, op, SETTLE, true) {
            Ok(OpState::Applied | OpState::Rebuilt | OpState::Held) => Ok(()),
            Ok(OpState::Refused(why)) => {
                // Take the refused box back and free the slot immediately.
                let unit = loop {
                    if let Some(back) = unit_host.take_back() {
                        match back {
                            Back::Refused { unit, .. }
                            | Back::Evicted { unit, .. }
                            | Back::Removed(unit)
                            | Back::Superseded(unit)
                            | Back::Withdrawn { unit, .. } => break unit,
                            Back::Dismissed => {}
                        }
                    }
                    std::thread::sleep(POLL);
                };
                {
                    let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.model.remove_plate(self.slot, id);
                }
                unit_host.release();
                Err((unit, refusal(self.slot, why)))
            }
            Ok(OpState::Pending) | Err(_) => {
                log::warn!(
                    "[engine_io] slot {}: the engine did not take unit {id} within {} ms; it stands as sent",
                    self.slot,
                    SETTLE.as_millis()
                );
                Ok(())
            }
        }
    }

    /// Take the slot's unit back: the engine releases its notes, crossfades it to bypass and stops it.
    /// `Ok(None)` when the slot holds none; `Err` when it does not come back within `timeout` (the
    /// unit stays the engine's, and a later `remove` or `take_evicted` can still recover it).
    pub fn remove(&self, timeout: Duration) -> Result<Option<Box<dyn UnitProcessor>>, String> {
        let (unit_host, id, op) = {
            let mut port_guard = self.core.ports[self.slot].lock().map_err(|_| "slot port poisoned".to_string())?;
            let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(port) = port_guard.as_mut() {
                state.table.reap_port(self.slot, port);
            }
            let Some(row) = state.table.legacy_row_of_owner_mut(self.slot, self.token) else {
                return Ok(None);
            };
            let id = row.id;
            let install = row.token;
            // Already returned (evicted by a rebuild that raced this removal, or late refusal): take it now.
            if let Some(back) = row.mailbox.pop_front() {
                let unit = unpack_back(self.slot, back);
                state.model.remove_plate(self.slot, id);
                let _ = state.table.release_row(install);
                return Ok(unit);
            }
            let Some(port) = port_guard.as_mut() else {
                return Err("no engine holds this slot".to_string());
            };
            if !port.remove(id) {
                return Err(format!("slot {}'s port is full", self.slot));
            }
            let gen = self.core.engine_gen.load(Acquire);
            if let Some(row) = state.table.row_mut(install) {
                row.leave = Leave::Pending { held: None };
                if row.in_engine.generation() == Some(gen) {
                    row.in_engine = RowEngineState::Removing { generation: gen };
                }
            }
            let op = state.model.mint_op(install, gen, self.slot);
            state.model.push_pending(
                self.slot,
                PendingOp {
                    op,
                    unit: id,
                    seat_op: SeatOp::Remove,
                    state: OpState::Pending,
                    kind: PendingKind::Remove { owner_token: Some(install) },
                },
            );
            (UnitHost::new(self.core.clone(), self.slot, id, install), id, op)
        };

        let waited = wait_op_inner(&self.core, op, timeout, false);
        match waited {
            Ok(OpState::Applied | OpState::Rebuilt) => {}
            Ok(OpState::Refused(_)) => {
                // Unknown: might already have been evicted into the mailbox.
                if let Some(back) = unit_host.take_back() {
                    let unit = unpack_back(self.slot, back);
                    {
                        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.model.remove_plate(self.slot, id);
                    }
                    unit_host.release();
                    return Ok(unit);
                }
                return Err(format!("slot {}'s unit did not come back within {} ms", self.slot, timeout.as_millis()));
            }
            Ok(OpState::Held) => {
                return Err(format!("slot {}'s unit removal is held while closing", self.slot));
            }
            Ok(OpState::Pending) | Err(_) => {
                return Err(format!("slot {}'s unit did not come back within {} ms", self.slot, timeout.as_millis()));
            }
        }

        // The removal applied (or a rebuild evicted it): wait for the 10 ms crossfade-out box to arrive.
        let deadline = Instant::now() + timeout.max(Duration::from_millis(500));
        loop {
            service_if_idle(&self.core);
            if let Some(back) = unit_host.take_back() {
                let unit = unpack_back(self.slot, back);
                {
                    let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.model.remove_plate(self.slot, id);
                }
                unit_host.release();
                return Ok(unit);
            }
            if Instant::now() >= deadline {
                return Err(format!("slot {}'s unit did not come back within {} ms", self.slot, timeout.as_millis()));
            }
            std::thread::sleep(POLL);
        }
    }

    /// This owner's unit, if the device side evicted it (a new engine at another rate) or a refusal
    /// arrived after [`SETTLE`]: re-activate it at [`SlotHost::rate`] and install it again. The plugin
    /// owner polls this every turn.
    pub fn take_evicted(&self) -> Option<Box<dyn UnitProcessor>> {
        let mut port_guard = self.core.ports[self.slot].lock().ok()?;
        let mut state = self.core.state.lock().ok()?;
        if let Some(port) = port_guard.as_mut() {
            state.table.reap_port(self.slot, port);
        }
        let row = state.table.legacy_row_of_owner_mut(self.slot, self.token)?;
        let id = row.id;
        let install = row.token;
        let back = row.mailbox.pop_front()?;
        let unit = unpack_back(self.slot, back);
        state.model.remove_plate(self.slot, id);
        let _ = state.table.release_row(install);
        unit
    }

    /// Give up on this owner's unit: a `remove` timed out and the owner leaks its plugin rather than
    /// unload code the unit may still run. Whenever the unit comes back it leaks too, never reaching
    /// another owner, and the slot frees ([`SlotHost::install`] or the device side's eviction reaps it).
    pub fn abandon(&self) {
        let mut port_guard = self.core.ports[self.slot].lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(row) = state.table.legacy_row_of_owner(self.slot, self.token) {
            let id = row.id;
            let install = row.token;
            state.model.remove_plate(self.slot, id);
            state.table.abandon_row(install);
            let _ = state.table.release_row(install);
        }
        if let Some(port) = port_guard.as_mut() {
            state.table.reap_port(self.slot, port);
        }
    }
}

fn unpack_back(slot: usize, back: Back) -> Option<Box<dyn UnitProcessor>> {
    match back {
        Back::Removed(unit)
        | Back::Superseded(unit)
        | Back::Withdrawn { unit, .. }
        | Back::Evicted { unit, .. } => Some(unit),
        Back::Refused { unit, why } => {
            log::warn!("[engine_io] slot {slot}: late install refusal: {}", refusal(slot, why));
            Some(unit)
        }
        Back::Dismissed => None,
    }
}

/// Why the engine refused an install, as a load error says it.
fn refusal(slot: usize, why: SeatRefusal) -> String {
    match why {
        SeatRefusal::Latency => format!("slot {slot}: the plugin's latency is past the most a slot compensates (one second)"),
        SeatRefusal::Full => format!("slot {slot} seats no more units"),
        SeatRefusal::Id => format!("slot {slot}: the engine already seats this unit's id"),
        SeatRefusal::Placement => format!("slot {slot}: an instrument goes only at the head of an empty chain"),
        SeatRefusal::Unknown => format!("slot {slot}: the engine refused the unit"),
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::Arc;
    use std::time::Duration;

    use lf_engine::{SlotEvent, UnitProcessor};

    use crate::engine_io::test_rig::TestDevice;

    /// An effect that outputs a constant and counts its calls.
    struct Constant {
        level: f32,
        calls: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
    }

    impl UnitProcessor for Constant {
        fn info(&self) -> lf_engine::UnitInfo {
            lf_engine::UnitInfo { kind: lf_engine::UnitKind::Plugin, source: false, latency: 0 }
        }
        fn process(&mut self, _cx: &lf_engine::UnitCx, _input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
            self.calls.fetch_add(1, SeqCst);
            for o in out {
                o.fill(self.level);
            }
        }
        fn stop(&mut self) {
            self.stops.fetch_add(1, SeqCst);
        }
        fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
            self
        }
    }

    #[test]
    fn a_unit_installs_renders_and_comes_back_stopped_while_the_engine_renders() {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let slot = device.host().slot(0);
        let (calls, stops) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let unit = Box::new(Constant { level: 0.25, calls: calls.clone(), stops: stops.clone() });
        assert!(slot.install(unit, 48_000).is_ok());
        let again = Box::new(Constant { level: 0.0, calls: calls.clone(), stops: stops.clone() });
        assert!(slot.install(again, 48_000).is_err(), "one unit per slot");
        assert!(device.wait_blocks(20));
        assert!(calls.load(SeqCst) > 0, "the engine processed the unit");
        let back = slot.remove(Duration::from_secs(2)).expect("comes back").expect("a unit");
        assert_eq!(stops.load(SeqCst), 1, "stopped once, before it left");
        let back = back.into_any().downcast::<Constant>().expect("the host's own type");
        assert_eq!(back.level, 0.25);
        assert!(slot.remove(Duration::from_millis(10)).unwrap().is_none(), "nothing left in the slot");
    }

    #[test]
    fn a_removal_that_races_a_rebuild_takes_its_unit_from_the_eviction_mailbox() {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let core = &device.host().core;
        let slot = device.host().slot(0);
        let stops = Arc::new(AtomicUsize::new(0));
        let unit = Box::new(Constant { level: 0.5, calls: Arc::new(AtomicUsize::new(0)), stops: stops.clone() });
        assert!(slot.install(unit, 48_000).is_ok());
        assert!(device.wait_blocks(20));
        let removed = std::thread::scope(|s| {
            // Port 1 held: the rebuild takes port 0 and waits for port 1, so a removal that has already
            // looked in the empty mailbox waits on port 0 until the rebuild has parked its unit.
            let held = core.ports[1].lock().unwrap();
            let rebuild = s.spawn(|| device.rebuild_at(44_100, 512));
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while core.ports[0].try_lock().is_ok() {
                assert!(std::time::Instant::now() < deadline, "the rebuild takes port 0");
                std::thread::sleep(Duration::from_millis(1));
            }
            let removal = s.spawn(|| slot.remove(Duration::from_secs(2)));
            std::thread::sleep(Duration::from_millis(100));
            drop(held);
            rebuild.join().unwrap();
            removal.join().unwrap()
        });
        let back = removed.expect("the removal answers").expect("the removal gets the unit the rebuild evicted");
        assert_eq!(back.into_any().downcast::<Constant>().expect("the host's own type").level, 0.5);
        assert_eq!(stops.load(SeqCst), 1, "stopped once, as it was evicted");
        assert!(!core.legacy_has_evicted(0), "no unit left parked");

        // The slot works on: the next unit installs, and the next rebuild hands it back.
        let unit = Box::new(Constant { level: 0.25, calls: Arc::new(AtomicUsize::new(0)), stops: stops.clone() });
        assert!(slot.install(unit, 44_100).is_ok());
        device.rebuild_at(48_000, 512);
        let back = slot.take_evicted().expect("the next eviction reaches its owner");
        assert_eq!(back.into_any().downcast::<Constant>().unwrap().level, 0.25);
    }

    /// An effect with a latency and nothing else.
    struct Latent(lf_engine::grid::Frame);

    impl UnitProcessor for Latent {
        fn info(&self) -> lf_engine::UnitInfo {
            lf_engine::UnitInfo { kind: lf_engine::UnitKind::Plugin, source: false, latency: self.0 }
        }
        fn process(&mut self, _cx: &lf_engine::UnitCx, _input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
            for o in out {
                o.fill(0.0);
            }
        }
        fn stop(&mut self) {}
        fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
            self
        }
    }

    #[test]
    fn an_install_the_engine_refuses_comes_back_to_its_caller_with_why_and_frees_the_slot() {
        for running in [true, false] {
            let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
            if !running {
                device.stop();
            }
            let slot = device.host().slot(0);
            let past = lf_engine::max_chain_latency(48_000) + 1;
            let (unit, why) = slot.install(Box::new(Latent(past)), 48_000).err().expect("refused, not reported loaded");
            assert!(why.contains("latency"), "running {running}: {why}");
            assert_eq!(unit.into_any().downcast::<Latent>().expect("the caller's own unit").0, past);
            assert!(!device.host().core.legacy_occupied(0), "running {running}: the slot is free");
            assert!(slot.install(Box::new(Latent(64)), 48_000).is_ok(), "running {running}: the next load seats");
            assert!(slot.remove(Duration::from_secs(2)).unwrap().is_some());
        }
    }

    #[test]
    fn a_swap_never_meets_its_own_removal_in_the_engine() {
        let device = TestDevice::start(48_000, 64, Duration::from_millis(1), |_| 0.0);
        let core = &device.host().core;
        let slot = device.host().slot(1);
        let mut ids = Vec::new();
        for round in 0..20 {
            // Removed, then at once installed again: each install has its own id, so the engine never
            // judges it against the seat still fading out under the last one.
            assert!(slot.install(Box::new(Latent(round)), 48_000).is_ok(), "round {round}: seated");
            ids.push(core.legacy_unit_id(1));
            let back = slot.remove(Duration::from_secs(2)).expect("comes back").expect("a unit");
            assert_eq!(back.into_any().downcast::<Latent>().unwrap().0, round);
        }
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "a fresh id per install: {ids:?}");
        assert!(!ids.contains(&0));
    }

    #[test]
    fn with_no_device_running_the_host_services_the_slot_itself() {
        let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        device.stop();
        let slot = device.host().slot(1);
        let stops = Arc::new(AtomicUsize::new(0));
        let unit = Box::new(Constant { level: 1.0, calls: Arc::new(AtomicUsize::new(0)), stops: stops.clone() });
        assert!(slot.install(unit, 48_000).is_ok());
        assert!(slot.remove(Duration::from_millis(100)).unwrap().is_some());
        assert_eq!(stops.load(SeqCst), 1);
    }
}
