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
//! Each [`EngineHost::slot`](super::EngineHost::slot) call is one owner's handle, with its own token:
//! a unit comes back (on the port, or evicted) only to the handle that installed it or a clone. An
//! owner that gives up on its unit ([`SlotHost::abandon`]: a `remove` timed out, so it leaks the
//! plugin) orphans it: whenever the unit comes back it leaks too, and the slot frees.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lf_engine::{SlotPort, SlotProcessor};

use super::Core;

/// How often `remove` looks for its unit.
const POLL: Duration = Duration::from_millis(1);

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
    /// in the engine included: try again once it is out), or the port is full.
    pub fn install(&self, unit: Box<dyn SlotProcessor>, rate: u32) -> Result<(), (Box<dyn SlotProcessor>, String)> {
        {
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
            if let Err(unit) = port.install(unit) {
                return Err((unit, format!("slot {}'s port is full", self.slot)));
            }
            self.core.holder[self.slot].store(self.token, Release);
        }
        self.service_if_idle();
        Ok(())
    }

    /// Take the slot's unit back: the engine releases its notes, crossfades it to bypass and stops it.
    /// `Ok(None)` when the slot holds none; `Err` when it does not come back within `timeout` (the
    /// unit stays the engine's, and a later `remove` or `take_evicted` can still recover it).
    pub fn remove(&self, timeout: Duration) -> Result<Option<Box<dyn SlotProcessor>>, String> {
        if let Some(unit) = self.take_evicted() {
            return Ok(Some(unit));
        }
        {
            let mut port = self.core.ports[self.slot].lock().map_err(|_| "slot port poisoned".to_string())?;
            if self.core.holder[self.slot].load(Acquire) != self.token {
                return Ok(None);
            }
            let port = port.as_mut().ok_or_else(|| "no engine holds this slot".to_string())?;
            if !port.remove() {
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
    pub fn take_evicted(&self) -> Option<Box<dyn SlotProcessor>> {
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
            if let Some(unit) = port.returned() {
                log::warn!("[engine_io] slot {}: an abandoned unit came back; leaking it", self.slot);
                std::mem::forget(unit);
                self.core.holder[self.slot].store(0, Release);
            }
        }
    }

    /// This owner's unit, if the engine handed it back on the port.
    fn returned(&self) -> Option<Box<dyn SlotProcessor>> {
        let mut port = self.core.ports[self.slot].lock().ok()?;
        if self.core.holder[self.slot].load(Acquire) != self.token {
            return None;
        }
        let unit = port.as_mut()?.returned();
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

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::Arc;
    use std::time::Duration;

    use lf_engine::grid::Frame;
    use lf_engine::{SlotEvent, SlotKind, SlotProcessor};

    use crate::engine_io::test_rig::TestDevice;

    /// An effect that outputs a constant and counts its calls.
    struct Constant {
        level: f32,
        calls: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
    }

    impl SlotProcessor for Constant {
        fn kind(&self) -> SlotKind {
            SlotKind::Effect
        }
        fn latency(&self) -> Frame {
            0
        }
        fn process(&mut self, _frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
            self.calls.fetch_add(1, SeqCst);
            out.fill(self.level);
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
        assert!(device.wait_blocks(20, Duration::from_secs(5)));
        assert!(calls.load(SeqCst) > 0, "the engine processed the unit");
        let back = slot.remove(Duration::from_secs(2)).expect("comes back").expect("a unit");
        assert_eq!(stops.load(SeqCst), 1, "stopped once, before it left");
        let back = back.into_any().downcast::<Constant>().expect("the host's own type");
        assert_eq!(back.level, 0.25);
        assert!(slot.remove(Duration::from_millis(10)).unwrap().is_none(), "nothing left in the slot");
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
