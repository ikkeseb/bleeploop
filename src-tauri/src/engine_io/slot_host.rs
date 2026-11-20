//! OWNS: a plugin owner's handshake with the engine for one slot: install a unit, take it back, and
//! pick up a unit the device side evicted (a sample-rate change built a new engine).
//!
//! The owner thread builds and activates a unit at [`SlotHost::rate`], then [`SlotHost::install`]s it
//! with that rate (a unit activated for an engine a rebuild has since replaced comes straight back as
//! an eviction, to be re-activated); lifecycle work
//! (a restart, a teardown) first [`SlotHost::remove`]s it, which blocks until the engine has
//! crossfaded it out and stopped it. While a device runs the engine services the slot's port at each
//! block start. While none runs nothing would, so the host takes the engine lock itself and services
//! the port at once (no crossfade: nothing is sounding). A unit the device side evicted waits in
//! [`Core::evicted`] until its owner takes it back ([`SlotHost::take_evicted`]), re-activates it at
//! the new [`SlotHost::rate`] and installs it again.
//!
//! A slot seats one plugin unit here, under an id minted for each install (`Core::next_unit`), so a
//! reinstall (a restart, a swap) never meets its own removal's id in the engine; the chain host that
//! reads each `Event::Seat` is E2's (`docs/unit-contract.md` §9). An install settles on the engine's
//! outcome: it waits until the engine has taken the message (its port's ticket), and an install the
//! engine refused (a latency past `lf_engine::max_chain_latency`) comes back to its caller with the
//! reason, as any other load error, never reported loaded while the slot plays dry.
//!
//! Each [`EngineHost::slot`](super::EngineHost::slot) call is one owner's handle, with its own token:
//! a unit comes back (on the port, or evicted) only to the handle that installed it or a clone. An
//! owner that gives up on its unit ([`SlotHost::abandon`]: a `remove` timed out, so it leaks the
//! plugin) orphans it: whenever the unit comes back it leaks too, and the slot frees.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lf_engine::{SeatRefusal, SlotPort, Ticket, UnitId, UnitProcessor};

use super::Core;

/// How often `remove` looks for its unit, and `install` for the engine's answer.
const POLL: Duration = Duration::from_millis(1);
/// How long `install` waits for the engine to take its unit: a running device services the port at its
/// next block, an idle one at once. Past it (a stalled device) the install stands as sent: the device
/// owner's recovery evicts the unit to its owner, whose reinstall settles again.
const SETTLE: Duration = Duration::from_secs(2);

/// `Core::holder` for a unit whose owner abandoned it.
pub(crate) const ORPHAN: u64 = u64::MAX;

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
        let token = core.next_token.fetch_add(1, Relaxed);
        SlotHost { core, slot, token }
    }

    pub fn slot(&self) -> usize {
        self.slot
    }

    /// The id this slot's unit was last installed under.
    fn unit_id(&self) -> UnitId {
        self.core.unit_ids[self.slot].load(Acquire)
    }

    /// A fresh unit id, never 0.
    fn mint(&self) -> UnitId {
        loop {
            let id = self.core.next_unit.fetch_add(1, Relaxed);
            if id != 0 {
                return id;
            }
        }
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
        let id = self.mint();
        let ticket = {
            // Under the port lock: a rebuild holds every port while it publishes the new rate and
            // replaces the ports, so the rate and the port read here belong to one engine.
            let mut port = match self.core.ports[self.slot].lock() {
                Ok(port) => port,
                Err(_) => return Err((unit, "slot port poisoned".to_string())),
            };
            let Some(port) = port.as_mut() else {
                return Err((unit, "no audio device is open".to_string()));
            };
            self.reap(port);
            if self.core.holder[self.slot].load(Acquire) != 0 {
                return Err((unit, format!("slot {} already holds a unit", self.slot)));
            }
            if self.core.rate.load(Relaxed) != rate {
                let mut waiting = self.core.evicted[self.slot].lock().unwrap_or_else(|e| e.into_inner());
                if waiting.is_some() {
                    return Err((unit, format!("slot {} already holds a unit", self.slot)));
                }
                *waiting = Some((self.token, unit));
                return Ok(());
            }
            let ticket = match port.install(id, unit) {
                Ok(ticket) => ticket,
                Err(unit) => return Err((unit, format!("slot {}'s port is full", self.slot))),
            };
            self.core.unit_ids[self.slot].store(id, Release);
            self.core.holder[self.slot].store(self.token, Release);
            ticket
        };
        self.settle(id, &ticket)
    }

    /// Wait for the engine's outcome of install `id`: seated, `Ok`; refused, the unit back with why, and
    /// the slot free. A rebuild that replaced the port meanwhile took the unit to the eviction mailbox
    /// (refused or not), and a device that never services the port within [`SETTLE`] leaves it there:
    /// both stand as `Ok`, for the owner's eviction path.
    fn settle(&self, id: UnitId, ticket: &Ticket) -> Result<(), (Box<dyn UnitProcessor>, String)> {
        let deadline = Instant::now() + SETTLE;
        loop {
            self.service_if_idle();
            {
                let mut port = self.core.ports[self.slot].lock().unwrap_or_else(|e| e.into_inner());
                let Some(port) = port.as_mut().filter(|p| ticket.of(p)) else { return Ok(()) };
                if ticket.answered() {
                    let Some((unit, why)) = port.refused(id) else { return Ok(()) };
                    if self.core.holder[self.slot].compare_exchange(self.token, 0, AcqRel, Acquire).is_ok() {
                        self.core.unit_ids[self.slot].store(0, Release);
                    }
                    return Err((unit, refusal(self.slot, why)));
                }
            }
            if Instant::now() >= deadline {
                log::warn!("[engine_io] slot {}: the engine did not take unit {id} within {} ms; it stands as sent", self.slot, SETTLE.as_millis());
                return Ok(());
            }
            std::thread::sleep(POLL);
        }
    }

    /// Take the slot's unit back: the engine releases its notes, crossfades it to bypass and stops it.
    /// `Ok(None)` when the slot holds none; `Err` when it does not come back within `timeout` (the
    /// unit stays the engine's, and a later `remove` or `take_evicted` can still recover it).
    pub fn remove(&self, timeout: Duration) -> Result<Option<Box<dyn UnitProcessor>>, String> {
        {
            let mut port = self.core.ports[self.slot].lock().map_err(|_| "slot port poisoned".to_string())?;
            if self.core.holder[self.slot].load(Acquire) != self.token {
                // Looked at under the port lock: a rebuild parks a unit with every port held, so an
                // eviction is either done here or not begun.
                return Ok(self.take_evicted());
            }
            let port = port.as_mut().ok_or_else(|| "no engine holds this slot".to_string())?;
            if !port.remove(self.unit_id()) {
                return Err(format!("slot {}'s port is full", self.slot));
            }
        }
        let deadline = Instant::now() + timeout;
        loop {
            self.service_if_idle();
            if let Some(unit) = self.returned() {
                return Ok(Some(unit));
            }
            if let Some(unit) = self.take_evicted() {
                return Ok(Some(unit));
            }
            if Instant::now() >= deadline {
                return Err(format!("slot {}'s unit did not come back within {} ms", self.slot, timeout.as_millis()));
            }
            std::thread::sleep(POLL);
        }
    }

    /// This owner's unit, if the device side evicted it (a new engine at another rate): re-activate it
    /// at [`SlotHost::rate`] and install it again. The plugin owner polls this every turn.
    pub fn take_evicted(&self) -> Option<Box<dyn UnitProcessor>> {
        let mut waiting = self.core.evicted[self.slot].lock().ok()?;
        if waiting.as_ref().is_some_and(|(owner, _)| *owner == self.token) {
            waiting.take().map(|(_, unit)| unit)
        } else {
            None
        }
    }

    /// Give up on this owner's unit: a `remove` timed out and the owner leaks its plugin rather than
    /// unload code the unit may still run. Whenever the unit comes back it leaks too, never reaching
    /// another owner, and the slot frees ([`SlotHost::install`] or the device side's eviction reaps it).
    pub fn abandon(&self) {
        let mut port = self.core.ports[self.slot].lock().unwrap_or_else(|e| e.into_inner());
        if self.core.holder[self.slot].compare_exchange(self.token, ORPHAN, AcqRel, Acquire).is_ok() {
            if let Some(port) = port.as_mut() {
                self.reap(port);
            }
        }
        let mut waiting = self.core.evicted[self.slot].lock().unwrap_or_else(|e| e.into_inner());
        if waiting.as_ref().is_some_and(|(owner, _)| *owner == self.token) {
            std::mem::forget(waiting.take());
        }
    }

    /// Under the slot's port lock: an abandoned unit the engine has handed back leaks, and the slot
    /// frees. Its plugin stays loaded, leaked by its owner, but nothing may call into it again (a drop
    /// included).
    fn reap(&self, port: &mut SlotPort) {
        if self.core.holder[self.slot].load(Acquire) == ORPHAN {
            if let Some((_, unit)) = port.returned() {
                log::warn!("[engine_io] slot {}: an abandoned unit came back; leaking it", self.slot);
                std::mem::forget(unit);
                self.core.holder[self.slot].store(0, Release);
            }
        }
    }

    /// This owner's unit, if the engine handed it back on the port.
    fn returned(&self) -> Option<Box<dyn UnitProcessor>> {
        let mut port = self.core.ports[self.slot].lock().ok()?;
        if self.core.holder[self.slot].load(Acquire) != self.token {
            return None;
        }
        let unit = port.as_mut()?.returned().map(|(_, unit)| unit);
        if unit.is_some() {
            self.core.holder[self.slot].store(0, Release);
        }
        unit
    }

    /// With no device running nothing services the port: do it here, under the engine lock. A unit that
    /// panics here faults the engine as a callback panic does (the owner replaces it); a faulted engine
    /// is left to that.
    fn service_if_idle(&self) {
        if self.core.running.load(Acquire) {
            return;
        }
        let mut rt = self.core.rt.lock().unwrap_or_else(|e| e.into_inner());
        // The owner raises `running` under this lock: a device may have started while this waited.
        if self.core.running.load(Acquire) || rt.faulted {
            return;
        }
        let rt = &mut *rt;
        let Some(engine) = rt.engine.as_mut() else { return };
        if catch_unwind(AssertUnwindSafe(|| engine.service_slots_idle())).is_err() {
            self.core.counters.panics.fetch_add(1, Relaxed);
            self.core.latch_fault(rt);
        }
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
        assert!(core.evicted[0].lock().unwrap().is_none(), "no unit left parked");

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
            assert_eq!(device.host().core.holder[0].load(SeqCst), 0, "running {running}: the slot is free");
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
            ids.push(core.unit_ids[1].load(SeqCst));
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
