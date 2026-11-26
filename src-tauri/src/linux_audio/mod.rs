//! JACK owns the engine while the graph runs. The host keeps the rings and moves the same engine
//! between devices of the same sample rate, so closing or changing a route preserves the loops.
//! Session bytes, feed frames, note ownership and persisted engine settings share the Windows code.

mod device;
pub(crate) use device::{in_rt_callback, list_inputs, list_outputs};
#[path = "../engine_io/ends.rs"]
mod ends;
#[path = "../engine_io/feed.rs"]
#[allow(dead_code)] // Folder-dialog feed holds are Windows-only.
mod feed;
mod input;
pub(crate) mod ipc;
#[allow(dead_code)] // Shared MIDI parsing and hardware-owner routes; JACK currently takes page notes.
#[path = "../engine_io/midi/parse.rs"]
mod parse;
#[allow(dead_code)]
#[path = "../engine_io/midi/queue.rs"]
mod queue;
#[allow(dead_code)]
#[path = "../engine_io/midi/router.rs"]
mod router;
#[path = "../engine_io/session.rs"]
mod session;
#[path = "../engine_io/settings.rs"]
mod settings;
#[path = "../engine_io/midi/ui_route.rs"]
mod ui_route;
#[path = "../engine_io/wire.rs"]
mod wire;

use lf_engine::{scope::ScopeBin, tuner::TunerFrame};
use lf_engine::{
    Command, Engine, EngineConfig, EngineHandle, Event, LaneState, TimedCommand, UnitMeter,
    SLOT_COUNT, TRACK_COUNT,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::{Arc, Mutex, MutexGuard};
use tauri::Manager;

use device::{Device, DeviceRequest, DeviceStatus, Prepared};
use ends::{Ends, FeedDrain};
use feed::FeedThread;
use settings::Settings;

pub(crate) struct App {
    host: EngineHost,
    feed: FeedThread,
}

pub(crate) fn setup(app: &tauri::App) -> std::io::Result<()> {
    let host = EngineHost::default();
    let feed = FeedThread::spawn(host.clone())?;
    app.manage(App { host, feed });
    Ok(())
}

pub(crate) fn shutdown(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<App>() {
        state.host.core.exiting.store(true, Release);
        state.feed.stop();
        state.host.close();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Default)]
struct EngineHost {
    core: Arc<Core>,
}

// Only open, close, fault teardown and channel changes take device_op. The controls and feed
// keep reading the previous device while a silent candidate negotiates with JACK. Lock order
// in a transition is device_op → session_busy (only across a rate change) → run → settings → ends.
// No audio callback takes these locks.
#[derive(Default)]
struct Core {
    device_op: Mutex<()>,
    exiting: AtomicBool,
    run: Mutex<Run>,
    settings: Mutex<Settings>,
    ends: Mutex<Option<Ends>>,
    session_busy: Mutex<(u64, usize)>,
    master_render: Mutex<()>,
    engine_gen: AtomicU64,
    tuner_fence: AtomicI64,
    tuner_breaks: AtomicU64,
    chains: Arc<[chain::ChainState; SLOT_COUNT]>,
}

impl Core {
    fn rate(&self) -> Option<u32> {
        lock(&self.ends).as_ref().map(|e| e.rate)
    }
}

#[derive(Default)]
struct Run {
    device: Option<Device>,
    request: Option<DeviceRequest>,
    idle: Option<Box<Engine>>,
    frame: i64,
    events: Vec<DeviceEvent>,
    input: input::Input,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum DeviceEvent {
    EngineFaulted,
    Lost { backend: String, reason: String },
}

#[derive(Serialize)]
pub(crate) enum OpenError {
    RateChange {
        device: String,
        from: u32,
        to: u32,
    },
    #[serde(untagged)]
    Failed(String),
}

impl From<String> for OpenError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

// Linux uses the legacy built-in synths and input FX. Both chain racks in the shared feed are empty.
mod chain {
    use super::*;
    #[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
    pub struct ChainState {
        units: [(); 0],
        latency: i64,
        held: bool,
    }

    pub struct ChainRev {
        pub rev: u64,
        pub chains: Arc<[ChainState; SLOT_COUNT]>,
    }
}

fn engine_ends(handle: EngineHandle, rate: u32) -> Ends {
    Ends {
        commands: handle.commands,
        pushed: 0,
        events: handle.events,
        overview: handle.overview,
        session: Some(handle.session),
        scope: handle.scope,
        scope_bin: lf_engine::scope::scope_bin_frames(rate) as u32,
        tuner: handle.tuner,
        rate,
        unit_meters: handle.unit_meters,
    }
}

impl EngineHost {
    fn open(&self, request: DeviceRequest, force: bool) -> Result<DeviceStatus, OpenError> {
        self.open_with(request, force, Prepared::new)
    }

    fn open_with(
        &self,
        request: DeviceRequest,
        force: bool,
        prepare: impl FnOnce(&DeviceRequest) -> Result<Prepared, String>,
    ) -> Result<DeviceStatus, OpenError> {
        let _op = lock(&self.core.device_op);
        if self.core.exiting.load(Acquire) {
            return Err("the app is closing".to_string().into());
        }
        self.retire_fault();
        {
            let mut run = lock(&self.core.run);
            if let Some(device) = run.device.as_ref().filter(|d| {
                run.request
                    .as_ref()
                    .is_some_and(|old| d.can_keep(old, &request))
            }) {
                let status = device.status();
                run.request = Some(request);
                return Ok(status);
            }
        }

        let mut route = prepare(&request)?;
        self.retire_fault();
        // A rate change replaces the session port. Finish any outstanding session job first and
        // exclude a load between the rate refusal check and the handoff. Same-rate opens retain
        // the port, so snapshot and import can continue throughout their negotiation.
        let rate = route.rate();
        let replacing = self.core.rate().is_some_and(|old| old != rate);
        let _session = replacing.then(|| lock(&self.core.session_busy));
        if replacing && !force {
            self.check_rate(rate)?;
        }
        let previous = {
            let run = lock(&self.core.run);
            run.device.as_ref().and(run.request.clone())
        };
        // The server's quantum is global. A rate refusal must precede the setter. When the
        // previous client's clock can still render, punch it out and check its final lanes too.
        if replacing {
            self.stop_device(None);
        }
        let switched = (|| {
            if replacing && !force {
                self.check_rate(rate)?;
            }
            route.request_buffer(request.buffer)?;
            self.stop_device(None);
            route.settle()?;
            self.install(route, request).map_err(OpenError::from)
        })();
        if switched.is_err() && lock(&self.core.run).device.is_none() {
            if let Some(old) = previous {
                let restored = (|| -> Result<(), String> {
                    let mut rollback = Prepared::new(&old)?;
                    if self.core.rate().is_some_and(|rate| rate != rollback.rate()) {
                        return Err("the previous JACK route changed sample rate".into());
                    }
                    rollback.request_buffer(old.buffer)?;
                    rollback.settle()?;
                    self.install(rollback, old).map(drop)
                })();
                if let Err(why) = restored {
                    log::warn!("JACK could not restore the previous route: {why}");
                    lock(&self.core.run).events.push(DeviceEvent::Lost {
                        backend: "Jack".into(),
                        reason: why,
                    });
                }
            }
        }
        switched
    }

    fn check_rate(&self, rate: u32) -> Result<(), OpenError> {
        let ends = lock(&self.core.ends);
        if let Some(old) = ends.as_ref().filter(|e| {
            e.rate != rate && (0..TRACK_COUNT).any(|i| e.overview.lane(i).state != LaneState::Empty)
        }) {
            return Err(OpenError::RateChange {
                device: "JACK".into(),
                from: old.rate,
                to: rate,
            });
        }
        Ok(())
    }

    // A joined callback may have panicked during negotiation. Decide on reuse after the
    // join: a poisoned engine was discarded, so even a same-rate candidate gets a fresh one.
    fn install(&self, route: Prepared, request: DeviceRequest) -> Result<DeviceStatus, String> {
        let rate = route.rate();
        let retained = {
            let mut run = lock(&self.core.run);
            if self.core.rate() == Some(rate) {
                run.idle.take().map(|e| (e, run.frame))
            } else {
                None
            }
        };
        let device = if let Some((engine, frame)) = retained {
            match route.start(engine, frame) {
                Ok(device) => device,
                Err((why, engine)) => {
                    lock(&self.core.run).idle = Some(engine);
                    return Err(why);
                }
            }
        } else {
            // Pool allocation is off the run lock. A snapshot of the previous session still has
            // its retained engine here until this candidate takes over.
            let (engine, handle) = Engine::new(EngineConfig::new(rate));
            let mut ends = engine_ends(handle, rate);
            let mut run = lock(&self.core.run);
            let mut settings = lock(&self.core.settings);
            let replay: Vec<_> = settings.replay().collect();
            for &command in &replay {
                ends.commands
                    .push(TimedCommand {
                        frame: None,
                        command,
                    })
                    .map_err(|_| "settings exceed the engine command ring".to_string())?;
                ends.pushed += 1;
            }
            // start only validates the ready route and submits the engine; every graph wait
            // and the old client's close completed before this short handoff.
            let device = route.start(Box::new(engine), 0).map_err(|(why, _)| why)?;
            let mut existing = lock(&self.core.ends);
            let gen = self.core.engine_gen.fetch_add(1, Release) + 1;
            settings.follow(gen);
            for command in replay {
                settings.pushed(&command);
            }
            *existing = Some(ends);
            drop(existing);
            drop(settings);
            let old_engine = run.idle.take();
            run.frame = 0;
            run.input.rebuilt(self);
            drop(run);
            drop(old_engine);
            device
        };
        let status = device.status();
        let mut run = lock(&self.core.run);
        run.device = Some(device);
        run.request = Some(request);
        Ok(status)
    }

    // The caller owns device_op. During jack_client_close the old rings stay reachable,
    // and once it joins, session jobs can be serviced on the stopped engine.
    fn stop_device(&self, reason: Option<&str>) {
        let device = lock(&self.core.run).device.take();
        let Some(device) = device else {
            return;
        };
        let (mut engine, frame, faulted) = device.stop();
        if faulted {
            let mut run = lock(&self.core.run);
            run.events.push(DeviceEvent::EngineFaulted);
            let mut ends = lock(&self.core.ends);
            self.core.engine_gen.fetch_add(1, Release);
            *ends = None;
            run.idle = None;
            run.frame = 0;
            return;
        }
        engine.punch_out();
        let mut run = lock(&self.core.run);
        run.frame = frame;
        run.idle = Some(engine);
        self.core.tuner_fence.store(frame, Relaxed);
        self.core.tuner_breaks.fetch_add(1, Release);
        if let Some(why) = reason {
            run.events.push(DeviceEvent::Lost {
                backend: "Jack".into(),
                reason: why.into(),
            });
        }
    }

    fn close(&self) {
        let _op = lock(&self.core.device_op);
        self.stop_device(None);
    }

    fn set_channel(&self, slot: usize, channel: Option<device::InputPick>) -> Result<(), String> {
        let _op = lock(&self.core.device_op);
        self.retire_fault();
        let mut run = lock(&self.core.run);
        let device = run.device.as_mut().ok_or("no JACK device is open")?;
        device.set_channel(slot, channel)?;
        if let Some(request) = &mut run.request {
            request.input_channels[slot] = channel;
        }
        Ok(())
    }

    fn status(&self) -> Option<DeviceStatus> {
        self.poll_fault();
        lock(&self.core.run)
            .device
            .as_ref()
            .filter(|d| d.fault().is_none())
            .map(Device::status)
    }

    fn feed_anchor(&self) -> Option<(i64, f64, u32)> {
        let run = lock(&self.core.run);
        let device = run.device.as_ref()?;
        let (frame, at_ms) = device.anchor()?;
        Some((frame, at_ms, device.status().sample_rate))
    }

    // Health polling never waits behind an open. Its owner checks again after prepare, and
    // stop_device always inspects the callback's outcome after it joins.
    fn poll_fault(&self) {
        if let Ok(_op) = self.core.device_op.try_lock() {
            self.retire_fault();
        }
    }

    fn retire_fault(&self) {
        let reason = lock(&self.core.run).device.as_ref().and_then(Device::fault);
        if let Some(reason) = reason {
            log::warn!("JACK device lost: {reason}");
            self.stop_device(Some(reason));
        }
    }

    fn publish_if_idle(&self) {
        self.poll_fault();
        let mut run = lock(&self.core.run);
        run.input.drain(self);
        if let Some(engine) = run.idle.as_mut() {
            engine.publish_idle();
        }
    }

    fn service_session_if_idle(&self) {
        self.poll_fault();
        if let Some(engine) = lock(&self.core.run).idle.as_mut() {
            engine.service_session_idle();
        }
    }

    fn settings(&self) -> Vec<Command> {
        lock(&self.core.settings).replay().collect()
    }
    fn mixes(&self) -> Vec<Event> {
        lock(&self.core.settings).mixes().collect()
    }
    fn take_device_events(&self) -> Vec<DeviceEvent> {
        std::mem::take(&mut lock(&self.core.run).events)
    }
    fn chain_told(&self, _rev: u64, _seq: u64) {}
    fn take_meter(&self) -> ([[f32; 2]; SLOT_COUNT], [bool; SLOT_COUNT]) {
        lock(&self.core.run)
            .device
            .as_ref()
            .map(Device::meter)
            .unwrap_or_default()
    }

    fn send(&self, cmd: TimedCommand) -> Result<(), ()> {
        let mut settings = lock(&self.core.settings);
        let mut ends = lock(&self.core.ends);
        if let Some(ends) = ends.as_mut() {
            ends.commands.push(cmd).map_err(|_| ())?;
            ends.pushed += 1;
            settings.record(&cmd.command);
            settings.pushed(&cmd.command);
        } else if !settings.record(&cmd.command)
            && !matches!(
                cmd.command,
                Command::NoteOff(_) | Command::AllNotesOff | Command::Press
            )
        {
            return Err(());
        }
        Ok(())
    }

    fn drain_feed_taps(
        &self,
        out: &mut Vec<Event>,
        columns: Option<&mut Vec<ScopeBin>>,
        tuner: Option<&mut Vec<TunerFrame>>,
        meters: Option<&mut Vec<UnitMeter>>,
    ) -> FeedDrain {
        let mut settings = lock(&self.core.settings);
        let mut ends = lock(&self.core.ends);
        let gen = self.core.engine_gen.load(Acquire);
        let mut batch = FeedDrain {
            gen,
            overview: None,
            rate: 0,
            scope_bin: 0,
            chains: chain::ChainRev {
                rev: 0,
                chains: self.core.chains.clone(),
            },
            stash_lost: false,
        };
        if let Some(e) = ends.as_mut() {
            batch.overview = Some(e.overview.clone());
            batch.rate = e.rate;
            batch.scope_bin = e.scope_bin;
            while let Ok(event) = e.events.pop() {
                match event {
                    Event::Mix { frame, lane, mix } => {
                        settings.mixed(gen, frame, lane, &mix);
                    }
                    Event::Transport { bpm, meter, .. } => {
                        settings.transported(gen, bpm, meter);
                    }
                    Event::Toggled { toggle, on, .. } => {
                        settings.toggled(gen, toggle, on);
                    }
                    Event::Refused { reason, .. } => settings.refused(gen, reason),
                    Event::Seat { .. } | Event::Chain { .. } | Event::UnitParam { .. } => continue,
                    _ => {}
                }
                out.push(event);
            }
            if let Some(columns) = columns {
                while let Ok(column) = e.scope.pop() {
                    columns.push(column);
                }
            }
            if let Some(tuner) = tuner {
                while let Ok(frame) = e.tuner.pop() {
                    if tuner.len() < tuner.capacity() {
                        tuner.push(frame);
                    }
                }
            }
            if let Some(meters) = meters {
                while let Ok(meter) = e.unit_meters.pop() {
                    meters.push(meter);
                }
            }
        }
        batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_engine::ProcessContext;
    use serde_json::{json, Value};
    use tauri::ipc::Channel;

    // Exercise the page's real input wire, render the engine with known capture samples instead
    // of a sound card, and read the same session bytes the Export button requests.
    fn record_from_page(builtin: bool) {
        const RATE: u32 = 16_000;
        const BLOCK: usize = 128;
        let host = EngineHost::default();
        let mut config = EngineConfig::new(RATE);
        config.max_loop_seconds = 3.0;
        let (mut engine, handle) = Engine::new(config);
        *lock(&host.core.ends) = Some(engine_ends(handle, RATE));
        let mut input = input::Input::default();
        let epoch = input.subscribe(&host, 1.0, Channel::new(|_| Ok(())), true);
        let mut send = |payload: Value| {
            assert_eq!(
                input
                    .send(&host, epoch, serde_json::from_value(payload).unwrap(), true)
                    .unwrap(),
                None
            );
        };
        send(json!([
            {"engine":{"SetBpm":120}}, {"engine":{"SetFixedLength":true}},
            {"engine":{"SetFixedBars":1}}, {"engine":{"SetSlotLive":[0,!builtin]}},
            {"engine":{"SetMetronome":false}},
            {"input":{"selectTarget":{"slot":0,"target":{"Builtin":"organ"}}}},
            {"engine":{"RecDub":0}}
        ]));
        if builtin {
            send(
                json!([{"input":{"note":{"owner":"key:KeyA","note":60,"velocity":100,"on":true}}}]),
            );
        }
        let capture = if builtin {
            [0.0; BLOCK]
        } else {
            [0.125; BLOCK]
        };
        let silent = [0.0; BLOCK];
        let mut l = [0.0; BLOCK];
        let mut r = [0.0; BLOCK];
        let mut frame = 0;
        let mut render = |engine: &mut Engine, capture: &[f32; BLOCK]| {
            let cx = ProcessContext {
                frame,
                xrun: false,
                damaged: false,
                align_frames: 0,
                input_frames: 0,
            };
            engine.process_inputs(
                &cx,
                [[capture, capture], [&silent, &silent]],
                &mut l,
                &mut r,
            );
            frame += BLOCK as i64;
            l.iter().map(|s| s * s).sum::<f32>()
        };
        // Two bars for the count and fixed first take, then wait past the limiter's pre-delay.
        for _ in 0..530 {
            render(&mut engine, &capture);
        }
        assert_eq!(engine.looper().master(), 32_000);
        send(json!([{"input":"allNotesOff"}, {"engine":{"SetSlotLive":[0,false]}}]));
        for _ in 0..260 {
            render(&mut engine, &silent);
        }
        let played: f32 = (0..250).map(|_| render(&mut engine, &silent)).sum();
        assert!(
            played > 1.0,
            "the loop must remain audible with its input silenced: {played}"
        );
        lock(&host.core.run).idle = Some(Box::new(engine));
        let bytes = host.snapshot(false).unwrap();
        let size = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let header: Value = serde_json::from_slice(&bytes[4..4 + size]).unwrap();
        assert_eq!(header["tracks"][0]["frames"], 32_000);
        let samples: Vec<_> = bytes[4 + size..]
            .chunks_exact(4)
            .map(|s| f32::from_le_bytes(s.try_into().unwrap()))
            .collect();
        assert_eq!(samples.len(), 2 * 32_000);
        if builtin {
            assert!(
                samples.iter().any(|s| *s > 0.01) && samples.iter().any(|s| *s < -0.01),
                "an oscillating synth stem"
            );
        } else {
            assert!(
                samples.iter().all(|s| (*s - 0.125).abs() < 1e-6),
                "the empty live slot preserved its dry input"
            );
        }
    }

    // A gated device enumeration reproduces a JACK server taking its time without hardware.
    // The old engine already sounds a page note. Its session copy and that key's release must
    // both finish before we let the candidate proceed. A close must instead wait in order.
    #[test]
    fn slow_open_keeps_input_feed_and_session_responsive_and_serializes_close() {
        use std::sync::mpsc;
        use std::time::Duration;

        let host = EngineHost::default();
        let mut config = EngineConfig::new(16_000);
        config.max_loop_seconds = 1.0;
        let (mut engine, handle) = Engine::new(config);
        *lock(&host.core.ends) = Some(engine_ends(handle, 16_000));
        let epoch = {
            let mut run = lock(&host.core.run);
            let epoch = run
                .input
                .subscribe(&host, 1.0, Channel::new(|_| Ok(())), true);
            let press = serde_json::from_value(
                json!([{"input":{"note":{"owner":"key:A","note":60,"velocity":90,"on":true}}}]),
            )
            .unwrap();
            assert_eq!(run.input.send(&host, epoch, press, true).unwrap(), None);
            epoch
        };
        let cx = ProcessContext {
            frame: 0,
            xrun: false,
            damaged: false,
            align_frames: 0,
            input_frames: 0,
        };
        engine.process(&cx, &[0.0; 128], &mut [0.0; 128], &mut [0.0; 128]);
        lock(&host.core.run).idle = Some(Box::new(engine));

        let req: DeviceRequest = serde_json::from_value(json!({"backend":"Jack"})).unwrap();
        let (started, preparing) = mpsc::sync_channel(0);
        let (resume, gate) = mpsc::channel();
        let (response, received) = mpsc::channel();
        let (closed, closing) = mpsc::channel();
        let (closing_now, close_attempt) = mpsc::sync_channel(0);
        std::thread::scope(|scope| {
            let start_host = host.clone();
            let open = scope.spawn(move || {
                start_host.open_with(req, false, |_| {
                    started.send(()).unwrap();
                    gate.recv_timeout(Duration::from_secs(5)).unwrap();
                    Err("gated open".into())
                })
            });
            preparing.recv_timeout(Duration::from_secs(2)).unwrap();
            let close_host = host.clone();
            scope.spawn(move || {
                closing_now.send(()).unwrap();
                close_host.close();
                closed.send(()).unwrap();
            });
            let play_host = host.clone();
            scope.spawn(move || {
                play_host.publish_if_idle();
                assert!(play_host.status().is_none());
                play_host.snapshot(false).unwrap();
                let release = serde_json::from_value(
                    json!([{"input":{"note":{"owner":"key:A","note":60,"velocity":0,"on":false}}}]),
                )
                .unwrap();
                let result = lock(&play_host.core.run)
                    .input
                    .send(&play_host, epoch, release, false)
                    .unwrap();
                let ends = lock(&play_host.core.ends);
                response
                    .send((result, ends.as_ref().unwrap().pushed))
                    .unwrap();
            });
            // Always release the gated open before reporting failure, so a regression cannot hang CI.
            close_attempt.recv_timeout(Duration::from_secs(2)).unwrap();
            let progress = received.recv_timeout(Duration::from_secs(2));
            let closed_early = closing.recv_timeout(Duration::from_millis(25)).is_ok();
            resume.send(()).unwrap();
            assert!(
                matches!(open.join().unwrap(), Err(OpenError::Failed(why)) if why == "gated open")
            );
            assert_eq!(
                progress.unwrap(),
                (None, 2),
                "press and release crossed the command ring"
            );
            assert!(!closed_early, "close must follow the pending open");
            closing.recv_timeout(Duration::from_secs(2)).unwrap();
        });
    }

    #[test]
    fn page_records_builtin_and_plays_the_committed_loop() {
        record_from_page(true);
    }

    #[test]
    fn page_records_dry_input_and_plays_the_committed_loop() {
        record_from_page(false);
    }
}
