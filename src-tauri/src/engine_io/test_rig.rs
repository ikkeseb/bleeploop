//! Test-only: an engine in a [`Core`] rendered by a plain thread instead of a device, so a plugin
//! owner's install/remove/restart handshake runs against the real engine without hardware. The thread
//! takes the engine lock the way the callback does (`try_lock`: a miss renders nothing and counts),
//! publishes the frame clock, and keeps the left channel it rendered. [`TestDevice::rebuild_at`] swaps in
//! an engine at another rate through the device owner's own `swap_engine`, evicting the units;
//! [`TestDevice::stop`] and [`TestDevice::resume`] are a close and a reopen at the same rate.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::{Engine, EngineConfig, ProcessContext};

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
}

type Input = Arc<dyn Fn(Frame) -> f32 + Send + Sync>;

impl TestDevice {
    /// Build an engine at `rate` (2-second lanes, to keep tests light) and render `block`-frame blocks
    /// every `period`, with `input(frame)` as the device input.
    pub(crate) fn start(rate: u32, block: usize, period: Duration, input: impl Fn(Frame) -> f32 + Send + Sync + 'static) -> TestDevice {
        let core = Arc::new(Core::new());
        let config = EngineConfig { max_loop_seconds: 2.0, ..EngineConfig::new(rate) };
        let (engine, handle) = Engine::new(config);
        let [p0, p1] = handle.slots;
        core.ports[0].lock().unwrap().port = Some(p0);
        core.ports[1].lock().unwrap().port = Some(p1);
        let scope_bin = lf_engine::scope::scope_bin_frames(rate) as u32;
        *core.ends.lock().unwrap() =
            Some(Ends { commands: handle.commands, pushed: 0, events: handle.events, overview: handle.overview, session: Some(handle.session), scope: handle.scope, scope_bin });
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
                    match core.rt.try_lock() {
                        Ok(mut rt) => {
                            if let Some(engine) = rt.engine.as_mut() {
                                let ctx = ProcessContext { frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
                                engine.process(&ctx, &x, &mut l, &mut r);
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
                    std::thread::sleep(period);
                }
            })
            .expect("spawn the test device");
        self.join = Some(join);
    }

    pub(crate) fn host(&self) -> &EngineHost {
        &self.host
    }

    /// A device at another rate: a new engine (2-second lanes, `max_block`) replaces the old one, whose
    /// units go to their slots' desks as evictions, while the render thread keeps rendering (it
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
        let target = self.blocks.load(Relaxed) + n;
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
