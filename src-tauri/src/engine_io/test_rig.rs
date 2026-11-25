//! Test-only: an engine in a [`Core`] rendered by a plain thread instead of a device, so a plugin
//! owner's install/remove/restart handshake runs against the real engine without hardware. The thread
//! takes the engine lock the way the callback does (`try_lock`: a miss renders nothing and counts),
//! publishes the frame clock, and keeps the left channel it rendered. [`TestDevice::rebuild_at`] swaps in
//! an engine at another rate through the device owner's own `swap_engine`, evicting the units;
//! [`TestDevice::stop`] and [`TestDevice::resume`] are a close and a reopen at the same rate.
//!
//! The device renders at its period in wall time, and a loaded runner stretches every period: a wait
//! through engine frames that a test only waits out (a take's count-in) is rendered back to back
//! instead ([`TestDevice::rush_until`]), so its wall time no longer scales with the block count. A take
//! is started and stopped here ([`start_take`], [`stop_take`]) for every test that holds one.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::{Command, Engine, EngineConfig, LaneState, ProcessContext, TimedCommand};

use super::{Core, EngineHost, Ends};

/// How long a test waits for something that must happen: far past any loaded machine, never reached by a pass.
pub(crate) const PATIENCE: Duration = Duration::from_secs(30);

/// Frames of the left channel a [`TestDevice`] keeps (the most recent).
const KEEP: usize = 1 << 20;

pub(crate) struct TestDevice {
    host: EngineHost,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// Blocks rendered so far.
    pub(crate) blocks: Arc<AtomicU64>,
    /// The left channel rendered, oldest first (capped at `KEEP` frames).
    pub(crate) output: Arc<Mutex<Vec<f32>>>,
    /// The device frame the next block renders: a resume continues there.
    frame: Arc<AtomicI64>,
    block: usize,
    period: Duration,
    input: Input,
    /// While set, the render thread renders back to back until this holds of the engine after a block.
    rush: Rush,
}

type Input = Arc<dyn Fn(Frame) -> f32 + Send + Sync>;
type Rush = Arc<Mutex<Option<fn(&Engine) -> bool>>>;

impl TestDevice {
    /// Build an engine at `rate` (2-second lanes, to keep tests light) and render `block`-frame blocks
    /// every `period`, with `input(frame)` as the device input.
    pub(crate) fn start(rate: u32, block: usize, period: Duration, input: impl Fn(Frame) -> f32 + Send + Sync + 'static) -> TestDevice {
        let cap = EngineConfig::new(rate).event_capacity;
        TestDevice::start_with_events(rate, block, period, cap, input)
    }

    /// [`TestDevice::start`] with an explicit event ring capacity (e.g. a small ring to test full-ring
    /// recovery without filling 4096 events).
    pub(crate) fn start_with_events(
        rate: u32,
        block: usize,
        period: Duration,
        event_capacity: usize,
        input: impl Fn(Frame) -> f32 + Send + Sync + 'static,
    ) -> TestDevice {
        let core = Arc::new(Core::new());
        let config = EngineConfig { max_loop_seconds: 2.0, event_capacity, ..EngineConfig::new(rate) };
        let (engine, handle) = Engine::new(config);
        let [p0, p1] = handle.slots;
        *core.ports[0].lock().unwrap() = Some(p0);
        *core.ports[1].lock().unwrap() = Some(p1);
        let scope_bin = lf_engine::scope::scope_bin_frames(rate) as u32;
        *core.ends.lock().unwrap() =
            Some(Ends { commands: handle.commands, pushed: 0, events: handle.events, overview: handle.overview, session: Some(handle.session), scope: handle.scope, scope_bin, tuner: handle.tuner, rate, unit_meters: handle.unit_meters });
        core.rt.lock().unwrap().engine = Some(engine);
        core.rate.store(rate, Relaxed);
        core.max_block.store(config.max_block as u32, Relaxed);
        let mut device = TestDevice {
            host: EngineHost { core },
            stop: Arc::new(AtomicBool::new(false)),
            join: None,
            blocks: Arc::new(AtomicU64::new(0)),
            output: Arc::new(Mutex::new(Vec::new())),
            frame: Arc::new(AtomicI64::new(0)),
            block,
            period,
            input: Arc::new(input),
            rush: Arc::new(Mutex::new(None)),
        };
        device.resume();
        device
    }

    /// Start rendering again after [`TestDevice::stop`], from the frame where it stopped (the engine and
    /// its slots kept, as a reopen at the same rate keeps them). `running` goes up under the engine
    /// lock, as the device owner raises it.
    pub(crate) fn resume(&mut self) {
        if self.join.is_some() {
            return;
        }
        {
            let _rt = self.host.core.rt.lock().unwrap();
            self.host.core.running.store(true, Release);
        }
        self.stop.store(false, Release);
        let core = self.host.core.clone();
        let (stop, blocks, output, frame_at, input) = (self.stop.clone(), self.blocks.clone(), self.output.clone(), self.frame.clone(), self.input.clone());
        let rush = self.rush.clone();
        let (block, period) = (self.block, self.period);
        let join = std::thread::Builder::new()
            .name("lf-test-device".into())
            .spawn(move || {
                let (mut x, mut l, mut r) = (vec![0.0f32; block], vec![0.0f32; block], vec![0.0f32; block]);
                while !stop.load(Acquire) {
                    let frame = frame_at.load(Relaxed);
                    let entry = Instant::now();
                    for (k, s) in x.iter_mut().enumerate() {
                        *s = input(frame + k as Frame);
                    }
                    let rate = core.rate.load(Relaxed);
                    let mut rendered = false;
                    match core.rt.try_lock() {
                        Ok(mut rt) => {
                            if let Some(engine) = rt.engine.as_mut() {
                                rendered = true;
                                let ctx = ProcessContext { frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
                                engine.process(&ctx, &x, &mut l, &mut r);
                                let mut until = rush.lock().unwrap_or_else(|e| e.into_inner());
                                if until.is_some_and(|done| done(engine)) {
                                    *until = None;
                                }
                            }
                        }
                        Err(_) => {
                            core.counters.lock_misses.fetch_add(1, Relaxed);
                            l.fill(0.0);
                        }
                    }
                    core.clock.publish(entry, frame, block as u32, rate);
                    core.counters.callbacks.fetch_add(1, Relaxed);
                    {
                        let mut out = output.lock().unwrap();
                        out.extend_from_slice(&l);
                        if out.len() > KEEP {
                            let extra = out.len() - KEEP;
                            out.drain(..extra);
                        }
                    }
                    frame_at.store(frame + block as Frame, Relaxed);
                    blocks.fetch_add(1, Relaxed);
                    if rush.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
                        std::thread::sleep(period);
                    } else if !rendered {
                        // The engine lock's holder (a swap, a host call) needs the CPU to let it go.
                        std::thread::yield_now();
                    }
                    // Rushing, a rendered block goes straight on to the next: a yield between blocks
                    // gave the CPU away on a loaded runner, 494 blocks in 30 s (4f24c28, run 45714981558).
                }
            })
            .expect("spawn the test device");
        self.join = Some(join);
    }

    pub(crate) fn host(&self) -> &EngineHost {
        &self.host
    }

    /// Render back to back (no period and no yield, so one time slice renders a count-in's few hundred
    /// blocks) until `done` holds of the engine after a block, then at the period again, from that block
    /// on. A miss of the engine lock renders nothing, yields and keeps rushing. What a test does after `done` holds runs at the device's own pace.
    pub(crate) fn rush_until(&self, done: fn(&Engine) -> bool) {
        *self.rush.lock().unwrap_or_else(|e| e.into_inner()) = Some(done);
    }

    /// The device and its engine as a timed-out wait reports them: the blocks rendered, the device
    /// frame, the engine lock's misses, whether it still rushes, the engine generation, the commands
    /// pushed against those the engine took, and lane 0 in the overview the feed reads.
    pub(crate) fn describe(&self) -> String {
        let core = &self.host.core;
        let rushing = self.rush.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        let mut text = format!(
            "{} blocks to frame {}, {} engine lock misses, {}rushing, engine gen {}",
            self.blocks.load(Relaxed),
            self.frame.load(Relaxed),
            core.counters.lock_misses.load(Relaxed),
            if rushing { "" } else { "not " },
            core.engine_gen.load(Acquire),
        );
        match core.ends.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(ends) => {
                let lane = ends.overview.lane(0);
                text += &format!(
                    ", {} commands pushed, {} taken, lane 0 {:?} at {} frames",
                    ends.pushed,
                    ends.overview.commands_taken(),
                    lane.state,
                    lane.frames
                );
            }
            None => text += ", no overview (no engine)",
        }
        text
    }

    /// A device at another rate: a new engine (2-second lanes, `max_block`) replaces the old one, whose
    /// units go to their slot hosts' eviction mailboxes, while the render thread keeps rendering (it
    /// renders nothing during the swap, which counts lock misses).
    pub(crate) fn rebuild_at(&self, rate: u32, max_block: usize) {
        let config = EngineConfig { max_loop_seconds: 2.0, max_block, ..EngineConfig::new(rate) };
        let (engine, handle) = Engine::new(config);
        drop(super::owner::swap_engine(&self.host.core, engine, handle, config));
    }

    /// Stop rendering (the device is gone; the engine and its slots stay). The render thread is joined
    /// before `running` drops, as the device owner does: a slot host that sees it down takes the engine
    /// lock itself, which must never happen under a live render.
    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        self.host.core.running.store(false, Release);
        self.host.core.clock.clear();
    }

    /// Wait until `n` more blocks have rendered (or [`PATIENCE`] passed): true when they did. The bound
    /// is a ceiling a starved render thread on a loaded machine still meets; a pass never waits for it.
    pub(crate) fn wait_blocks(&self, n: u64) -> bool {
        self.wait_block_count(self.blocks.load(Relaxed) + n)
    }

    /// Wait until `target` blocks have rendered in all (or [`PATIENCE`] passed): true when they did. For
    /// a count read before the render could move it ([`TestDevice::resume`] renders its first block at
    /// once), where [`TestDevice::wait_blocks`] would count from after that block.
    pub(crate) fn wait_block_count(&self, target: u64) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while self.blocks.load(Relaxed) < target {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }
}

impl Drop for TestDevice {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Lane 0 records a take: past its count-in, it has captured frames.
pub(crate) fn take_records(engine: &Engine) -> bool {
    let lane = engine.looper().overview().lane(0);
    lane.state == LaneState::Recording && lane.frames > 0
}

/// Start a free take on lane 0 (240 BPM: a one-bar count-in of 48,000 frames at 48 kHz) and wait
/// until it records. The count-in renders back to back ([`TestDevice::rush_until`]): at a 64-frame,
/// 5 ms device it is 750 blocks, which a loaded runner stretched past [`PATIENCE`]. The take itself
/// runs at the device's pace, so it lasts as long in wall time as the test expects.
pub(crate) fn start_take(device: &TestDevice) {
    let host = device.host();
    for command in [Command::SetBpm(240.0), Command::SetFixedLength(false), Command::RecDub(0)] {
        host.send(TimedCommand { frame: None, command }).unwrap();
    }
    device.rush_until(take_records);
    wait_lane0(device, "the take records", |state, frames| state == LaneState::Recording && frames > 0);
}

/// Stop lane 0's take and wait until it no longer records.
pub(crate) fn stop_take(device: &TestDevice) {
    device.host().send(TimedCommand { frame: None, command: Command::Stop(0) }).unwrap();
    wait_lane0(device, "the take stops", |state, _| state != LaneState::Recording);
}

/// Drain the feed until lane 0 in its overview satisfies `cond` ([`PATIENCE`] at most), else panic with
/// what the device and the engine show ([`TestDevice::describe`]).
fn wait_lane0(device: &TestDevice, what: &str, cond: impl Fn(LaneState, Frame) -> bool) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let (_, overview) = device.host().drain_feed(&mut Vec::new());
        if overview.is_some_and(|ov| cond(ov.lane(0).state, ov.lane(0).frames)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}; {}", device.describe());
        std::thread::sleep(Duration::from_millis(2));
    }
}
