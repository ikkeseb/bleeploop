//! OWNS: a plugin owner's handshake with the engine for one slot ([`SlotHost`]), over the slot's desk
//! (`desk`), which routes the engine's replies to their owners by tag.
//!
//! Each [`EngineHost::slot`](super::EngineHost::slot) call is one owner with its own tag (a clone is the
//! same owner). The owner builds and activates a unit at [`SlotHost::rate`] and puts it in:
//! [`SlotHost::install`] into an empty slot, or [`SlotHost::replace`] beside the unit another owner
//! plays (staged muted, then crossfaded to); [`SlotHost::answer`] reads a replace's outcome, and
//! [`SlotHost::withdraw`] calls it off while it stages. Lifecycle work (a restart, a teardown) first
//! [`SlotHost::remove`]s the unit, which waits until the engine has crossfaded it out and stopped it.
//! A unit that left the engine on its own waits on the desk for [`SlotHost::take_back`]: an eviction
//! (a rebuild at another rate, or an install activated at a rate since replaced) is re-activated at the
//! new [`SlotHost::rate`] and installed again; a unit another owner's replace retired never goes in
//! again, and nor does its owner. An owner that gives up on its unit ([`SlotHost::abandon`]: a wait
//! timed out, so it leaks the plugin) leaks whatever comes back of it, and the slot frees.
//!
//! While a device runs the engine services the slot's port at each block start. While none runs nothing
//! would, so the host takes the engine lock itself and services the port at once (no crossfade and no
//! staging: nothing is sounding).

mod desk;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant};

use lf_engine::{HandbackReason, Rejection, SlotProcessor};

pub(crate) use desk::Desk;
pub use desk::Standing;

use super::Core;

/// How often `answer` and `remove` look for their reply.
const POLL: Duration = Duration::from_millis(1);

/// A replace's outcome ([`SlotHost::answer`]).
pub enum Answer {
    /// The new unit is the slot's; the one it replaced comes back to its owner as [`Back::Out`].
    Committed,
    /// The unit never became the slot's: back, stopped if it processed. The playing unit plays on.
    Refused(Box<dyn SlotProcessor>, Rejection),
    /// No answer yet.
    Pending,
}

/// A unit of this owner's back from the engine, stopped: on its own ([`SlotHost::take_back`]), or
/// removed ([`SlotHost::remove_back`]).
pub enum Back {
    /// Evicted, or removed: re-activate it at [`SlotHost::rate`] and install it again.
    Evicted(Box<dyn SlotProcessor>),
    /// Retired by another owner's replace: never install it again.
    Out(Box<dyn SlotProcessor>),
}

impl Back {
    pub fn into_unit(self) -> Box<dyn SlotProcessor> {
        match self {
            Back::Evicted(unit) | Back::Out(unit) => unit,
        }
    }
}

/// One owner's handshake with a slot (`EngineHost::slot`). Cheap to clone (a clone is the same owner,
/// and dropping any clone is the owner dropping: `Drop`).
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

    /// This owner's tag: what its units are sent with.
    pub fn tag(&self) -> u64 {
        self.token
    }

    /// The engine's sample rate: what a unit is activated at. `None` before a device first opened.
    pub fn rate(&self) -> Option<u32> {
        self.core.rate()
    }

    /// The largest block the engine hands a unit.
    pub fn max_block(&self) -> usize {
        self.core.max_block.load(Relaxed) as usize
    }

    /// Where this owner stands on the slot; `None` once nothing of it is in the engine or on the desk,
    /// unless it was retired: that one reads Retiring until it drops.
    pub fn standing(&self) -> Option<Standing> {
        self.desk().standing(self.token)
    }

    /// Test-only: nothing of this owner's is in the engine or waits on the desk.
    #[cfg(test)]
    pub(crate) fn holds_nothing(&self) -> bool {
        self.desk().holds_nothing(self.token)
    }

    /// Hand a unit activated at `rate` to the engine; it crossfades in at the next block (engaged at
    /// once while no device runs). An engine at another rate has replaced the one `rate` was read from:
    /// the unit waits for [`SlotHost::take_back`] instead, as an eviction. Gives the unit back with the
    /// reason when no engine exists yet, another owner's replace retired this one, the slot already
    /// holds a unit (an abandoned one still in the engine included: try again once it is out), or the
    /// port is full.
    pub fn install(&self, unit: Box<dyn SlotProcessor>, rate: u32) -> Result<(), (Box<dyn SlotProcessor>, String)> {
        {
            // Under the desk lock: a rebuild holds every desk while it publishes the new rate and
            // replaces the ports, so the rate and the port read here belong to one engine.
            let mut desk = self.desk();
            if desk.occupied() {
                // A remove queued while no device runs (an abandoned owner's) waits for a service.
                drop(desk);
                self.service_if_idle();
                desk = self.desk();
            }
            if desk.port.is_none() {
                return Err((unit, "no audio device is open".to_string()));
            }
            if desk.standing(self.token) == Some(Standing::Retiring) {
                return Err((unit, format!("slot {}: this owner was replaced", self.slot)));
            }
            if desk.occupied() {
                return Err((unit, format!("slot {} already holds a unit", self.slot)));
            }
            if self.core.rate.load(Relaxed) != rate {
                desk.park(self.token, unit);
                return Ok(());
            }
            if let Err(unit) = desk.port.as_mut().expect("checked above").install(self.token, unit) {
                return Err((unit, format!("slot {}'s port is full", self.slot)));
            }
            desk.enter(self.token, None);
        }
        self.service_if_idle();
        Ok(())
    }

    /// Stage a unit activated at `rate` beside the one the slot plays, then crossfade to it at `gain`
    /// (into an empty slot, an install gliding to `gain`). [`SlotHost::answer`] reads the outcome. Gives
    /// the unit back when no engine exists, this owner already has a unit on the slot, the engine runs at
    /// another rate (never parked: nothing plays it meanwhile), or the port is full.
    pub fn replace(&self, unit: Box<dyn SlotProcessor>, rate: u32, gain: f32) -> Result<(), (Box<dyn SlotProcessor>, String)> {
        {
            let mut desk = self.desk();
            if desk.standing(self.token).is_some() {
                return Err((unit, format!("slot {}: this owner already has a unit here", self.slot)));
            }
            if self.core.rate.load(Relaxed) != rate {
                return Err((unit, "busy: the device rate changed".to_string()));
            }
            let Some(port) = desk.port.as_mut() else {
                return Err((unit, "no audio device is open".to_string()));
            };
            if let Err(unit) = port.replace(self.token, unit, gain) {
                return Err((unit, format!("busy: slot {}'s port is full", self.slot)));
            }
            desk.enter(self.token, Some(gain));
        }
        self.service_if_idle();
        Ok(())
    }

    /// This owner's replace's outcome, waiting up to `timeout` for it. A `NotHeld` its withdraw drew
    /// (the commit won the race) never reaches here; the commit does.
    pub fn answer(&self, timeout: Duration) -> Answer {
        let deadline = Instant::now() + timeout;
        loop {
            self.service_if_idle();
            {
                let mut desk = self.desk();
                if desk.take_committed(self.token) {
                    return Answer::Committed;
                }
                let rejected = |reason: HandbackReason| match reason {
                    HandbackReason::Rejected(why) => Some(why),
                    _ => None,
                };
                if let Some((why, unit)) = desk.take_unit(self.token, rejected) {
                    if why == Rejection::Busy {
                        log::error!("[engine_io] slot {}: a replace came back Busy; swaps on a slot are serialised", self.slot);
                    }
                    return Answer::Refused(unit, why);
                }
            }
            if Instant::now() >= deadline {
                return Answer::Pending;
            }
            std::thread::sleep(POLL);
        }
    }

    /// Call off this owner's replace while its unit stages; read the outcome with [`SlotHost::answer`]
    /// (`Refused(.., Cancelled)`, or `Committed` when the commit came first). False when no engine exists
    /// or the port is full.
    pub fn withdraw(&self) -> bool {
        let mut desk = self.desk();
        let sent = desk.port.as_mut().is_some_and(|port| port.withdraw(self.token));
        if sent {
            desk.withdrew(self.token);
        }
        sent
    }

    /// This owner's unit, if it left the engine on its own: an eviction, or retired by another owner's
    /// replace (an eviction of a retired owner's unit included). The plugin owner polls this every turn.
    pub fn take_back(&self) -> Option<Back> {
        self.take(&mut self.desk(), |reason| matches!(reason, HandbackReason::Evicted | HandbackReason::ReplacedOut))
    }

    /// Take this owner's unit back: the engine releases its notes, crossfades it to bypass and stops it
    /// (a retired unit fading out is waited for). A unit already back on the desk comes first.
    /// `Ok(None)` when the owner has none on the slot; `Err` when it does not come back within
    /// `timeout` (it stays the engine's, and a later `remove` or `take_back` can still recover it).
    pub fn remove(&self, timeout: Duration) -> Result<Option<Box<dyn SlotProcessor>>, String> {
        self.remove_back(timeout).map(|back| back.map(Back::into_unit))
    }

    /// [`SlotHost::remove`], telling what may become of the unit: [`Back::Out`] when another owner's
    /// replace retired this one, whether its commit came before the remove or while it waited.
    pub fn remove_back(&self, timeout: Duration) -> Result<Option<Back>, String> {
        {
            // Under the desk lock: a rebuild hands every unit back with every desk held, so an eviction
            // is either on the desk here or not begun.
            let mut desk = self.desk();
            if let Some(back) = self.take(&mut desk, |_| true) {
                return Ok(Some(back));
            }
            match (desk.standing(self.token), desk.in_engine(self.token)) {
                (None, _) | (Some(Standing::Active | Standing::Retiring), false) => return Ok(None),
                (Some(Standing::Abandoned), _) => return Err(format!("slot {}: this owner's unit is lost", self.slot)),
                // The engine never answers a remove of a unit fading out: it comes back `ReplacedOut`.
                (Some(Standing::Retiring), true) => {}
                (Some(Standing::Active), true) => {
                    let port = desk.port.as_mut().ok_or_else(|| "no engine holds this slot".to_string())?;
                    if !port.remove(self.token) {
                        return Err(format!("slot {}'s port is full", self.slot));
                    }
                }
            }
        }
        let deadline = Instant::now() + timeout;
        loop {
            self.service_if_idle();
            {
                let mut desk = self.desk();
                if let Some(back) = self.take(&mut desk, |_| true) {
                    return Ok(Some(back));
                }
                if desk.take_not_held(self.token) {
                    return Ok(None);
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("slot {}'s unit did not come back within {} ms", self.slot, timeout.as_millis()));
            }
            std::thread::sleep(POLL);
        }
    }

    /// Give up on this owner's unit: a wait timed out and the owner leaks its plugin rather than unload
    /// code the unit may still run. Whatever comes back of it leaks too, never reaching another owner,
    /// and the slot frees; a replace that commits after this is removed at once.
    pub fn abandon(&self) {
        self.desk().abandon(self.token);
        self.service_if_idle();
    }

    /// The first unit of this owner's on `desk` whose reason `pick` takes, as what may become of it: one
    /// that came back `ReplacedOut`, or to an owner no longer Active, never goes in again.
    fn take(&self, desk: &mut Desk, pick: impl Fn(HandbackReason) -> bool) -> Option<Back> {
        // Read before the take, which releases an owner with nothing left in the engine.
        let active = desk.standing(self.token) == Some(Standing::Active);
        let (reason, unit) = desk.take_unit(self.token, |reason| pick(reason).then_some(reason))?;
        Some(if active && reason != HandbackReason::ReplacedOut { Back::Evicted(unit) } else { Back::Out(unit) })
    }

    /// The slot's desk, locked, with the port's replies routed.
    fn desk(&self) -> MutexGuard<'_, Desk> {
        let mut desk = self.core.ports[self.slot].lock().unwrap_or_else(|e| e.into_inner());
        desk.pump(&self.core.settings);
        desk
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

impl Drop for SlotHost {
    /// The owner is gone: the desk forgets it once nothing of it is left there (a retired owner's
    /// standing included). The clones are its owner thread's and its handle's, which joins that thread
    /// before it drops, so none drops while the owner still runs.
    fn drop(&mut self) {
        self.desk().forget(self.token);
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use lf_engine::grid::Frame;
    use lf_engine::{Command, Engine, EngineConfig, Rejection, SlotEvent, SlotKind, SlotProcessor, TimedCommand};

    use super::{Answer, Back, Desk, SlotHost, Standing};
    use crate::engine_io::test_rig::{TestDevice, PATIENCE};

    /// What a [`Constant`] counts: its calls, its stops and its drops (a drop the desk made would count).
    #[derive(Clone, Default)]
    struct Counts {
        calls: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl Counts {
        fn calls(&self) -> usize {
            self.calls.load(SeqCst)
        }
        fn stops(&self) -> usize {
            self.stops.load(SeqCst)
        }
        fn drops(&self) -> usize {
            self.drops.load(SeqCst)
        }
    }

    /// A unit (an effect unless `kind` says otherwise) that outputs a constant and counts; `refuse`
    /// makes it refuse to start processing.
    struct Constant {
        level: f32,
        kind: SlotKind,
        refuse: bool,
        counts: Counts,
    }

    fn constant(level: f32) -> (Box<Constant>, Counts) {
        let counts = Counts::default();
        (Box::new(Constant { level, kind: SlotKind::Effect, refuse: false, counts: counts.clone() }), counts)
    }

    impl SlotProcessor for Constant {
        fn kind(&self) -> SlotKind {
            self.kind
        }
        fn latency(&self) -> Frame {
            0
        }
        fn process(&mut self, _frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
            self.counts.calls.fetch_add(1, SeqCst);
            out.fill(if self.refuse { 0.0 } else { self.level });
        }
        fn stop(&mut self) {
            self.counts.stops.fetch_add(1, SeqCst);
        }
        fn start_refused(&self) -> bool {
            self.refuse
        }
        fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
            self
        }
    }

    impl Drop for Constant {
        fn drop(&mut self) {
            self.counts.drops.fetch_add(1, SeqCst);
        }
    }

    fn level(unit: Box<dyn SlotProcessor>) -> f32 {
        unit.into_any().downcast::<Constant>().expect("the host's own type").level
    }

    /// The loudest of the last two blocks the device rendered (a block the engine lock missed is silent).
    fn heard(device: &TestDevice) -> f32 {
        let out = device.output.lock().unwrap();
        out[out.len().saturating_sub(512)..].iter().fold(0.0, |max, s| max.max(s.abs()))
    }

    fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(value) = f() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Install `unit` once the slot frees.
    fn install_eventually(slot: &SlotHost, unit: Box<dyn SlotProcessor>, rate: u32) {
        let mut unit = Some(unit);
        eventually("the slot frees", || match slot.install(unit.take().expect("kept"), rate) {
            Ok(()) => Some(()),
            Err((back, _)) => {
                unit = Some(back);
                None
            }
        });
    }

    fn refused(answer: Answer) -> (Box<dyn SlotProcessor>, Rejection) {
        match answer {
            Answer::Refused(unit, why) => (unit, why),
            Answer::Committed => panic!("committed, not refused"),
            Answer::Pending => panic!("no answer"),
        }
    }

    /// A device at 48 kHz with owner A's unit (0.25) playing in slot 0; B is a second owner of slot 0.
    fn playing() -> (TestDevice, SlotHost, Counts, SlotHost) {
        playing_every(Duration::from_millis(1))
    }

    /// [`playing`] with a 256-frame block rendered every `period`.
    fn playing_every(period: Duration) -> (TestDevice, SlotHost, Counts, SlotHost) {
        let device = TestDevice::start(48_000, 256, period, |_| 0.0);
        let (a, b) = (device.host().slot(0), device.host().slot(0));
        let (unit, counts) = constant(0.25);
        assert!(a.install(unit, 48_000).is_ok());
        assert!(device.wait_blocks(5));
        (device, a, counts, b)
    }

    #[test]
    fn a_unit_installs_renders_and_comes_back_stopped_while_the_engine_renders() {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let slot = device.host().slot(0);
        let (unit, counts) = constant(0.25);
        assert!(slot.install(unit, 48_000).is_ok());
        let (again, _) = constant(0.0);
        assert!(slot.install(again, 48_000).is_err(), "one unit per slot");
        assert!(device.wait_blocks(20));
        assert!(counts.calls() > 0, "the engine processed the unit");
        let back = slot.remove(Duration::from_secs(2)).expect("comes back").expect("a unit");
        assert_eq!(counts.stops(), 1, "stopped once, before it left");
        assert_eq!(level(back), 0.25);
        assert!(slot.remove(Duration::from_millis(10)).unwrap().is_none(), "nothing left in the slot");
    }

    #[test]
    fn a_removal_that_races_a_rebuild_takes_its_unit_from_the_desk() {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let core = &device.host().core;
        let slot = device.host().slot(0);
        let (unit, counts) = constant(0.5);
        assert!(slot.install(unit, 48_000).is_ok());
        assert!(device.wait_blocks(20));
        let removed = std::thread::scope(|s| {
            // Desk 1 held: the rebuild takes desk 0 and waits for desk 1, so a removal that has not yet
            // looked at the desk waits on desk 0 until the rebuild has handed its unit back there.
            let held = core.ports[1].lock().unwrap();
            let rebuild = s.spawn(|| device.rebuild_at(44_100, 512));
            let deadline = Instant::now() + Duration::from_secs(5);
            while core.ports[0].try_lock().is_ok() {
                assert!(Instant::now() < deadline, "the rebuild takes desk 0");
                std::thread::sleep(Duration::from_millis(1));
            }
            let removal = s.spawn(|| slot.remove(Duration::from_secs(2)));
            std::thread::sleep(Duration::from_millis(100));
            drop(held);
            rebuild.join().unwrap();
            removal.join().unwrap()
        });
        let back = removed.expect("the removal answers").expect("the removal gets the unit the rebuild evicted");
        assert_eq!(level(back), 0.5);
        assert_eq!(counts.stops(), 1, "stopped once, as it was evicted");
        assert!(slot.take_back().is_none(), "no unit left on the desk");

        // The slot works on: the next unit installs, and the next rebuild hands it back.
        let (unit, _) = constant(0.25);
        assert!(slot.install(unit, 44_100).is_ok());
        device.rebuild_at(48_000, 512);
        let Some(Back::Evicted(back)) = slot.take_back() else { panic!("the next eviction reaches its owner") };
        assert_eq!(level(back), 0.25);
    }

    #[test]
    fn with_no_device_running_the_host_services_the_slot_itself() {
        let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        device.stop();
        let slot = device.host().slot(1);
        let (unit, counts) = constant(1.0);
        assert!(slot.install(unit, 48_000).is_ok());
        assert!(slot.remove(Duration::from_millis(100)).unwrap().is_some());
        assert_eq!(counts.stops(), 1);
    }

    #[test]
    fn a_replace_commits_to_its_own_owner_and_retires_the_other() {
        let (device, a, counts_a, b) = playing();
        assert_ne!(a.tag(), b.tag(), "each handle is its own owner");
        let (unit, counts_b) = constant(0.5);
        assert!(b.replace(unit, 48_000, 0.8).is_ok());
        assert!(matches!(b.answer(PATIENCE), Answer::Committed));
        assert_eq!(a.standing(), Some(Standing::Retiring), "retired at the commit");
        assert!(matches!(a.answer(Duration::ZERO), Answer::Pending), "B's commit is not A's");
        let Back::Out(out) = eventually("A's unit fades out", || a.take_back()) else { panic!("retired, not evicted") };
        assert_eq!(counts_a.stops(), 1, "stopped once, after its fade");
        assert_eq!(level(out), 0.25);
        assert_eq!(a.standing(), Some(Standing::Retiring), "still retired once its unit is back");
        assert!(b.take_back().is_none(), "A's unit never reaches B");
        assert!(device.wait_blocks(5));
        assert!(counts_b.calls() > 0, "B's unit plays");
        let back = b.remove(PATIENCE).unwrap().expect("B's own unit");
        assert_eq!(level(back), 0.5);
    }

    #[test]
    fn a_replace_whose_unit_refuses_to_start_comes_back_and_the_playing_unit_plays_on() {
        let (device, a, counts_a, b) = playing();
        let (mut unit, counts_b) = constant(0.5);
        unit.refuse = true;
        assert!(b.replace(unit, 48_000, 0.8).is_ok());
        let (back, why) = refused(b.answer(PATIENCE));
        assert_eq!((why, level(back), counts_b.stops()), (Rejection::StartRefused, 0.5, 1));
        assert_eq!((a.standing(), b.standing()), (Some(Standing::Active), None));
        let calls = counts_a.calls();
        assert!(device.wait_blocks(5));
        assert!(counts_a.calls() > calls, "A plays on");
        assert_eq!(level(a.remove(PATIENCE).unwrap().expect("A's unit")), 0.25);
    }

    #[test]
    fn a_withdraw_before_the_commit_sends_the_unit_back_cancelled() {
        let (device, a, _, b) = playing();
        {
            // No block runs while the engine lock is held: the withdraw lands while the unit stages.
            let _held = device.host().core.rt.lock().unwrap();
            let (unit, _) = constant(0.5);
            assert!(b.replace(unit, 48_000, 0.8).is_ok());
            assert!(b.withdraw());
        }
        let (back, why) = refused(b.answer(PATIENCE));
        assert_eq!((why, level(back)), (Rejection::Cancelled, 0.5));
        assert_eq!((a.standing(), b.standing()), (Some(Standing::Active), None));
        assert_eq!(level(a.remove(PATIENCE).unwrap().expect("A's unit")), 0.25);
    }

    #[test]
    fn a_withdraw_after_the_commit_is_answered_committed_and_its_not_held_never_reaches_a_remove() {
        let (device, a, _, b) = playing();
        let (unit, _) = constant(0.5);
        assert!(b.replace(unit, 48_000, 0.8).is_ok());
        // The engine commits (and answers the withdraw `NotHeld`) before anyone reads the desk.
        assert!(device.wait_blocks(20));
        assert!(b.withdraw());
        assert!(device.wait_blocks(5));
        assert!(matches!(b.answer(PATIENCE), Answer::Committed));
        assert_eq!(a.standing(), Some(Standing::Retiring));
        let back = b.remove(PATIENCE).unwrap().expect("the NotHeld was the withdraw's: B's unit comes back");
        assert_eq!(level(back), 0.5);
    }

    #[test]
    fn a_withdraw_fired_by_the_owners_own_answer_timeout_cancels_the_staging_unit() {
        let (device, a, _, b) = playing();
        let held = device.host().core.rt.lock().unwrap();
        let (unit, _) = constant(0.5);
        assert!(b.replace(unit, 48_000, 0.8).is_ok());
        assert!(matches!(b.answer(Duration::from_millis(5)), Answer::Pending), "no block runs");
        assert!(b.withdraw());
        drop(held);
        let (back, why) = refused(b.answer(PATIENCE));
        assert_eq!((why, level(back)), (Rejection::Cancelled, 0.5));
        assert_eq!(a.standing(), Some(Standing::Active));
    }

    #[test]
    fn an_owner_evicted_then_replaced_is_retired_and_never_goes_in_again() {
        let (device, a, _, b) = playing();
        device.rebuild_at(44_100, 512);
        let (unit, _) = constant(0.5);
        assert!(b.replace(unit, 44_100, 0.8).is_ok(), "into the slot A's eviction left empty");
        assert!(matches!(b.answer(PATIENCE), Answer::Committed));
        assert_eq!(a.standing(), Some(Standing::Retiring), "retired with its unit still on the desk");
        device.rebuild_at(48_000, 512);
        let (again, _) = constant(0.1);
        let (again, why) = a.install(again, 48_000).expect_err("a replaced owner never installs again");
        assert!(why.contains("replaced"), "{why}");
        let Some(Back::Out(out)) = a.take_back() else { panic!("A's evicted unit comes back retired") };
        assert_eq!(level(out), 0.25);
        let (again, why) = a.install(again, 48_000).expect_err("nor once its unit is back");
        assert!(why.contains("replaced"), "{why}");
        let (_, why) = device.host().slot(0).install(again, 48_000).expect_err("B's eviction holds the slot");
        assert!(why.contains("already holds"), "{why}");
        let Some(Back::Evicted(back)) = b.take_back() else { panic!("B's unit goes in again") };
        assert!(b.install(back, 48_000).is_ok());
    }

    #[test]
    fn a_swaps_gain_enters_the_replay_even_when_a_rebuild_comes_before_the_answer_is_read() {
        let (device, _a, _, b) = playing();
        // Live, so the slot is heard: the rebuilt engine's level is what its replay set.
        for command in [Command::SetSlotGain(0, 0.3), Command::SetSlotLive(0, true)] {
            device.host().send(TimedCommand { frame: None, command }).unwrap();
        }
        let (unit, _) = constant(0.5);
        assert!(b.replace(unit, 48_000, 0.7).is_ok());
        assert!(device.wait_blocks(20));
        assert!(device.host().settings().contains(&Command::SetSlotGain(0, 0.3)), "nothing has read the desk yet");
        let swapped = heard(&device);
        assert!(swapped > 0.1, "B plays at the swap's gain: {swapped}");
        device.rebuild_at(44_100, 512);
        let replay = device.host().settings();
        assert!(replay.contains(&Command::SetSlotGain(0, 0.7)) && !replay.contains(&Command::SetSlotGain(0, 0.3)), "{replay:?}");
        assert!(matches!(b.answer(PATIENCE), Answer::Committed), "the answer waits on the desk across the rebuild");
        let Some(Back::Evicted(unit)) = b.take_back() else { panic!("B's unit was evicted") };
        assert!(b.install(unit, 44_100).is_ok());
        assert!(device.wait_blocks(20));
        let rebuilt = heard(&device);
        assert!((rebuilt - swapped).abs() < 0.01, "the new engine plays B at the swap's gain: {rebuilt}, not {swapped}");
    }

    #[test]
    fn a_commit_that_takes_the_slot_off_live_enters_the_replay() {
        let (device, _a, _, b) = playing();
        device.host().send(TimedCommand { frame: None, command: Command::SetSlotLive(0, true) }).unwrap();
        assert!(device.wait_blocks(5), "GO LIVE applies before the swap");
        let (mut unit, _) = constant(0.5);
        unit.kind = SlotKind::Instrument;
        assert!(b.replace(unit, 48_000, 0.7).is_ok(), "an instrument for the live effect");
        assert!(matches!(b.answer(PATIENCE), Answer::Committed));
        let replay = device.host().settings();
        assert!(replay.contains(&Command::SetSlotLive(0, false)) && !replay.contains(&Command::SetSlotLive(0, true)), "{replay:?}");
    }

    #[test]
    fn an_abandoned_owners_late_unit_leaks_and_the_slot_frees() {
        let (device, a, counts_a, _) = playing();
        {
            let _held = device.host().core.rt.lock().unwrap();
            assert!(a.remove(Duration::from_millis(20)).is_err(), "no block runs while the lock is held");
            a.abandon();
        }
        assert_eq!(a.standing(), Some(Standing::Abandoned));
        let (unit, _) = constant(0.5);
        install_eventually(&device.host().slot(0), unit, 48_000);
        assert_eq!((counts_a.stops(), counts_a.drops()), (1, 0), "stopped, then leaked: never dropped");
        assert_eq!(a.standing(), None);
    }

    #[test]
    fn a_replace_its_owner_abandoned_before_the_commit_is_removed_and_leaks() {
        let (device, a, _, b) = playing();
        let (unit, counts_b) = constant(0.5);
        {
            let _held = device.host().core.rt.lock().unwrap();
            assert!(b.replace(unit, 48_000, 0.8).is_ok());
            b.abandon();
        }
        let Back::Out(out) = eventually("A's unit is retired", || a.take_back()) else { panic!("retired, not evicted") };
        assert_eq!(level(out), 0.25);
        let (unit, _) = constant(0.1);
        install_eventually(&device.host().slot(0), unit, 48_000);
        assert_eq!((counts_b.stops(), counts_b.drops()), (1, 0), "removed at its commit, then leaked: never dropped");
        assert_eq!(b.standing(), None);
    }

    #[test]
    fn a_replace_at_a_replaced_rate_comes_back_with_its_unit_and_parks_nothing() {
        let (_device, a, _, b) = playing();
        let (unit, _) = constant(0.5);
        let (back, why) = b.replace(unit, 44_100, 0.8).expect_err("activated for another engine");
        assert!(why.starts_with("busy:"), "{why}");
        assert_eq!(level(back), 0.5);
        assert!(b.standing().is_none() && b.take_back().is_none(), "nothing of B's on the desk");
        assert_eq!(a.standing(), Some(Standing::Active));
    }

    #[test]
    fn a_rebuild_over_a_full_reply_ring_still_hands_the_parked_unit_to_its_owner() {
        let (mut device, a, counts_a, _) = playing();
        device.stop();
        // Four withdraws fill the message ring; the rebuild's service answers each `NotHeld`, which fills
        // the reply ring, so the eviction parks in the old engine behind them.
        for _ in 0..4 {
            assert!(a.withdraw());
        }
        device.rebuild_at(44_100, 512);
        let Some(Back::Evicted(back)) = a.take_back() else { panic!("the parked eviction reaches its owner") };
        assert_eq!((counts_a.stops(), counts_a.drops()), (1, 0), "stopped once; never dropped with the old engine");
        assert_eq!(level(back), 0.25);
    }

    #[test]
    fn a_desk_dropped_with_unread_units_leaks_them() {
        let (parked, in_mail) = constant(0.5);
        let (evicted, on_port) = constant(0.25);
        let (mut engine, handle) = Engine::new(EngineConfig { max_loop_seconds: 2.0, ..EngineConfig::new(48_000) });
        let [mut port, _] = handle.slots;
        assert!(port.install(2, evicted).is_ok());
        engine.service_slots_idle();
        engine.evict_slots();
        let mut desk = Desk::new(0);
        desk.park(1, parked);
        desk.port = Some(port);
        drop(desk);
        drop(engine);
        assert_eq!((in_mail.drops(), on_port.drops()), (0, 0), "leaked, from the mail and the port alike");
    }

    #[test]
    fn an_owner_that_abandons_a_committed_replace_on_a_stopped_device_has_its_unit_removed_and_leaked() {
        let (mut device, a, _, b) = playing();
        device.stop();
        let (unit, counts_b) = constant(0.5);
        assert!(b.replace(unit, 48_000, 0.8).is_ok(), "serviced at once: committed");
        assert_eq!(a.standing(), Some(Standing::Retiring), "A's read routed B's commit to B's mail");
        b.abandon();
        assert_eq!(b.standing(), None, "removed at once, nothing waits for a device");
        assert_eq!((counts_b.stops(), counts_b.drops()), (1, 0), "stopped, then leaked: never dropped");
        let (unit, _) = constant(0.1);
        assert!(device.host().slot(0).install(unit, 48_000).is_ok(), "the slot is free");
    }

    #[test]
    fn an_owner_that_abandons_a_committed_replace_on_a_running_device_has_its_unit_removed_and_leaked() {
        let (device, a, _, b) = playing();
        let (unit, counts_b) = constant(0.5);
        assert!(b.replace(unit, 48_000, 0.8).is_ok());
        eventually("A's read routes B's commit to B's mail", || (a.standing() == Some(Standing::Retiring)).then_some(()));
        b.abandon();
        let (unit, _) = constant(0.1);
        install_eventually(&device.host().slot(0), unit, 48_000);
        assert_eq!((counts_b.stops(), counts_b.drops()), (1, 0), "removed, then leaked: never dropped");
        assert_eq!(b.standing(), None);
    }

    #[test]
    fn an_install_on_a_stopped_device_services_the_remove_an_abandoned_commit_left_queued() {
        let (mut device, a, _, b) = playing();
        let core = device.host().core.clone();
        let held = core.rt.lock().unwrap();
        let (unit, counts_b) = constant(0.5);
        assert!(b.replace(unit, 48_000, 0.8).is_ok());
        b.abandon();
        // Stopped before any block took the replace: the next service commits it, its read queues the
        // remove, and only an install looks at the slot after that.
        device.stop();
        drop(held);
        let (unit, _) = constant(0.1);
        install_eventually(&device.host().slot(0), unit, 48_000);
        assert_eq!((counts_b.stops(), counts_b.drops()), (1, 0), "removed, then leaked: never dropped");
        let Some(Back::Out(out)) = a.take_back() else { panic!("A was retired by B's commit") };
        assert_eq!(level(out), 0.25);
    }

    #[test]
    fn a_retired_owners_remove_waits_for_its_unit_to_fade_out() {
        // 20 ms a block: the fade (480 frames) spans two, which the poll below cannot miss.
        let (device, a, counts_a, b) = playing_every(Duration::from_millis(20));
        let core = &device.host().core;
        let (unit, _) = constant(0.5);
        assert!(b.replace(unit, 48_000, 0.8).is_ok());
        // Held from the commit on: no block runs, so A's unit fades until it is released.
        let held = eventually("B's replace commits", || {
            let held = core.rt.lock().unwrap();
            (a.standing() == Some(Standing::Retiring)).then_some(held)
        });
        assert!(core.ports[0].lock().unwrap().in_engine(a.tag()), "caught while A's unit fades out");
        let removed = std::thread::scope(|s| {
            let removal = s.spawn(|| a.remove(PATIENCE));
            std::thread::sleep(Duration::from_millis(50));
            assert!(!removal.is_finished(), "the remove waits for the unit fading out");
            drop(held);
            removal.join().unwrap()
        });
        let back = removed.expect("comes back").expect("A's own unit");
        assert_eq!((level(back), counts_a.stops()), (0.25, 1));
        assert_eq!(a.standing(), Some(Standing::Retiring));
    }

    /// A retired owner reads Retiring once its unit is back, taken back or removed, and never installs
    /// again; its drop forgets it.
    #[test]
    fn a_retired_owner_stays_retiring_once_its_unit_is_back_until_it_drops() {
        for removed in [false, true] {
            let (_device, a, _, b) = playing();
            let (unit, _) = constant(0.5);
            assert!(b.replace(unit, 48_000, 0.8).is_ok());
            assert!(matches!(b.answer(PATIENCE), Answer::Committed));
            let back = if removed { a.remove_back(PATIENCE).unwrap() } else { Some(eventually("A's unit fades out", || a.take_back())) };
            assert!(matches!(back, Some(Back::Out(_))), "back retired");
            assert_eq!(a.standing(), Some(Standing::Retiring), "still retired, its unit back");
            assert!(a.remove_back(Duration::ZERO).unwrap().is_none(), "nothing left to remove");
            let (again, _) = constant(0.1);
            let (_, why) = a.install(again, 48_000).expect_err("a retired owner never installs again");
            assert!(why.contains("replaced"), "{why}");
            let (core, tag) = (a.core.clone(), a.tag());
            drop(a);
            assert_eq!(core.ports[0].lock().unwrap().standing(tag), None, "forgotten once its owner drops");
            assert!(b.remove(PATIENCE).unwrap().is_some());
        }
    }
}
