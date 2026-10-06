//! The VST2 unit and owner against the in-process fixture (`vst2_fixture.rs`: a real `AEffect`
//! behind a real entry, no DLL), loaded with `engine_slot` into a test device's engine, where the
//! production `run_with` owns it. The editor and the unit are also driven directly, with the test
//! thread standing in for the owner. No audio device and no visible window: the device renders to
//! memory and an editor's host window is never shown.
use super::super::engine_slot::{self, EngineSlotEvent, EngineSlotHandle, PluginFormat};
use super::super::super::editor_window::client_size;
use super::super::super::tone::{self, TempDir, ToneBinding, ToneIdentity};
use super::super::super::vst2::fixture::{self, Probe, Shape, TracedEvent};
use super::super::super::vst2::host_callback;
use super::*;
use crate::engine_io::test_rig::TestDevice;
use lf_engine::{Command, NoteTarget, TimedCommand};
use rtrb::{Producer, RingBuffer};
use std::sync::atomic::{AtomicBool, Ordering::Release};
use std::sync::mpsc::{channel, Sender, SyncSender};
use std::sync::Mutex;
use std::time::Duration;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, IsWindowVisible, PostMessageW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, WM_CLOSE,
};

/// The fixture's unique id as a descriptor carries it.
const ID: &str = "4c664678";
const SYNTH: i32 = EFF_FLAGS_CAN_REPLACING | EFF_FLAGS_IS_SYNTH;
const NULL: *mut c_void = std::ptr::null_mut();

type Seen = Arc<Mutex<Vec<EngineSlotEvent>>>;

/// A probed instrument: no input, two outputs, three parameters.
fn instrument(probe: &'static Probe) -> Shape {
    Shape { inputs: 0, flags: SYNTH, probe: Some(probe), ..Shape::default() }
}

/// A device rendering 256-frame blocks at `rate`, with a constant 0.1 as its input, paced near
/// real time.
fn device(rate: u32) -> TestDevice {
    TestDevice::start(rate, 256, Duration::from_millis(5), |_| 0.1)
}

/// Whether `pred` holds for every frame of the device's last block. Never before it has rendered a
/// whole one: an empty output is not a silent one.
fn last_block(device: &TestDevice, pred: impl Fn(f32) -> bool) -> bool {
    let output = device.output.lock().unwrap();
    output.len() >= 256 && output.iter().rev().take(256).all(|&x| pred(x))
}

fn rendered_near(device: &TestDevice, level: f32) -> bool {
    last_block(device, |x| (x - level).abs() < 1e-3)
}

/// Whether an instrument at level 0.25 or more is what the device renders. Not an exact level: an
/// instrument reaches the output through the master limiter, an effect's wet signal does not.
fn sounding(device: &TestDevice) -> bool {
    last_block(device, |x| x > 0.2)
}

/// The level the device has settled at: its last block, once that is one value (the limiter's
/// gain still creeps in the sixth digit, so two settled levels are compared with a tolerance).
fn settled_level(device: &TestDevice) -> Option<f32> {
    let output = device.output.lock().unwrap();
    let last = *output.last()?;
    (last > 0.0 && output.iter().rev().take(256).all(|&x| (x - last).abs() < 1e-5)).then_some(last)
}

/// Wait for the device to settle on one level above silence and return it.
fn level_once_settled(device: &TestDevice) -> f32 {
    assert!(wait_for(3000, || settled_level(device).is_some()), "the output settles");
    settled_level(device).unwrap()
}

fn installed(device: &TestDevice, slot: usize) -> Option<(SlotKind, Frame)> {
    device.host().core.rt.lock().unwrap().engine.as_ref().unwrap().slot(slot)
}

fn send(device: &TestDevice, command: Command) {
    device.host().send(TimedCommand { frame: None, command }).unwrap();
}

/// Spin until `pred` holds or `ms` elapse; returns whether it held.
fn wait_for(ms: u64, pred: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    pred()
}

/// Load the fixture in `shape` as plugin `id` through the engine-mode owner: the entry runs on the
/// owner thread. The sink keeps every event.
fn try_load(device: &TestDevice, slot: usize, shape: Shape, id: &'static str, tone: Option<ToneBinding>) -> (Result<EngineSlotHandle, String>, Seen) {
    let seen = Seen::default();
    let sink_seen = seen.clone();
    let handle = engine_slot::spawn(
        PluginFormat::Vst2,
        device.host().slot(slot),
        0,
        Arc::new(move |e| sink_seen.lock().unwrap().push(e)),
        tone,
        move |ctx| {
            run_with(ctx, id, move || {
                fixture::arm(shape);
                Ok(Opened { module: None, entry: fixture::entry, file_stem: "fixture-file".to_string() })
            })
        },
    );
    (handle, seen)
}

fn load(device: &TestDevice, slot: usize, shape: Shape) -> (EngineSlotHandle, Seen) {
    load_with_tone(device, slot, shape, None)
}

fn load_with_tone(device: &TestDevice, slot: usize, shape: Shape, tone: Option<ToneBinding>) -> (EngineSlotHandle, Seen) {
    let (handle, seen) = try_load(device, slot, shape, ID, tone);
    (handle.expect("the fixture loads into the engine"), seen)
}

/// Call the host as the plugin behind `probe` would, from this thread, with no pointer.
fn plugin_calls_host(probe: &Probe, opcode: i32, index: i32, value: isize, opt: f32) -> isize {
    // SAFETY: the instance is alive and the opcodes used this way read no pointer.
    unsafe { host_callback(probe.raw(), opcode, index, value, NULL, opt) }
}

/// The same call from a thread of the plugin's own.
fn plugin_thread_calls_host(probe: &'static Probe, opcode: i32, index: i32, value: isize, opt: f32) -> isize {
    std::thread::spawn(move || plugin_calls_host(probe, opcode, index, value, opt)).join().unwrap()
}

/// What the load sends a plugin with three parameters and no stored tone, in order.
fn load_calls(block: isize) -> Vec<(i32, isize)> {
    vec![
        (EFF_OPEN, 0),
        (EFF_GET_PLUG_CATEGORY, 0),
        (EFF_GET_EFFECT_NAME, 0),
        (EFF_SET_SAMPLE_RATE, 0),
        (EFF_SET_BLOCK_SIZE, block),
        (EFF_MAINS_CHANGED, 1),
        (EFF_START_PROCESS, 0),
        (EFF_GET_PARAM_NAME, 0),
        (EFF_GET_PARAM_NAME, 0),
        (EFF_GET_PARAM_NAME, 0),
    ]
}

/// What one restart or re-activation sends, in order.
fn cycle_calls(block: isize) -> Vec<(i32, isize)> {
    vec![
        (EFF_STOP_PROCESS, 0),
        (EFF_MAINS_CHANGED, 0),
        (EFF_SET_SAMPLE_RATE, 0),
        (EFF_SET_BLOCK_SIZE, block),
        (EFF_MAINS_CHANGED, 1),
        (EFF_START_PROCESS, 0),
    ]
}

/// Every dispatcher call the fixture recorded came on one thread, which is neither this test's nor
/// the device's: the owner.
fn assert_lifecycle_on_the_owner(probe: &Probe) {
    let calls = probe.calls.lock().unwrap();
    let owner = calls[0].thread;
    assert_ne!(owner, std::thread::current().id());
    for call in calls.iter() {
        assert!(call.thread == owner && !call.on_device, "opcode {} came off the owner thread", call.opcode);
    }
}

/// The plugin was rendered only between `effStartProcess` and `effStopProcess`, and no lifecycle
/// call reached it while a process call was in flight.
fn assert_rendered_only_while_started(probe: &Probe) {
    assert_eq!(probe.renders_while_stopped.load(Relaxed), 0, "a process call on a plugin that was not started");
    assert_eq!(probe.lifecycle_in_process.load(Relaxed), 0, "a lifecycle call while a process call was in flight");
}

#[test]
fn a_loaded_slot_renders_in_the_engine_and_unloads_in_order() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    probe.set_level(0.25);
    let (handle, _) = load(&device, 1, Shape { delay: 32, ..instrument(probe) });
    assert_eq!(handle.kind(), SlotKind::Instrument, "a synth: an instrument");
    assert_eq!(handle.name(), "Fixture");
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 8), "the engine processes the unit");
    assert!(wait_for(2000, || sounding(&device)), "its output reaches the device");
    assert_eq!(installed(&device, 1), Some((SlotKind::Instrument, 32)), "installed with the latency it reported");
    assert_eq!(f32::from_bits(probe.rate.load(Relaxed)), 48_000.0, "told the engine's rate");
    assert_eq!(probe.block.load(Relaxed), 4096, "and its largest block");
    assert_eq!(probe.last_frames.load(Relaxed), 256);
    assert_eq!((probe.distinct_outputs.load(Relaxed), probe.rows_missing.load(Relaxed)), (2, false));

    // Inside its process call the plugin reads the unit's own time and is told it runs in real time.
    assert_eq!(f64::from_bits(probe.time_rate.load(Relaxed)), 48_000.0);
    assert_eq!((f64::from_bits(probe.time_tempo.load(Relaxed)), probe.time_flags.load(Relaxed)), (120.0, 0));
    assert_eq!(probe.process_level.load(Relaxed), PROCESS_LEVEL_REALTIME);
    let position = f64::from_bits(probe.time_pos.load(Relaxed));
    assert!(wait_for(2000, || f64::from_bits(probe.time_pos.load(Relaxed)) > position), "the position runs");
    assert_eq!(f64::from_bits(probe.time_pos.load(Relaxed)) % 256.0, 0.0, "in frames processed");

    // The load, in order. Every one of these calls answered 0 (the fixture's dispatcher), which is
    // no failure for effOpen, effMainsChanged or effStartProcess.
    assert_eq!(probe.opcodes(), load_calls(4096));

    handle.unload().expect("the unit comes back");
    assert_eq!(installed(&device, 1), None, "the slot is empty");
    assert_eq!(
        probe.opcodes()[load_calls(4096).len()..],
        [(EFF_STOP_PROCESS, 0), (EFF_MAINS_CHANGED, 0), (EFF_CLOSE, 0)],
        "stopped, suspended and closed, in that order, once the unit was out of the engine"
    );
    assert_eq!((probe.instances.load(Relaxed), probe.closes.load(Relaxed)), (1, 1));
    assert_lifecycle_on_the_owner(probe);
    assert_eq!(probe.rt_calls_off_device.load(Relaxed), 0, "every process call ran on the device thread");
    assert_eq!(probe.calls_while_halted.load(Relaxed), 0);
    assert_rendered_only_while_started(probe);
}

#[test]
fn an_effect_slot_takes_the_live_input_and_its_output_reaches_the_device() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    let (handle, _) = load(&device, 1, Shape { probe: Some(probe), ..Shape::default() });
    assert_eq!(handle.kind(), SlotKind::Effect, "inputs and no synth flag: an effect");
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 8 && rendered_near(&device, 0.0)), "not live: silence");
    assert_eq!(installed(&device, 1), Some((SlotKind::Effect, 0)));
    assert_eq!(probe.distinct_inputs.load(Relaxed), 2, "each declared input has a row of its own");
    send(&device, Command::SetSlotLive(1, true));
    assert!(
        wait_for(2000, || rendered_near(&device, 0.1)),
        "live: the device input, copied to both inputs and echoed, the pair's mean back to mono"
    );
    handle.unload().unwrap();
    assert_eq!(probe.closes.load(Relaxed), 1);
}

#[test]
fn a_note_and_a_host_parameter_edit_reach_the_plugin() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    let (handle, _) = load(&device, 0, instrument(probe));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 4));
    send(&device, Command::SelectInstrument(NoteTarget::Slot(0)));
    send(&device, Command::NoteOn(62, 0.5));
    assert!(wait_for(2000, || probe.note_ons.load(Relaxed) == 1), "the note-on reaches the plugin");
    assert_eq!((probe.last_key.load(Relaxed), probe.last_velocity.load(Relaxed)), (62, 64), "velocity 0.5 of 127");
    assert!((0..256).contains(&probe.last_delta.load(Relaxed)), "at an offset inside its block");
    send(&device, Command::NoteOff(62));
    assert!(wait_for(2000, || probe.note_offs.load(Relaxed) == 1), "and its note-off");
    assert_eq!(probe.bad_events.load(Relaxed), 0, "both as MIDI events on channel 0");

    // The plugin's parameters are its indices, listed with their names and live values.
    probe.values[2].store(0.75f32.to_bits(), Relaxed);
    let listed = handle.list_params().unwrap();
    assert_eq!(listed.iter().map(|p| (p.id, p.name.as_str(), p.value)).collect::<Vec<_>>(), [
        (0, "Fixture parameter 0", 0.0),
        (1, "Fixture parameter 1", 0.0),
        (2, "Fixture parameter 2", 0.75),
    ]);
    assert!(listed.iter().all(|p| (p.min_value, p.max_value) == (0.0, 1.0)));
    assert!(handle.set_param(3, 0.1).is_err(), "an index the plugin never declared");
    let before = probe.sets.load(Relaxed);
    handle.set_param(1, 0.3).unwrap();
    assert!(wait_for(2000, || probe.sets.load(Relaxed) == before + 1), "the edit reaches setParameter");
    assert_eq!((probe.last_set_index.load(Relaxed), f32::from_bits(probe.last_set_value.load(Relaxed))), (1, 0.3));
    assert_eq!(probe.sets_on_device.load(Relaxed), 1, "on the thread that processes the plugin");
    handle.unload().unwrap();
    assert_eq!(probe.rt_calls_off_device.load(Relaxed), 0, "effProcessEvents ran on the device thread too");
    assert_lifecycle_on_the_owner(probe);
}

/// `audioMasterAutomate` from the three places a plugin calls it: a thread of its own, the owner
/// thread (inside `effIdle`, which the owner sends after `audioMasterNeedIdle`), and the audio
/// thread from inside `setParameter`. Each reaches the caller with the value the plugin reported,
/// and the host never sends it back or asks for it.
#[test]
fn plugin_automation_reaches_the_caller_with_its_value_and_is_never_echoed() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    let (handle, seen) = load(&device, 0, instrument(probe));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 4));
    let (sets, gets) = (probe.sets.load(Relaxed), probe.gets.load(Relaxed));
    let told = |id: u32, value: f32| {
        let event = EngineSlotEvent::ParamChanged { id, value: f64::from(value) };
        wait_for(2000, || seen.lock().unwrap().contains(&event))
    };

    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_AUTOMATE, 2, 0, 0.7), 0);
    assert!(told(2, 0.7), "from a thread of the plugin's own");

    *probe.on_idle.lock().unwrap() = Some(Box::new(|effect| {
        // SAFETY: the live instance, inside its own `effIdle`; automate reads no pointer.
        unsafe { host_callback(effect, AUDIO_MASTER_AUTOMATE, 0, 0, NULL, 0.4) };
    }));
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_NEED_IDLE, 0, 0, 0.0), 1);
    assert!(told(0, 0.4), "from the owner thread, inside the effIdle the plugin asked for");
    assert_eq!(probe.idles.load(Relaxed), 1);
    assert_eq!((probe.sets.load(Relaxed), probe.gets.load(Relaxed)), (sets, gets), "neither was sent back or read back");

    probe.automate_in_set.store(true, Relaxed);
    handle.set_param(1, 0.8).unwrap();
    assert!(told(1, 0.4), "from the audio thread, inside setParameter: the plugin's value, not the host's");
    std::thread::sleep(Duration::from_millis(100)); // a few more owner turns
    assert_eq!(probe.sets.load(Relaxed), sets + 1, "the host's own edit, once: the report is never echoed");
    assert_eq!(probe.value(1), 0.8, "and the plugin keeps what the host set");
    assert_eq!(probe.gets.load(Relaxed), gets, "the reported value is never fetched with getParameter");
    assert_eq!(seen.lock().unwrap().len(), 3, "one event per report");
    handle.unload().unwrap();
}

#[test]
fn an_io_change_inside_process_silences_the_unit_until_the_owner_has_cycled_it() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    probe.set_level(0.25);
    let (handle, _) = load(&device, 0, instrument(probe));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 8 && sounding(&device)), "it plays");
    let blocks = device.blocks.load(Relaxed);

    // The plugin grows to four outputs and says so from inside its process call.
    probe.grow_in_process.store(4, Relaxed);
    assert!(
        wait_for(2000, || probe.count(EFF_START_PROCESS, 0) == 2 && probe.distinct_outputs.load(Relaxed) == 4),
        "stopped → suspended → resumed → reinstalled, with a row for each of the four outputs"
    );
    assert_eq!(probe.calls_while_halted.load(Relaxed), 0, "no plugin call between the report and the cycle");
    assert_eq!(probe.opcodes()[load_calls(4096).len()..], cycle_calls(4096));
    assert!(wait_for(2000, || sounding(&device)), "and it plays again");
    assert_eq!(installed(&device, 0), Some((SlotKind::Instrument, 0)));
    assert!(device.blocks.load(Relaxed) > blocks, "the device kept rendering through the restart");
    assert_eq!(device.host().core.counters.lock_misses.load(Relaxed), 0, "the owner never held the engine");
    assert!(!probe.rows_missing.load(Relaxed));
    handle.unload().unwrap();
    assert_lifecycle_on_the_owner(probe);
    assert_eq!(probe.rt_calls_off_device.load(Relaxed), 0);
    assert_rendered_only_while_started(probe);
}

/// A plugin that settles its pins inside `effMainsChanged(1)` and reports it there
/// (`audioMasterIOChanged`): the report describes the state the owner reads right after, so it is
/// consumed and the plugin is not cycled for it again.
#[test]
fn an_io_change_reported_while_resuming_describes_the_state_just_read() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    probe.set_level(0.25);
    probe.io_changed_in_resume.store(true, Relaxed);
    let (handle, _) = load(&device, 0, instrument(probe));
    assert!(wait_for(2000, || sounding(&device)), "it plays: the load's report halted nothing");
    let processed = probe.processes.load(Relaxed);
    std::thread::sleep(Duration::from_millis(200)); // several owner turns
    assert!(probe.processes.load(Relaxed) > processed, "and it is still processed");
    assert_eq!(probe.opcodes(), load_calls(4096), "and no cycle followed");
    handle.unload().unwrap();
    assert_rendered_only_while_started(probe);
}

/// A restart into a layout the host refuses (no output) leaves the slot bypassed with the plugin
/// suspended. A device change then finds no unit in the engine and touches nothing; the plugin's
/// next report, on a layout the host takes, brings it back at the NEW rate and block.
#[test]
fn a_refused_restart_leaves_the_slot_bypassed_through_a_device_change_until_a_good_restart() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    probe.set_level(0.25);
    let (handle, _) = load(&device, 0, instrument(probe));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 8 && sounding(&device)), "it plays");

    probe.outputs_on_suspend.store(0, Relaxed);
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0), 1);
    assert!(
        wait_for(2000, || probe.count(EFF_MAINS_CHANGED, 0) == 1 && rendered_near(&device, 0.0)),
        "the cycle suspended it and the slot went silent"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(installed(&device, 0), None, "the refused unit stays out of the engine");
    assert_eq!(probe.count(EFF_MAINS_CHANGED, 1), 1, "never resumed on the refused layout");
    assert_eq!(probe.count(EFF_START_PROCESS, 0), 1);

    let calls = probe.opcodes().len();
    device.rebuild_at(44_100, 1024);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(probe.opcodes().len(), calls, "a device change does not retry a parked unit");
    assert_eq!(installed(&device, 0), None);

    // SAFETY: the unit is parked and the owner reads the effect only once the report below asks it to.
    unsafe { probe.set_outputs(2) };
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0), 1);
    assert!(wait_for(2000, || probe.count(EFF_START_PROCESS, 0) == 2 && sounding(&device)), "it plays again");
    assert_eq!(f32::from_bits(probe.rate.load(Relaxed)), 44_100.0, "resumed at the rate the device has now");
    assert_eq!((probe.block.load(Relaxed), probe.host_rate_at_resume.load(Relaxed)), (1024, 44_100));
    assert_eq!(probe.count(EFF_STOP_PROCESS, 0), 1, "a plugin that was not running is not stopped again");
    assert!(installed(&device, 0).is_some());
    assert_eq!(probe.calls_while_halted.load(Relaxed), 0);
    handle.unload().unwrap();
    assert_eq!(probe.closes.load(Relaxed), 1);
    assert_lifecycle_on_the_owner(probe);
    assert_rendered_only_while_started(probe);
}

#[test]
fn a_slot_left_bypassed_by_a_refused_restart_still_unloads_in_order() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    let (handle, _) = load(&device, 0, instrument(probe));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 8));
    probe.outputs_on_suspend.store(0, Relaxed);
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0), 1);
    assert!(wait_for(2000, || probe.count(EFF_SET_BLOCK_SIZE, 4096) == 2), "the cycle ran up to the refusal");
    handle.unload().expect("a bypassed slot still unloads");
    assert_eq!(
        probe.opcodes()[load_calls(4096).len()..],
        [(EFF_STOP_PROCESS, 0), (EFF_MAINS_CHANGED, 0), (EFF_SET_SAMPLE_RATE, 0), (EFF_SET_BLOCK_SIZE, 4096), (EFF_CLOSE, 0)],
        "the unload neither stops nor suspends a plugin the failed cycle left suspended"
    );
    assert_rendered_only_while_started(probe);
}

#[test]
fn a_device_change_reactivates_the_plugin_at_the_new_rate() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    probe.set_level(0.25);
    let (handle, _) = load(&device, 1, instrument(probe));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 4));

    device.rebuild_at(44_100, 1024);
    assert!(wait_for(2000, || probe.count(EFF_START_PROCESS, 0) == 2), "the owner took the unit back and reinstalled it");
    assert_eq!(probe.opcodes()[load_calls(4096).len()..], cycle_calls(1024), "stopped and suspended, then the new block");
    assert_eq!(f32::from_bits(probe.rate.load(Relaxed)), 44_100.0, "told the new rate");
    assert_eq!(probe.host_rate_at_resume.load(Relaxed), 44_100, "which the host answers from inside the resume");
    let processed = probe.processes.load(Relaxed);
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > processed + 4), "the new engine processes it");
    assert_eq!(f64::from_bits(probe.time_rate.load(Relaxed)), 44_100.0, "its time info runs at the new rate");
    assert!(wait_for(2000, || sounding(&device)));
    assert!(installed(&device, 1).is_some());
    handle.unload().unwrap();
    assert_lifecycle_on_the_owner(probe);
    assert_rendered_only_while_started(probe);
}

/// Sixteen outputs with signal on the first pair only, as a multi-output instrument's main pair,
/// its two sides at different levels: the slot plays that pair's mean, as loud as a plain stereo
/// instrument at the level between them, not one side of it, not the sum, and not the mean of all
/// sixteen (an eighth of it).
#[test]
fn a_multi_output_instrument_is_not_attenuated() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let stereo = Probe::new();
    stereo.set_level(0.5);
    let (handle, _) = load(&device, 0, instrument(stereo));
    let reference = level_once_settled(&device);
    handle.unload().unwrap();
    assert!(wait_for(2000, || rendered_near(&device, 0.0)));

    let probe = Probe::new();
    probe.set_level(0.5);
    probe.spread.store(0.25f32.to_bits(), Relaxed); // 0.75 left, 0.25 right
    probe.signal_outputs.store(2, Relaxed);
    let (handle, _) = load(&device, 0, Shape { outputs: 16, ..instrument(probe) });
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 8));
    let level = level_once_settled(&device);
    assert!((level - reference).abs() < 1e-3, "the pair's mean at full level: {level} beside {reference}");
    assert_eq!(probe.distinct_outputs.load(Relaxed), 16, "every declared output has a row of its own");
    handle.unload().unwrap();
}

/// A VST 1.0-era plugin: no `processReplacing`, only the call that ADDS into its outputs. Its rows
/// are cleared before every call, so it plays at the level a replacing plugin does and stays there.
#[test]
fn a_plugin_without_can_replacing_plays_through_process() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let replacing = Probe::new();
    replacing.set_level(0.25);
    let (handle, _) = load(&device, 0, instrument(replacing));
    let reference = level_once_settled(&device);
    handle.unload().unwrap();
    assert!(wait_for(2000, || rendered_near(&device, 0.0)));

    let probe = Probe::new();
    probe.set_level(0.25);
    let old = Shape { flags: EFF_FLAGS_IS_SYNTH, process_replacing: false, ..instrument(probe) };
    let (handle, _) = load(&device, 0, old);
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 8));
    let level = level_once_settled(&device);
    assert!((level - reference).abs() < 1e-3, "{level} beside {reference}");
    let processed = probe.processes.load(Relaxed);
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > processed + 32));
    let later = level_once_settled(&device);
    assert!((later - reference).abs() < 1e-3, "nothing piles up across calls: {later} beside {reference}");
    handle.unload().unwrap();
}

/// A load that fails closes the instance it opened, and leaves alone one it never called.
#[test]
fn a_failed_load_closes_what_it_opened_and_frees_the_slot() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let other = Probe::new();
    let (loaded, _) = try_load(&device, 0, instrument(other), "0000beef", None);
    let err = loaded.err().expect("another plugin's id");
    assert!(err.contains("4c664678, not 0000beef"), "{err}");
    assert_eq!(other.opcodes(), [(EFF_OPEN, 0), (EFF_CLOSE, 0)]);

    let refused = Probe::new();
    let (loaded, _) = try_load(&device, 0, Shape { outputs: 0, ..instrument(refused) }, ID, None);
    let err = loaded.err().expect("an effect this host refuses");
    assert!(err.contains("channel count 0"), "{err}");
    assert_eq!(refused.opcodes(), [], "a refused effect is never called, so it is never closed either");
    assert_eq!(installed(&device, 0), None);

    // A shell (several plugins in one file): the scan lists none, and a load refuses one too.
    let shell = Probe::new();
    let (loaded, _) = try_load(&device, 0, Shape { category: PLUG_CATEGORY_SHELL, ..instrument(shell) }, ID, None);
    assert_eq!(loaded.err().as_deref(), Some("a VST2 shell plugin is not supported"));
    assert_eq!(shell.opcodes(), [(EFF_OPEN, 0), (EFF_GET_PLUG_CATEGORY, 0), (EFF_CLOSE, 0)], "opened, asked, closed");
    assert_eq!(shell.closes.load(Relaxed), 1);
    assert_eq!(installed(&device, 0), None);

    let probe = Probe::new();
    let (handle, _) = load(&device, 0, instrument(probe));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 4), "the slot takes the next load");
    handle.unload().unwrap();
}

fn identity() -> ToneIdentity {
    ToneIdentity { format: "vst2".into(), path: r"C:\fixture.dll".into(), id: ID.into() }
}

/// The fixture's tone as the store holds it.
fn stored(dir: &TempDir) -> Option<Vst2State> {
    let tone = dir.store().load(0, &identity()).unwrap()?;
    Some(tone::decode_vst2(&tone.state).unwrap())
}

fn store_state(dir: &TempDir, state: &[u8]) {
    let t = tone::Tone { identity: identity(), name: "Fixture".into(), state: state.to_vec() };
    dir.store().save_encoded(0, &tone::encode(&t)).unwrap();
}

/// Where `opcode` first comes in the calls the fixture recorded.
fn first(probe: &Probe, opcode: i32, value: isize) -> usize {
    probe.opcodes().iter().position(|&call| call == (opcode, value)).expect("the call was made")
}

/// A plugin without chunks keeps its parameters. Its `effSetProgram` resets every parameter, so
/// the restore only holds when the program goes first.
#[test]
fn a_parameter_tone_round_trips_with_its_program_selected_first() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let dir = TempDir::new("vst2-params");
    let shape = |probe: &'static Probe| {
        probe.program_resets.store(true, Relaxed);
        Shape { programs: 4, ..instrument(probe) }
    };
    let first_run = Probe::new();
    let (handle, _) = load_with_tone(&device, 0, shape(first_run), Some(dir.binding(0, identity())));
    assert_eq!(handle.tone(), None, "nothing stored yet");
    for (index, value) in [(0, 0.2), (1, 0.4), (2, 0.6)] {
        handle.set_param(index, value).unwrap();
    }
    assert!(wait_for(2000, || first_run.sets.load(Relaxed) == 3));
    first_run.program.store(2, Relaxed);
    let file = tone::decode(&handle.take_tone().unwrap()).unwrap();
    assert_eq!(file.identity, identity());
    let saved = Vst2State::Params { program: 2, values: vec![0.2, 0.4, 0.6] };
    assert_eq!(tone::decode_vst2(&file.state).unwrap(), saved);
    assert_eq!(stored(&dir), Some(saved), "and it lands in the store");
    assert!(first_run.processes.load(Relaxed) > 0 && installed(&device, 0).is_some(), "saved while the unit plays");
    handle.unload().unwrap();

    let second_run = Probe::new();
    let (handle, _) = load_with_tone(&device, 0, shape(second_run), Some(dir.binding(0, identity())));
    assert_eq!(handle.tone(), Some(ToneRestore::Restored));
    assert_eq!([second_run.value(0), second_run.value(1), second_run.value(2)], [0.2, 0.4, 0.6]);
    assert_eq!((second_run.program.load(Relaxed), second_run.set_programs.load(Relaxed)), (2, 1));
    assert!(first(second_run, EFF_SET_PROGRAM, 2) < first(second_run, EFF_MAINS_CHANGED, 1), "restored before it resumes");
    assert!(first(second_run, EFF_SET_BLOCK_SIZE, 4096) < first(second_run, EFF_SET_PROGRAM, 2), "and after it knows its rate");
    assert_eq!((second_run.sets.load(Relaxed), second_run.sets_on_device.load(Relaxed)), (3, 0), "on the owner thread");
    handle.unload().unwrap();
}

/// A plugin with `effFlagsProgramChunks` keeps its bank chunk (index 0, both ways), which restores
/// its own program: this one's bank ends in its program's number. Its `effSetProgram` clears the
/// chunk: selecting a program after `effSetChunk` would lose it. Its `effSetChunk` answers 0, which
/// is no refusal.
#[test]
fn a_chunk_tone_round_trips_and_no_program_is_selected_after_it() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let dir = TempDir::new("vst2-chunk");
    let shape = |probe: &'static Probe| {
        probe.program_resets.store(true, Relaxed);
        Shape { programs: 4, flags: SYNTH | EFF_FLAGS_PROGRAM_CHUNKS, ..instrument(probe) }
    };
    let first_run = Probe::new();
    let (handle, _) = load_with_tone(&device, 0, shape(first_run), Some(dir.binding(0, identity())));
    assert_eq!(handle.tone(), None);
    *first_run.chunk.lock().unwrap() = b"bank v".to_vec();
    first_run.program.store(1, Relaxed);
    let file = tone::decode(&handle.take_tone().unwrap()).unwrap();
    let saved = Vst2State::Chunk { program: 1, chunk: b"bank v\x01".to_vec() };
    assert_eq!(tone::decode_vst2(&file.state).unwrap(), saved);
    assert_eq!(stored(&dir), Some(saved));
    assert_eq!(first_run.get_chunk_index.load(Relaxed), 0, "the bank, not the current program");
    assert_eq!(first_run.gets.load(Relaxed), 3, "the listing's reads only: a chunk plugin's parameters are not saved");
    handle.unload().unwrap();

    let second_run = Probe::new();
    let (handle, _) = load_with_tone(&device, 0, shape(second_run), Some(dir.binding(0, identity())));
    assert_eq!(handle.tone(), Some(ToneRestore::Restored));
    assert_eq!(*second_run.chunk.lock().unwrap(), b"bank v");
    assert_eq!((second_run.set_chunks.load(Relaxed), second_run.set_programs.load(Relaxed)), (1, 0));
    assert_eq!(second_run.set_chunk_index.load(Relaxed), 0, "as a bank");
    assert_eq!(second_run.program.load(Relaxed), 1, "the bank restored its program: nothing selected one");
    assert!(first(second_run, EFF_SET_CHUNK, 7) < first(second_run, EFF_MAINS_CHANGED, 1), "restored before it resumes");
    assert!(wait_for(2000, || second_run.processes.load(Relaxed) > 4), "and it plays");

    // A change is saved by the unload, after the plugin stopped and before it closes.
    *second_run.chunk.lock().unwrap() = b"bank w".to_vec();
    handle.set_param(0, 0.1).unwrap();
    handle.unload().unwrap();
    assert_eq!(stored(&dir), Some(Vst2State::Chunk { program: 1, chunk: b"bank w\x01".to_vec() }));
    assert_eq!(second_run.set_programs.load(Relaxed), 0);
    let calls = second_run.opcodes();
    let chunk_asked = calls.iter().position(|&(opcode, _)| opcode == EFF_GET_CHUNK).unwrap();
    assert!(first(second_run, EFF_MAINS_CHANGED, 0) < chunk_asked && chunk_asked < first(second_run, EFF_CLOSE, 0));
}

/// The chunk is the plugin's memory and its length the plugin's word. A length of 0 or less is
/// nothing to save; one past what a tone holds is refused, and so is a length with no pointer.
/// Either way nothing is read at the pointer (this fixture's bank is five bytes long whatever it
/// claims) and nothing is stored.
#[test]
fn a_chunk_with_a_negative_or_oversized_length_saves_nothing() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let dir = TempDir::new("vst2-chunk-length");
    let probe = Probe::new();
    *probe.chunk.lock().unwrap() = b"tiny".to_vec();
    let chunky = Shape { flags: SYNTH | EFF_FLAGS_PROGRAM_CHUNKS, ..instrument(probe) };
    let (handle, _) = load_with_tone(&device, 0, chunky, Some(dir.binding(0, identity())));
    for lie in [-1, isize::MIN] {
        probe.chunk_len.store(lie, Relaxed);
        assert_eq!(handle.take_tone(), Ok(Vec::new()), "a length of {lie}: nothing to save");
        assert_eq!(stored(&dir), None);
    }
    for lie in [VST2_MAX_CHUNK as isize + 1, isize::MAX] {
        probe.chunk_len.store(lie, Relaxed);
        let err = handle.take_tone().unwrap_err();
        assert!(err.contains("more than"), "{err}");
        assert_eq!(stored(&dir), None);
    }
    probe.chunk_len.store(0, Relaxed);
    probe.chunk_null.store(true, Relaxed);
    let err = handle.take_tone().unwrap_err();
    assert!(err.contains("5-byte chunk with no pointer"), "{err}");
    assert_eq!(stored(&dir), None);
    probe.chunk_null.store(false, Relaxed);
    probe.chunk.lock().unwrap().clear();
    assert_eq!(handle.take_tone(), Ok(Vec::new()), "an empty chunk");
    *probe.chunk.lock().unwrap() = b"real".to_vec();
    assert!(!handle.take_tone().unwrap().is_empty());
    assert_eq!(stored(&dir), Some(Vst2State::Chunk { program: 0, chunk: b"real\0".to_vec() }));
    handle.unload().unwrap();
}

/// A tone that does not match the plugin, or does not parse, is refused before the plugin is
/// called for it: the load goes on at the plugin's defaults, says the tone failed, and keeps the
/// stored tone until the player changes something.
#[test]
fn a_tone_that_does_not_match_or_does_not_parse_loads_at_the_defaults() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let dir = TempDir::new("vst2-refused");
    let mismatches = [
        ("five parameters for three", SYNTH, tone::encode_vst2(&Vst2State::Params { program: 0, values: vec![0.5; 5] })),
        ("a program the plugin lacks", SYNTH, tone::encode_vst2(&Vst2State::Params { program: 7, values: vec![0.5; 3] })),
        ("a negative program", SYNTH, tone::encode_vst2(&Vst2State::Params { program: -1, values: vec![0.5; 3] })),
        ("a chunk for a plugin without chunks", SYNTH, tone::encode_vst2(&Vst2State::Chunk { program: 0, chunk: b"bank".to_vec() })),
        (
            "parameters for a plugin that keeps a chunk",
            SYNTH | EFF_FLAGS_PROGRAM_CHUNKS,
            tone::encode_vst2(&Vst2State::Params { program: 0, values: vec![0.5; 3] }),
        ),
        ("no container at all", SYNTH, b"garbage".to_vec()),
        ("another format's container", SYNTH, tone::encode_vst3(b"component", b"")),
    ];
    for (what, flags, state) in mismatches {
        store_state(&dir, &state);
        let probe = Probe::new();
        let (handle, _) = load_with_tone(&device, 0, Shape { flags, ..instrument(probe) }, Some(dir.binding(0, identity())));
        assert_eq!(handle.tone(), Some(ToneRestore::Failed), "{what}");
        assert_eq!(
            (probe.sets.load(Relaxed), probe.set_programs.load(Relaxed), probe.set_chunks.load(Relaxed)),
            (0, 0, 0),
            "{what}: the plugin was never called for it"
        );
        assert_eq!(probe.instances.load(Relaxed), 1, "{what}: the instance is untouched, so it is the one that runs");
        assert!(wait_for(2000, || probe.processes.load(Relaxed) > 4), "{what}: the load went on");
        handle.unload().unwrap();
        let kept = dir.store().load(0, &identity()).unwrap().unwrap();
        assert_eq!(kept.state, state, "{what}: the stored tone stays");
    }

    // A file that is no tone never reaches the plugin either.
    let path = dir.0.join(identity().file_name(0));
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() - 3]).unwrap();
    let probe = Probe::new();
    let (handle, _) = load_with_tone(&device, 0, instrument(probe), Some(dir.binding(0, identity())));
    assert_eq!(handle.tone(), Some(ToneRestore::Failed), "a truncated file");
    // A change the player makes is what the defaults replace it for.
    handle.set_param(0, 0.3).unwrap();
    assert!(wait_for(2000, || probe.sets.load(Relaxed) == 1));
    handle.unload().unwrap();
    assert_eq!(stored(&dir), Some(Vst2State::Params { program: 0, values: vec![0.3, 0.0, 0.0] }));
}

/// The engine does not hand the unit back (its lock is held, so nothing services the slot): the
/// unit may still be inside the plugin, and the plugin still calls the host. Nothing of it is
/// stopped, suspended or closed; the unit, the effect, its context and its module stay as they are.
#[test]
fn a_unit_the_engine_does_not_hand_back_leaves_the_plugin_loaded_and_uncalled() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    probe.set_level(0.25);
    let (handle, _) = load(&device, 0, instrument(probe));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 8));
    let calls = probe.opcodes();

    let engine = device.host().core.rt.lock().unwrap();
    let unloaded = handle.unload();
    drop(engine);
    let err = unloaded.expect_err("the unit did not come back");
    assert!(err.contains("did not come back"), "{err}");
    assert_eq!(probe.opcodes(), calls, "not stopped, not suspended, not closed");
    assert_eq!(probe.closes.load(Relaxed), 0);
    assert!(!probe.raw().is_null(), "the effect is still there");
    // The engine now finishes the removal it was asked for; the abandoned unit, when it comes back,
    // is leaked and makes no call. The context is still what the plugin's callbacks reach.
    assert!(device.wait_blocks(40, Duration::from_secs(5)));
    assert_eq!(plugin_calls_host(probe, AUDIO_MASTER_GET_SAMPLE_RATE, 0, 0, 0.0), 48_000);
    assert_eq!(probe.opcodes(), calls);
    assert_eq!(probe.rt_calls_off_device.load(Relaxed), 0);
}

/// The fixture as a plugin this thread owns, the way a load creates it.
fn plugin_here(slot: &SlotHost, shape: Shape) -> Vst2Plugin {
    fixture::with_shape(shape, || create(None, fixture::entry, ID, slot)).expect("the fixture opens")
}

/// The editor, with this thread as the plugin's owner and its host window never shown: opened at
/// the size the plugin reports once it is open, resized at once when the plugin asks from this
/// thread (granted, and refused when Windows clamps it), only latched from any other thread or
/// from inside a process call, closed once, and opened again.
#[test]
fn the_editor_opens_resizes_on_request_closes_and_reopens() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let slot = device.host().slot(0);

    let bare = Probe::new();
    let mut plugin = plugin_here(&slot, instrument(bare));
    assert_eq!(plugin.embed_editor(0).unwrap_err(), "plugin has no editor");
    assert_eq!(bare.opcodes(), [(EFF_OPEN, 0), (EFF_GET_PLUG_CATEGORY, 0)], "a plugin without the flag is not asked");
    drop(plugin);

    let probe = Probe::new();
    *probe.rect_open.lock().unwrap() = (640, 480);
    *probe.size_in_open.lock().unwrap() = Some((500, 350));
    let mut plugin = plugin_here(&slot, Shape { flags: SYNTH | EFF_FLAGS_HAS_EDITOR, ..instrument(probe) });
    let hwnd = plugin.embed_editor(0).expect("the editor opens");
    assert_eq!(probe.editor_parent.load(Relaxed), hwnd.0 as isize, "the plugin got the host window");
    assert_eq!(*probe.size_answers.lock().unwrap(), [1], "a size asked from inside effEditOpen is granted there");
    assert_eq!(client_size(hwnd), (640, 480), "then the size the plugin reports once it is open: not 400×300");
    assert_eq!(probe.opcodes()[2..], [(EFF_EDIT_GET_RECT, 0), (EFF_EDIT_OPEN, 0), (EFF_EDIT_GET_RECT, 0)]);
    assert_eq!(plugin.embed_editor(0).map(|again| again == hwnd), Ok(true), "an open editor is not opened twice");
    assert_eq!(probe.edit_opens.load(Relaxed), 1);

    let ask = |width: i32, height: isize| plugin_calls_host(probe, AUDIO_MASTER_SIZE_WINDOW, width, height, 0.0);
    assert_eq!(ask(800, 600), 1, "from the owner thread: resized here and now");
    assert_eq!(client_size(hwnd), (800, 600));
    // SAFETY: plain metric queries.
    let screen = unsafe { (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)) };
    assert_eq!(ask(screen.0 + 2000, screen.1 as isize + 2000), 0, "a size Windows clamps is not granted");
    assert_eq!(client_size(hwnd), (800, 600), "and the window keeps the size it had");
    // A side past any screen is refused before a window is touched or anything is latched, on the
    // owner thread and off it: a frame added to it would overflow.
    for (width, height) in [(i32::MAX, 600), (800, i32::MAX as isize), (16_385, 600), (800, 16_385), (800, isize::MAX)] {
        assert_eq!(ask(width, height), 0, "{width}×{height}");
        assert_eq!(client_size(hwnd), (800, 600), "{width}×{height}: the window is as it was");
        assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_SIZE_WINDOW, width, height, 0.0), 0);
        assert_eq!(plugin.ctx.take_resize(), None, "{width}×{height}: nothing is latched");
    }
    assert_eq!(ask(0, 600), 0);
    assert_eq!(plugin.ctx.take_resize(), None, "nothing is left queued by a request served or refused here");

    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_SIZE_WINDOW, 320, 200, 0.0), 0, "another thread: not yet");
    assert_eq!(client_size(hwnd), (800, 600), "no window is touched from there");
    assert_eq!(plugin.ctx.take_resize(), Some((320, 200)), "the owner applies it on its turn");
    let mut time = time_info(48_000.0, 0.0);
    {
        // SAFETY: `time` outlives the scope and only this thread touches it.
        let _processing = unsafe { ProcessingScope::enter(&plugin.ctx, &mut time) };
        assert_eq!(ask(300, 200), 0, "the owner thread inside a process call is the processing thread");
        assert_eq!(plugin_calls_host(probe, AUDIO_MASTER_IDLE, 0, 0, 0.0), 0);
    }
    assert_eq!(client_size(hwnd), (800, 600));
    assert_eq!(plugin.ctx.take_resize(), Some((300, 200)));

    assert_eq!(plugin_calls_host(probe, AUDIO_MASTER_IDLE, 0, 0, 0.0), 1, "the owner thread pumps its messages");
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_IDLE, 0, 0, 0.0), 0, "no other thread does");

    assert!(plugin.close_editor());
    assert!(!plugin.close_editor(), "closing a closed editor does nothing");
    assert_eq!(probe.edit_closes.load(Relaxed), 1);
    assert_eq!(ask(640, 480), 0, "with no editor open there is no window to size");
    assert_eq!(plugin.ctx.take_resize(), Some((640, 480)));

    let again = plugin.embed_editor(0).expect("it opens again");
    assert_eq!((probe.edit_opens.load(Relaxed), client_size(again)), (2, (640, 480)));
    assert!(plugin.close_editor());

    probe.refuse_open.store(true, Relaxed);
    let err = plugin.embed_editor(0).unwrap_err();
    assert!(err.contains("refused"), "{err}");
    assert!(plugin.editor.is_none());
    assert_eq!((probe.edit_opens.load(Relaxed), probe.edit_closes.load(Relaxed)), (2, 3), "told to close what it refused");

    probe.refuse_open.store(false, Relaxed);
    probe.no_rect.store(true, Relaxed);
    *probe.size_in_open.lock().unwrap() = None;
    let provisional = plugin.embed_editor(0).expect("a plugin that reports no size still opens");
    assert_eq!(client_size(provisional), (900, 600));

    drop(plugin);
    let calls = probe.opcodes();
    assert_eq!(calls[calls.len() - 2..], [(EFF_EDIT_CLOSE, 0), (EFF_CLOSE, 0)], "an open editor closes before the plugin");
    assert_eq!(probe.closes.load(Relaxed), 1);
}

/// A resumed plugin on this thread and its unit with a block of `max_frames`, the ring's producer
/// and the fault latch.
fn unit_here(plugin: &mut Vst2Plugin, slot: &SlotHost, max_frames: usize) -> (Box<Vst2Unit>, Producer<PluginEvent>, Arc<AtomicU32>) {
    plugin.configure(slot).unwrap();
    let info = plugin.resume().unwrap();
    let (params, ring) = RingBuffer::<PluginEvent>::new(8);
    let faults = Arc::new(AtomicU32::new(0));
    // SAFETY: the open effect `info` was validated from; the plugin outlives the unit in each test.
    let unit = unsafe { Vst2Unit::new(plugin.effect.raw(), plugin.ctx.clone(), &info, max_frames, ring, faults.clone()) };
    (unit, params, faults)
}

/// The unit on its own: a call longer than the plugin's block goes in slices, each note lands in
/// its slice at its offset there (one on a slice's first frame belongs to that slice, at 0) and
/// reaches the plugin before that slice's process call, ring params go to `setParameter` once, the
/// first pair's mean is the mono output, the time info advances by the frames processed, and
/// nothing allocates.
#[cfg(debug_assertions)]
#[test]
fn a_unit_slices_a_long_call_places_each_note_and_allocates_nothing() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let slot = device.host().slot(0);
    let probe = Probe::new();
    probe.set_level(0.5);
    probe.spread.store(0.25f32.to_bits(), Relaxed); // 0.75 left, 0.25 right
    let mut plugin = plugin_here(&slot, instrument(probe));
    let (unit, mut params, faults) = unit_here(&mut plugin, &slot, 64);
    let (unit, runs) = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 200], [0.0f32; 200]);
        unit.process(0, &input[..64], &[], &mut out[..64]);
        params.push(PluginEvent::Param { id: 1, value: 0.5 }).unwrap();
        let notes = [
            SlotEvent { offset: 10, kind: SlotEventKind::NoteOn { key: 60, velocity: 1.0 } },
            SlotEvent { offset: 64, kind: SlotEventKind::NoteOff { key: 60 } },
            SlotEvent { offset: 150, kind: SlotEventKind::NoteOn { key: 64, velocity: 0.0 } },
        ];
        // A window the shared counter was reset in runs again (`rt_allocations`); count the runs.
        let mut runs = 0;
        let allocations = engine_slot::rt_allocations(|| {
            runs += 1;
            unit.process(200, &input, &notes, &mut out)
        });
        assert_eq!(allocations, 0, "process allocates nothing");
        assert!(out.iter().all(|&x| x == 0.5), "0.75 and 0.25, their mean to mono");
        unit.stop();
        (unit, runs)
    })
    .join()
    .unwrap();
    assert_eq!(probe.processes.load(Relaxed), 1 + 4 * runs, "200 frames at 64 a block: 64 + 64 + 64 + 8");
    assert_eq!(probe.last_frames.load(Relaxed), 8);
    assert_eq!(
        (probe.note_ons.load(Relaxed), probe.note_offs.load(Relaxed), probe.event_calls.load(Relaxed)),
        (2 * runs, runs, 3 * runs),
        "effProcessEvents only for a slice with notes"
    );
    // Every event, not only the last: the slice it went to (counted in process calls, the 64-frame
    // call before the long one being the first) and its offset there.
    let event = |slice, delta, status, key| TracedEvent { slice, delta, status, key };
    let expected: Vec<TracedEvent> = (0..runs)
        .flat_map(|run| {
            let first = 1 + 4 * run;
            [event(first, 10, 0x90, 60), event(first + 1, 0, 0x80, 60), event(first + 2, 150 - 128, 0x90, 64)]
        })
        .collect();
    assert_eq!(probe.events(), expected, "the note on the second slice's first frame is that slice's, at 0");
    assert_eq!(
        (probe.slices_after_events.load(Relaxed), probe.event_calls_unanswered.load(Relaxed)),
        (3 * runs, 0),
        "each slice's events came before its process call"
    );
    assert_eq!(
        (probe.last_key.load(Relaxed), probe.last_delta.load(Relaxed), probe.last_velocity.load(Relaxed)),
        (64, 150 - 128, 1),
        "the second note in the third slice, at its offset there; the quietest note-on is 1, never a note-off"
    );
    assert_eq!(probe.bad_events.load(Relaxed), 0);
    assert_eq!((probe.sets.load(Relaxed), probe.last_set_index.load(Relaxed), probe.value(1)), (1, 1, 0.5), "the ring's param, once");
    let frames_before_last_slice = 64 + (runs - 1) * 200 + 192;
    assert_eq!(f64::from_bits(probe.time_pos.load(Relaxed)), frames_before_last_slice as f64, "the time each slice starts at");
    assert_eq!(plugin.ctx.sample_pos(), (frames_before_last_slice + 8) as u64, "and the position other threads are told");
    assert_eq!(probe.process_level.load(Relaxed), PROCESS_LEVEL_REALTIME);
    assert_eq!(faults.load(Relaxed), 0);
    assert_eq!(probe.opcodes().len(), 6, "the unit's stop called nothing: start and stop are the owner's");
    drop(unit);
    drop(plugin);
    assert_eq!(probe.count(EFF_STOP_PROCESS, 0), 1);
}

/// The unit on its own: once the plugin reports a layout change from inside a slice, the rest of
/// that call and every later call is silent and makes no plugin call (no process, no events, no
/// parameter), until the owner has resumed the plugin and rebuilt the rows to the new counts.
#[test]
fn an_io_change_mid_call_stops_every_later_plugin_call_until_the_rows_are_rebuilt() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let slot = device.host().slot(0);
    let probe = Probe::new();
    probe.set_level(0.5);
    let mut plugin = plugin_here(&slot, instrument(probe));
    let (unit, mut params, faults) = unit_here(&mut plugin, &slot, 64);
    let note = [SlotEvent { offset: 100, kind: SlotEventKind::NoteOn { key: 60, velocity: 1.0 } }];
    let (mut unit, mut params) = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 200], [1.0f32; 200]);
        probe.grow_in_process.store(4, Relaxed);
        unit.process(0, &input, &note, &mut out);
        assert!(out[..64].iter().all(|&x| x == 0.5), "the slice the plugin rendered is kept");
        assert!(out[64..].iter().all(|&x| x == 0.0), "the rest of the call is silent");
        params.push(PluginEvent::Param { id: 0, value: 0.5 }).unwrap();
        out.fill(1.0);
        unit.process(200, &input, &note, &mut out);
        assert!(out.iter().all(|&x| x == 0.0), "and so is every later call");
        (unit, params)
    })
    .join()
    .unwrap();
    assert_eq!(probe.processes.load(Relaxed), 1, "one slice, then nothing");
    assert_eq!((probe.event_calls.load(Relaxed), probe.sets.load(Relaxed)), (0, 0), "no events and no parameter either");
    assert_eq!(probe.calls_while_halted.load(Relaxed), 0);
    assert!(plugin.ctx.halted());

    // The owner's cycle, by hand: the plugin now declares four outputs.
    assert!(plugin.ctx.take_restart());
    plugin.suspend();
    let info = plugin.resume().expect("four outputs is a layout this host takes");
    assert_eq!(info.outputs, 4);
    assert!(!plugin.ctx.halted());
    unit.rearm(&info, 64);
    let unit = std::thread::spawn(move || {
        let (input, mut out) = ([0.0f32; 64], [0.0f32; 64]);
        unit.process(400, &input, &[], &mut out);
        assert!(out.iter().all(|&x| x == 0.5), "it plays again");
        unit
    })
    .join()
    .unwrap();
    assert_eq!((probe.distinct_outputs.load(Relaxed), probe.rows_missing.load(Relaxed)), (4, false));
    assert_eq!(probe.sets.load(Relaxed), 1, "the edit that waited in the ring arrives now");
    params.push(PluginEvent::Param { id: 3, value: 0.5 }).unwrap();
    params.push(PluginEvent::Param { id: 0, value: f64::NAN }).unwrap();
    let unit = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 64], [0.0f32; 64]);
        unit.process(464, &input, &[], &mut out);
        unit
    })
    .join()
    .unwrap();
    assert_eq!(probe.sets.load(Relaxed), 1, "an index the plugin never declared and a value that is no number are dropped");
    assert_eq!(faults.load(Relaxed), engine_slot::FAULT_PARAM, "and latched");

    // More notes in one slice than the event list holds (more than the engine ever queues): the
    // list's worth reaches the plugin, the rest is dropped and latched.
    let unit = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 64], [0.0f32; 64]);
        let flood = [SlotEvent { offset: 3, kind: SlotEventKind::NoteOn { key: 60, velocity: 1.0 } }; MAX_SLOT_EVENTS + 5];
        unit.process(528, &input, &flood, &mut out);
        unit
    })
    .join()
    .unwrap();
    assert_eq!((probe.note_ons.load(Relaxed), probe.event_calls.load(Relaxed)), (MAX_SLOT_EVENTS, 1));
    assert_eq!(faults.load(Relaxed), engine_slot::FAULT_PARAM | engine_slot::FAULT_EVENTS);
    drop(unit);
}

/// A plugin that settles its layout inside `effMainsChanged(1)`: what the owner installs is the
/// layout read AFTER the resume (two inputs and a 2-output effect became a 4-output instrument with
/// latency), and the report that came with it is consumed: no cycle follows.
#[test]
fn a_layout_settled_inside_the_resume_is_the_one_installed() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    probe.set_level(0.25);
    *probe.layout_on_resume.lock().unwrap() = Some((0, 4, 48));
    let (handle, _) = load(&device, 0, Shape { probe: Some(probe), ..Shape::default() });
    assert_eq!(handle.kind(), SlotKind::Instrument, "no input any more: an instrument");
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 8 && sounding(&device)), "it plays");
    assert_eq!(installed(&device, 0), Some((SlotKind::Instrument, 48)), "with the latency it settled on");
    assert_eq!((probe.distinct_outputs.load(Relaxed), probe.distinct_inputs.load(Relaxed)), (4, 0), "a row per output it has now");
    std::thread::sleep(Duration::from_millis(200)); // several owner turns
    assert_eq!(probe.opcodes(), load_calls(4096), "the report described the state just read: no cycle");
    assert_eq!(probe.calls_while_halted.load(Relaxed), 0);
    handle.unload().unwrap();
    assert_rendered_only_while_started(probe);
}

/// A restart in which the plugin settles on a layout the host refuses (no output) inside
/// `effMainsChanged(1)`: the check after the resume catches it, the plugin is stopped and
/// suspended again, and the slot stays bypassed.
#[test]
fn a_layout_the_host_refuses_settled_inside_the_resume_leaves_the_plugin_suspended() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    probe.set_level(0.25);
    let (handle, _) = load(&device, 0, instrument(probe));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 8 && sounding(&device)), "it plays");

    *probe.layout_on_resume.lock().unwrap() = Some((0, 0, 0));
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0), 1);
    assert!(wait_for(2000, || probe.count(EFF_MAINS_CHANGED, 0) == 2 && rendered_near(&device, 0.0)), "resumed, refused, suspended again");
    let mut refused = cycle_calls(4096);
    refused.extend([(EFF_STOP_PROCESS, 0), (EFF_MAINS_CHANGED, 0)]);
    assert_eq!(probe.opcodes()[load_calls(4096).len()..], refused);
    let processed = probe.processes.load(Relaxed);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(installed(&device, 0), None, "the unit stays out of the engine");
    assert_eq!(probe.processes.load(Relaxed), processed, "and the plugin is not processed");
    assert_eq!(probe.opcodes().len(), load_calls(4096).len() + refused.len(), "nor called");
    handle.unload().unwrap();
    assert_eq!(probe.opcodes().last(), Some(&(EFF_CLOSE, 0)));
    assert_eq!(probe.count(EFF_STOP_PROCESS, 0), 2, "not stopped a third time by the unload");
    assert_rendered_only_while_started(probe);
}

/// A plugin thread changes the layout and reports it right after the owner read the layout in a
/// resume, before the owner does anything else. The report stays latched: the unit goes in halted
/// (rows for the layout read, no plugin call), and the owner cycles the plugin once more, onto rows
/// for the layout it has now. (Cleared after the read, the report would be lost and the plugin
/// processed on rows for a layout it no longer has.)
#[test]
fn an_io_change_right_after_the_resume_read_the_layout_stays_latched_and_is_cycled() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    probe.set_level(0.25);
    let (handle, _) = load(&device, 0, instrument(probe));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 8 && sounding(&device)), "it plays");

    // A cycle validates twice: before the resume and after it. The second read is the one.
    probe.io_change_after_validate(1, 4);
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0), 1);
    assert!(
        wait_for(2000, || probe.count(EFF_START_PROCESS, 0) == 3 && probe.distinct_outputs.load(Relaxed) == 4),
        "the late report was kept: a second cycle, and a row for each of the four outputs"
    );
    assert_eq!(probe.calls_while_halted.load(Relaxed), 0, "no plugin call between the late report and its cycle");
    let mut cycles = cycle_calls(4096);
    cycles.extend(cycle_calls(4096));
    assert_eq!(probe.opcodes()[load_calls(4096).len()..], cycles);
    assert!(wait_for(2000, || sounding(&device)), "and it plays again");
    assert!(!probe.rows_missing.load(Relaxed));
    handle.unload().unwrap();
    assert_rendered_only_while_started(probe);
}

/// A plugin whose parameter count changes in `effOpen` (three when it was created, one once open):
/// the stored tone is checked against the count it has when the tone goes in, so a three-parameter
/// tone is refused before any `setParameter`, and the load goes on at the defaults.
#[test]
fn a_tone_is_checked_against_what_the_plugin_declares_once_it_is_open() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let dir = TempDir::new("vst2-open-shrinks");
    let state = tone::encode_vst2(&Vst2State::Params { program: 0, values: vec![0.5; 3] });
    store_state(&dir, &state);
    let probe = Probe::new();
    probe.params_on_open.store(1, Relaxed);
    let (handle, _) = load_with_tone(&device, 0, instrument(probe), Some(dir.binding(0, identity())));
    assert_eq!(handle.tone(), Some(ToneRestore::Failed), "three parameters for a plugin that now has one");
    assert_eq!((probe.sets.load(Relaxed), probe.set_programs.load(Relaxed)), (0, 0), "refused before the plugin was called for it");
    assert_eq!(handle.list_params().unwrap().len(), 1, "the load went on with the parameter it has");
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 4), "and it plays");
    handle.unload().unwrap();
    assert_eq!(dir.store().load(0, &identity()).unwrap().unwrap().state, state, "the stored tone stays");
}

/// After a restart the host refused, what the plugin declared at its last activation describes
/// nothing any more: no parameter is listed or read by those counts, a tone save makes no plugin
/// call and leaves the stored tone as it is, and all of it comes back with the next good restart.
#[test]
fn a_refused_restart_reads_no_parameter_and_keeps_the_stored_tone() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let dir = TempDir::new("vst2-stale");
    let probe = Probe::new();
    let (handle, _) = load_with_tone(&device, 0, instrument(probe), Some(dir.binding(0, identity())));
    handle.set_param(1, 0.4).unwrap();
    assert!(wait_for(2000, || probe.sets.load(Relaxed) == 1));
    assert!(!handle.take_tone().unwrap().is_empty());
    let kept = dir.store().load(0, &identity()).unwrap().unwrap().state;
    assert_eq!(tone::decode_vst2(&kept).unwrap(), Vst2State::Params { program: 0, values: vec![0.0, 0.4, 0.0] });

    probe.outputs_on_suspend.store(0, Relaxed);
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0), 1);
    assert!(wait_for(2000, || probe.count(EFF_SET_BLOCK_SIZE, 4096) == 2), "the cycle ran up to the refusal");
    probe.values[1].store(0.9f32.to_bits(), Relaxed);
    let (gets, calls) = (probe.gets.load(Relaxed), probe.opcodes().len());
    assert_eq!(handle.list_params().map(|listed| listed.len()), Ok(0), "nothing is listed by a past activation's count");
    assert!(handle.take_tone().is_err(), "and a save says so, so an export warns of the slot instead of leaving it out");
    assert!(handle.set_param(1, 0.2).is_err(), "no parameter is known meanwhile");
    assert_eq!(probe.gets.load(Relaxed), gets, "getParameter was not called");
    assert_eq!(probe.opcodes().len(), calls, "nor the dispatcher, for a name or the program");
    assert_eq!(dir.store().load(0, &identity()).unwrap().unwrap().state, kept, "the stored tone is as it was");

    // SAFETY: the unit is parked and the owner reads the effect only once the report below asks it to.
    unsafe { probe.set_outputs(2) };
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0), 1);
    assert!(wait_for(2000, || probe.count(EFF_START_PROCESS, 0) == 2), "a good restart");
    assert_eq!(handle.list_params().unwrap().iter().map(|p| p.value).collect::<Vec<_>>(), [0.0, f64::from(0.9f32), 0.0]);
    handle.unload().unwrap();
}

/// A plugin that drops from three parameters to one and reports it: a listing asked for right after
/// the report is answered by the new count, whether or not the owner's turn had come round to the
/// cycle, and no parameter the plugin no longer has is read.
#[test]
fn a_listing_asked_right_after_a_layout_report_reads_the_plugin_as_it_is_now() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    let (handle, _) = load(&device, 0, instrument(probe));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 4));
    for round in 0..20 {
        let params = if round % 2 == 0 { 1 } else { 3 };
        probe.params_on_suspend.store(params, Relaxed);
        assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0), 1);
        assert_eq!(handle.list_params().map(|listed| listed.len()), Ok(params as usize), "round {round}");
    }
    handle.unload().unwrap();
}

/// A program select that changes the plugin's parameter count: the tone's values are checked
/// against the plugin again before any of them is set, and none is.
#[test]
fn a_tone_whose_program_changes_the_parameter_count_sets_no_parameter() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let dir = TempDir::new("vst2-program-shrinks");
    store_state(&dir, &tone::encode_vst2(&Vst2State::Params { program: 1, values: vec![0.5; 3] }));
    let probe = Probe::new();
    probe.params_on_set_program.store(1, Relaxed);
    let (handle, _) = load_with_tone(&device, 0, Shape { programs: 4, ..instrument(probe) }, Some(dir.binding(0, identity())));
    assert_eq!(handle.tone(), Some(ToneRestore::Failed));
    assert_eq!((probe.set_programs.load(Relaxed), probe.sets.load(Relaxed)), (1, 0), "the program was selected, no value set");
    assert_eq!(handle.list_params().unwrap().len(), 1, "and the load went on with the plugin as it is");
    handle.unload().unwrap();
}

/// A parameter the plugin adds in a restart (three become five): its own report for a new index
/// reaches the caller with its value, and one for an index it still does not have is dropped.
#[test]
fn automation_for_a_parameter_added_in_a_restart_reaches_the_caller() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let probe = Probe::new();
    let (handle, seen) = load(&device, 0, instrument(probe));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 4));
    probe.params_on_suspend.store(5, Relaxed);
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0), 1);
    assert!(wait_for(2000, || probe.count(EFF_START_PROCESS, 0) == 2), "restarted");

    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_AUTOMATE, 5, 0, 0.9), 0);
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_AUTOMATE, 4, 0, 0.6), 0);
    let told = EngineSlotEvent::ParamChanged { id: 4, value: f64::from(0.6f32) };
    assert!(wait_for(2000, || seen.lock().unwrap().contains(&told)), "the new parameter's report arrives");
    std::thread::sleep(Duration::from_millis(100)); // a few more owner turns
    assert_eq!(*seen.lock().unwrap(), [told], "and nothing for index 5, which the plugin does not have");
    assert_eq!(handle.list_params().unwrap().len(), 5);
    handle.unload().unwrap();
}

/// A plugin whose owner (`run_with`) the test started itself, so it holds what a handle keeps to
/// itself: `running` and the request channel.
struct Driven {
    running: Arc<AtomicBool>,
    requests: Sender<OwnerRequest>,
    owner: std::thread::JoinHandle<Result<(), String>>,
    seen: Seen,
}

impl Driven {
    /// One editor request and its reply, as `EngineSlotHandle::open_editor` and `close_editor` send them.
    fn editor(&self, build: fn(Arc<AtomicBool>, SyncSender<Result<(), String>>) -> OwnerRequest) -> Result<(), String> {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        self.requests.send(build(Arc::new(AtomicBool::new(false)), reply)).map_err(|_| "owner thread gone".to_string())?;
        answer.recv_timeout(Duration::from_secs(5)).map_err(|e| format!("no reply: {e}"))?
    }

    /// As an unload: `running` down, the owner woken, then joined.
    fn unload(self) -> Result<(), String> {
        self.running.store(false, Release);
        let _ = self.requests.send(OwnerRequest::Wake);
        self.owner.join().expect("the owner thread ends")
    }
}

/// Start the production owner for the fixture in `shape` and wait until its unit is installed.
fn drive(device: &TestDevice, slot: usize, shape: Shape, tone: Option<ToneBinding>) -> Driven {
    let seen = Seen::default();
    let sink_seen = seen.clone();
    let running = Arc::new(AtomicBool::new(true));
    let (params_tx, params) = RingBuffer::<PluginEvent>::new(8);
    let (requests, owner_requests) = channel();
    let (ready, loaded) = super::super::load_ready_channel();
    let ctx = OwnerCtx {
        slot: device.host().slot(slot),
        running: running.clone(),
        requests: owner_requests,
        params,
        params_tx: Arc::new(Mutex::new(params_tx)),
        param_ids: Default::default(),
        sink: Arc::new(move |e| sink_seen.lock().unwrap().push(e)),
        editor_parent: 0,
        ready,
        tone: ToneKeeper::new(tone, Arc::new(AtomicBool::new(false))),
    };
    let owner = std::thread::spawn(move || {
        run_with(ctx, ID, move || {
            fixture::arm(shape);
            Ok(Opened { module: None, entry: fixture::entry, file_stem: "fixture-file".to_string() })
        })
    });
    let report = loaded.recv_timeout(Duration::from_secs(5)).expect("the owner reports its load");
    assert!(report.is_ok(), "the fixture loads: {:?}", report.err());
    Driven { running, requests, owner, seen }
}

/// A parameter the plugin's own editor moved after the owner's last drain, with the unload already
/// asked for: the report is still latched when the loop ends. The unload takes it for the tone (the
/// stored tone holds the moved value) and tells no caller.
#[test]
fn automation_reported_after_the_last_owner_turn_is_in_the_tone_the_unload_stores() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let dir = TempDir::new("vst2-late-automation");
    let probe = Probe::new();
    let driven = drive(&device, 0, instrument(probe), Some(dir.binding(0, identity())));
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 4));

    // Hold the owner inside `effIdle`, which it sends after this turn's drain of the latch.
    let (inside, entered) = channel();
    let (release, go) = channel::<()>();
    *probe.on_idle.lock().unwrap() = Some(Box::new(move |_| {
        inside.send(()).unwrap();
        let _ = go.recv_timeout(Duration::from_secs(5));
    }));
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_NEED_IDLE, 0, 0, 0.0), 1);
    entered.recv_timeout(Duration::from_secs(5)).expect("the owner is inside effIdle");
    probe.values[1].store(0.6f32.to_bits(), Relaxed);
    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_AUTOMATE, 1, 0, 0.6), 0);
    // The unload is asked for before the owner gets on: its loop ends without another drain.
    driven.running.store(false, Release);
    driven.requests.send(OwnerRequest::Wake).unwrap();
    release.send(()).unwrap();
    let seen = driven.seen.clone();
    driven.unload().expect("it unloads");
    assert_eq!(stored(&dir), Some(Vst2State::Params { program: 0, values: vec![0.0, 0.6, 0.0] }), "the moved value is kept");
    assert_eq!(*seen.lock().unwrap(), [], "and no caller is told from the teardown");
    assert_eq!(probe.closes.load(Relaxed), 1);
}

/// The owner loop's editor arms, through real requests, on a host window that is never shown: open
/// (once), `effEditIdle` every turn while it is open and none after, a size the plugin asks for
/// from another thread applied on the owner's next turn, a close request, and the window's own
/// close box, which tells the caller. Each close saves the tone.
#[test]
fn the_owner_loop_serves_editor_requests_the_close_box_and_a_foreign_resize() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let dir = TempDir::new("vst2-editor-loop");
    let probe = Probe::new();
    let shape = Shape { flags: SYNTH | EFF_FLAGS_HAS_EDITOR, ..instrument(probe) };
    let driven = drive(&device, 0, shape, Some(dir.binding(0, identity())));
    let window = || HWND(probe.editor_parent.load(Relaxed) as *mut c_void);
    // SAFETY: a plain query on a window handle; a dead one answers false.
    let visible = |hwnd: HWND| unsafe { IsWindowVisible(hwnd) }.as_bool();
    let idles_stopped = || {
        let idles = probe.edit_idles.load(Relaxed);
        std::thread::sleep(Duration::from_millis(150)); // several owner turns
        probe.edit_idles.load(Relaxed) == idles
    };
    assert!(idles_stopped(), "no effEditIdle before an editor is open");

    assert_eq!(driven.editor(OwnerRequest::OpenEditor), Ok(()));
    let hwnd = window();
    assert_eq!((probe.edit_opens.load(Relaxed), client_size(hwnd)), (1, (400, 300)), "open, at the plugin's size");
    assert!(!visible(hwnd), "a test's editor window is never shown");
    let idles = probe.edit_idles.load(Relaxed);
    assert!(wait_for(2000, || probe.edit_idles.load(Relaxed) > idles + 3), "effEditIdle runs while the editor is open");
    assert_eq!(driven.editor(OwnerRequest::OpenEditor), Ok(()));
    assert_eq!(probe.edit_opens.load(Relaxed), 1, "an open editor is not opened twice");

    assert_eq!(plugin_thread_calls_host(probe, AUDIO_MASTER_SIZE_WINDOW, 520, 340, 0.0), 0, "another thread: latched");
    assert!(wait_for(2000, || client_size(hwnd) == (520, 340)), "and applied by the owner's next turn");
    assert!(!visible(hwnd));

    probe.values[0].store(0.3f32.to_bits(), Relaxed);
    assert_eq!(driven.editor(OwnerRequest::CloseEditor), Ok(()));
    assert_eq!(probe.edit_closes.load(Relaxed), 1);
    assert_eq!(client_size(hwnd), (0, 0), "the host window is gone");
    assert_eq!(stored(&dir), Some(Vst2State::Params { program: 0, values: vec![0.3, 0.0, 0.0] }), "the close saved the tone");
    assert!(idles_stopped(), "no effEditIdle once it is closed");
    assert_eq!(driven.editor(OwnerRequest::CloseEditor), Ok(()), "closing a closed editor answers the same");
    assert_eq!(probe.edit_closes.load(Relaxed), 1);

    // The close box: the window's own WM_CLOSE, which the owner's pump dispatches.
    assert_eq!(driven.editor(OwnerRequest::OpenEditor), Ok(()));
    let hwnd = window();
    assert_eq!(probe.edit_opens.load(Relaxed), 2);
    probe.values[0].store(0.7f32.to_bits(), Relaxed);
    assert_eq!(*driven.seen.lock().unwrap(), [], "a request's close tells nobody: its caller knows");
    // SAFETY: a message posted to the owner thread's window, which that thread dispatches.
    unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }.expect("post WM_CLOSE");
    assert!(wait_for(2000, || *driven.seen.lock().unwrap() == [EngineSlotEvent::EditorClosed]), "the caller is told");
    let saved = Vst2State::Params { program: 0, values: vec![0.7, 0.0, 0.0] };
    assert!(wait_for(2000, || stored(&dir).as_ref() == Some(&saved)), "and the tone is saved");
    assert_eq!((probe.edit_closes.load(Relaxed), client_size(hwnd)), (2, (0, 0)));
    assert!(idles_stopped());
    assert_eq!(*driven.seen.lock().unwrap(), [EngineSlotEvent::EditorClosed], "once");

    driven.unload().expect("it unloads");
    assert_eq!(probe.closes.load(Relaxed), 1);
    assert_lifecycle_on_the_owner(probe);
    assert_rendered_only_while_started(probe);
}

/// A cycle whose unit the engine does not take (the slot holds another unit): the plugin, resumed
/// for the install, is stopped and suspended again, as every other failed cycle leaves it.
#[test]
fn a_cycle_whose_unit_the_engine_refuses_leaves_the_plugin_suspended() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let slot = device.host().slot(0);
    let (holder, probe) = (Probe::new(), Probe::new());
    let mut holding = plugin_here(&slot, instrument(holder));
    let (unit, _holder_params, _) = unit_here(&mut holding, &slot, 4096);
    assert!(slot.install(unit, 48_000).is_ok(), "the slot takes the first unit");

    let mut plugin = plugin_here(&slot, instrument(probe));
    let (unit, _params, _) = unit_here(&mut plugin, &slot, 4096);
    let before = probe.opcodes().len();
    let (unit, err) = reinstall(&mut plugin, unit, &slot).expect_err("the slot is taken");
    assert!(err.contains("already holds a unit"), "{err}");
    assert!(!plugin.running, "suspended");
    let mut refused = cycle_calls(4096);
    refused.extend([(EFF_STOP_PROCESS, 0), (EFF_MAINS_CHANGED, 0)]);
    assert_eq!(probe.opcodes()[before..], refused, "resumed for the install, then stopped and suspended");
    assert_eq!(probe.processes.load(Relaxed), 0);
    drop(unit);
    drop(plugin);
    assert_eq!(probe.opcodes().last(), Some(&(EFF_CLOSE, 0)));
    assert_eq!(probe.count(EFF_STOP_PROCESS, 0), 2, "the close did not stop it again");

    let back = slot.remove(REMOVE_TIMEOUT).expect("the first unit comes back").map(own);
    assert!(back.is_some());
    drop(back);
    drop(holding);
    assert_rendered_only_while_started(holder);
}

/// The unit on its own: the plugin raises its pin counts (2 in, 2 out to 40 in, 64 out) INSIDE a
/// process call and at once reads every input row and fills every output row it now declares.
/// Each of those rows is the unit's own scratch memory: the inputs read silence, the writes land
/// where nothing reads, the slot's own output for that call is what the plugin rendered, and the
/// next call makes no plugin call.
#[test]
fn a_plugin_that_grows_its_pins_inside_a_process_call_reads_and_writes_only_the_units_rows() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let slot = device.host().slot(0);
    let probe = Probe::new();
    let mut plugin = plugin_here(&slot, Shape { probe: Some(probe), ..Shape::default() });
    let (unit, _params, faults) = unit_here(&mut plugin, &slot, 64);
    let unit = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.3f32; 64], [0.0f32; 64]);
        unit.process(0, &input, &[], &mut out);
        assert!(out.iter().all(|&x| x == 0.3), "an effect: the input, echoed");
        probe.grow_before_render.store(40 << 8 | 64, Relaxed);
        out.fill(0.0);
        unit.process(64, &input, &[], &mut out);
        assert!(out.iter().all(|&x| x == 0.3), "the slot's own output for the call it grew in is intact");
        out.fill(1.0);
        unit.process(128, &input, &[], &mut out);
        assert!(out.iter().all(|&x| x == 0.0), "the next call is halted: silence");
        unit
    })
    .join()
    .unwrap();
    assert_eq!(probe.processes.load(Relaxed), 2, "and makes no plugin call");
    assert!(!probe.rows_missing.load(Relaxed), "all 64 entries of both arrays were there");
    assert_eq!(
        (probe.grown_inputs_read.load(Relaxed), probe.grown_inputs_loud.load(Relaxed), probe.grown_outputs_filled.load(Relaxed)),
        (38, 0, 62),
        "every new input row read silence; every new output row took a whole slice"
    );
    assert_eq!((probe.calls_while_halted.load(Relaxed), faults.load(Relaxed)), (0, 0));
    assert!(plugin.ctx.halted());
    drop(unit);
}

/// The unit on its own: a plugin with one output plays that output, not half of it.
#[test]
fn a_single_output_plugin_plays_its_one_output() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let slot = device.host().slot(0);
    let probe = Probe::new();
    probe.set_level(0.5);
    probe.spread.store(0.25f32.to_bits(), Relaxed); // its one output at 0.75
    let mut plugin = plugin_here(&slot, Shape { outputs: 1, ..instrument(probe) });
    let (unit, _params, _) = unit_here(&mut plugin, &slot, 64);
    let unit = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 100], [0.0f32; 100]);
        unit.process(0, &input, &[], &mut out);
        assert!(out.iter().all(|&x| x == 0.75), "the one output, as it is");
        unit
    })
    .join()
    .unwrap();
    assert_eq!((probe.processes.load(Relaxed), probe.distinct_outputs.load(Relaxed)), (2, 1));
    drop(unit);
}

/// The unit on its own: keys the plugin holds when it reports a layout change stay down in the
/// plugin, since the engine's note-offs for them arrive while the unit is halted and are dropped.
/// The first slice after the owner's cycle releases exactly those keys, at offset 0, before its own
/// events and in the same `effProcessEvents` call; a key released the normal way before the halt
/// gets no second note-off, and the call that carries the releases allocates nothing.
#[cfg(debug_assertions)]
#[test]
fn the_keys_a_halt_left_down_are_released_in_the_first_slice_after_the_cycle() {
    fn on(offset: u32, key: u8) -> SlotEvent {
        SlotEvent { offset, kind: SlotEventKind::NoteOn { key, velocity: 1.0 } }
    }
    fn off(offset: u32, key: u8) -> SlotEvent {
        SlotEvent { offset, kind: SlotEventKind::NoteOff { key } }
    }
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let slot = device.host().slot(0);
    let probe = Probe::new();
    let mut plugin = plugin_here(&slot, instrument(probe));
    let (unit, _params, faults) = unit_here(&mut plugin, &slot, 64);
    let mut unit = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 64], [0.0f32; 64]);
        // Three keys go down (one in each word of the set); one comes up again the normal way.
        unit.process(0, &input, &[on(0, 60), on(5, 100), on(9, 67), off(20, 67)], &mut out);
        // The plugin reports a layout change from inside its next call...
        probe.grow_in_process.store(4, Relaxed);
        unit.process(64, &input, &[], &mut out);
        // ...and the engine's note-offs for the removal find the unit halted.
        unit.process(128, &input, &[off(0, 60), off(0, 100)], &mut out);
        unit
    })
    .join()
    .unwrap();
    let event = |slice, delta, status, key| TracedEvent { slice, delta, status, key };
    let before_the_halt = [event(0, 0, 0x90, 60), event(0, 5, 0x90, 100), event(0, 9, 0x90, 67), event(0, 20, 0x80, 67)];
    assert_eq!(probe.events(), before_the_halt, "the engine's note-offs were dropped with everything else");
    assert_eq!((probe.event_calls.load(Relaxed), probe.processes.load(Relaxed)), (1, 2));

    // The owner's cycle, by hand.
    assert!(plugin.ctx.take_restart());
    plugin.suspend();
    let info = plugin.resume().expect("four outputs is a layout this host takes");
    unit.rearm(&info, 64);
    let (unit, runs) = std::thread::spawn(move || {
        let (input, mut out) = ([0.0f32; 64], [0.0f32; 64]);
        // A window the shared counter was reset in runs again (`rt_allocations`); count the runs.
        let mut runs = 0;
        let allocations = engine_slot::rt_allocations(|| {
            runs += 1;
            unit.process(192, &input, &[on(7, 72)], &mut out)
        });
        assert_eq!(allocations, 0, "the call that releases them allocates nothing");
        // A slice with nothing to say: no releases are left, so no event call.
        unit.process(256, &input, &[], &mut out);
        (unit, runs)
    })
    .join()
    .unwrap();
    let events = probe.events();
    assert_eq!(
        events[4..7],
        [event(2, 0, 0x80, 60), event(2, 0, 0x80, 100), event(2, 7, 0x90, 72)],
        "both held keys released at offset 0, before the slice's own note"
    );
    assert_eq!(events.len(), 7 + (runs - 1), "once: a later slice releases nothing again");
    assert!(events[7..].iter().all(|e| (e.delta, e.status, e.key) == (7, 0x90, 72)));
    assert_eq!(probe.event_calls.load(Relaxed), 1 + runs, "the releases and the slice's note went in one call");
    assert_eq!(probe.note_offs.load(Relaxed), 3, "key 67, released before the halt, got no second note-off");
    assert_eq!((probe.bad_events.load(Relaxed), probe.calls_while_halted.load(Relaxed), faults.load(Relaxed)), (0, 0, 0));
    assert_eq!((probe.slices_after_events.load(Relaxed), probe.event_calls_unanswered.load(Relaxed)), (1 + runs, 0));
    drop(unit);
}
