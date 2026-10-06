//! The VST2 unit and owner against the in-process fixture (`vst2_fixture.rs`: a real `AEffect`
//! behind a real entry, no DLL), loaded with `engine_slot` into a test device's engine, where the
//! production `run_with` owns it. The editor and the unit are also driven directly, with the test
//! thread standing in for the owner. No audio device and no visible window: the device renders to
//! memory and an editor's host window is never shown.
use super::super::engine_slot::{self, EngineSlotEvent, EngineSlotHandle, PluginFormat};
use super::super::super::editor_window::client_size;
use super::super::super::tone::{self, TempDir, ToneBinding, ToneIdentity};
use super::super::super::vst2::fixture::{self, Probe, Shape};
use super::super::super::vst2::host_callback;
use super::*;
use crate::engine_io::test_rig::TestDevice;
use lf_engine::{Command, NoteTarget, TimedCommand};
use rtrb::{Producer, RingBuffer};
use std::sync::Mutex;
use std::time::Duration;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN};

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

fn rendered_near(device: &TestDevice, level: f32) -> bool {
    device.output.lock().unwrap().iter().rev().take(256).all(|&x| (x - level).abs() < 1e-3)
}

/// Whether an instrument at level 0.25 or more is what the device renders. Not an exact level: an
/// instrument reaches the output through the master limiter, an effect's wet signal does not.
fn sounding(device: &TestDevice) -> bool {
    device.output.lock().unwrap().iter().rev().take(256).all(|&x| x > 0.2)
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
    std::thread::sleep(Duration::from_millis(200)); // several owner turns
    assert_eq!(probe.opcodes(), load_calls(4096), "and no cycle followed");
    handle.unload().unwrap();
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
}

/// Sixteen outputs with signal on the first pair only, as a multi-output instrument's main pair:
/// the slot plays that pair, as loud as a plain stereo instrument at the same level, not the mean
/// of all sixteen (an eighth of it).
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
    probe.signal_outputs.store(2, Relaxed);
    let (handle, _) = load(&device, 0, Shape { outputs: 16, ..instrument(probe) });
    assert!(wait_for(2000, || probe.processes.load(Relaxed) > 8));
    let level = level_once_settled(&device);
    assert!((level - reference).abs() < 1e-3, "the pair at full level: {level} beside {reference}");
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

/// A plugin with `effFlagsProgramChunks` keeps its bank chunk, which restores its own program. This
/// one's `effSetProgram` clears the chunk: selecting a program after `effSetChunk` would lose it.
/// Its `effSetChunk` answers 0, which is no refusal.
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
    let saved = Vst2State::Chunk { program: 1, chunk: b"bank v".to_vec() };
    assert_eq!(tone::decode_vst2(&file.state).unwrap(), saved);
    assert_eq!(stored(&dir), Some(saved));
    assert_eq!(first_run.gets.load(Relaxed), 3, "the listing's reads only: a chunk plugin's parameters are not saved");
    handle.unload().unwrap();

    let second_run = Probe::new();
    let (handle, _) = load_with_tone(&device, 0, shape(second_run), Some(dir.binding(0, identity())));
    assert_eq!(handle.tone(), Some(ToneRestore::Restored));
    assert_eq!(*second_run.chunk.lock().unwrap(), b"bank v");
    assert_eq!((second_run.set_chunks.load(Relaxed), second_run.set_programs.load(Relaxed)), (1, 0));
    assert!(first(second_run, EFF_SET_CHUNK, 6) < first(second_run, EFF_MAINS_CHANGED, 1), "restored before it resumes");
    assert!(wait_for(2000, || second_run.processes.load(Relaxed) > 4), "and it plays");

    // A change is saved by the unload, after the plugin stopped and before it closes.
    *second_run.chunk.lock().unwrap() = b"bank w".to_vec();
    handle.set_param(0, 0.1).unwrap();
    handle.unload().unwrap();
    assert_eq!(stored(&dir), Some(Vst2State::Chunk { program: 0, chunk: b"bank w".to_vec() }));
    let calls = second_run.opcodes();
    let chunk_asked = calls.iter().position(|&(opcode, _)| opcode == EFF_GET_CHUNK).unwrap();
    assert!(first(second_run, EFF_MAINS_CHANGED, 0) < chunk_asked && chunk_asked < first(second_run, EFF_CLOSE, 0));
}

/// The chunk is the plugin's memory and its length the plugin's word. A length of 0 or less is
/// nothing to save; one past what a tone holds is refused. Either way nothing is read at the
/// pointer (this fixture's chunk is four bytes long whatever it claims) and nothing is stored.
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
    probe.chunk.lock().unwrap().clear();
    assert_eq!(handle.take_tone(), Ok(Vec::new()), "an empty chunk");
    *probe.chunk.lock().unwrap() = b"real".to_vec();
    assert!(!handle.take_tone().unwrap().is_empty());
    assert_eq!(stored(&dir), Some(Vst2State::Chunk { program: 0, chunk: b"real".to_vec() }));
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
        ("five parameters for three", tone::encode_vst2(&Vst2State::Params { program: 0, values: vec![0.5; 5] })),
        ("a program the plugin lacks", tone::encode_vst2(&Vst2State::Params { program: 7, values: vec![0.5; 3] })),
        ("a negative program", tone::encode_vst2(&Vst2State::Params { program: -1, values: vec![0.5; 3] })),
        ("a chunk for a plugin without chunks", tone::encode_vst2(&Vst2State::Chunk { program: 0, chunk: b"bank".to_vec() })),
        ("no container at all", b"garbage".to_vec()),
        ("another format's container", tone::encode_vst3(b"component", b"")),
    ];
    for (what, state) in mismatches {
        store_state(&dir, &state);
        let probe = Probe::new();
        let (handle, _) = load_with_tone(&device, 0, instrument(probe), Some(dir.binding(0, identity())));
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
    assert_eq!(bare.opcodes(), [(EFF_OPEN, 0)], "a plugin without the flag is not asked");
    drop(plugin);

    let probe = Probe::new();
    *probe.rect_open.lock().unwrap() = (640, 480);
    *probe.size_in_open.lock().unwrap() = Some((500, 350));
    let mut plugin = plugin_here(&slot, Shape { flags: SYNTH | EFF_FLAGS_HAS_EDITOR, ..instrument(probe) });
    let hwnd = plugin.embed_editor(0).expect("the editor opens");
    assert_eq!(probe.editor_parent.load(Relaxed), hwnd.0 as isize, "the plugin got the host window");
    assert_eq!(*probe.size_answers.lock().unwrap(), [1], "a size asked from inside effEditOpen is granted there");
    assert_eq!(client_size(hwnd), (640, 480), "then the size the plugin reports once it is open: not 400×300");
    assert_eq!(probe.opcodes()[1..], [(EFF_EDIT_GET_RECT, 0), (EFF_EDIT_OPEN, 0), (EFF_EDIT_GET_RECT, 0)]);
    assert_eq!(plugin.embed_editor(0).map(|again| again == hwnd), Ok(true), "an open editor is not opened twice");
    assert_eq!(probe.edit_opens.load(Relaxed), 1);

    let ask = |width: i32, height: isize| plugin_calls_host(probe, AUDIO_MASTER_SIZE_WINDOW, width, height, 0.0);
    assert_eq!(ask(800, 600), 1, "from the owner thread: resized here and now");
    assert_eq!(client_size(hwnd), (800, 600));
    // SAFETY: plain metric queries.
    let screen = unsafe { (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)) };
    assert_eq!(ask(screen.0 + 2000, screen.1 as isize + 2000), 0, "a size Windows clamps is not granted");
    assert_eq!(client_size(hwnd), (800, 600), "and the window keeps the size it had");
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
/// its slice at its offset there, ring params go to `setParameter` once, the first pair is mixed to
/// mono, the time info advances by the frames processed, and nothing allocates.
#[cfg(debug_assertions)]
#[test]
fn a_unit_slices_a_long_call_places_each_note_and_allocates_nothing() {
    let _one = engine_slot::one_engine_test_at_a_time();
    let device = device(48_000);
    let slot = device.host().slot(0);
    let probe = Probe::new();
    probe.set_level(0.5);
    let mut plugin = plugin_here(&slot, instrument(probe));
    let (unit, mut params, faults) = unit_here(&mut plugin, &slot, 64);
    let (unit, runs) = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 200], [0.0f32; 200]);
        unit.process(0, &input[..64], &[], &mut out[..64]);
        params.push(PluginEvent::Param { id: 1, value: 0.5 }).unwrap();
        let notes = [
            SlotEvent { offset: 10, kind: SlotEventKind::NoteOn { key: 60, velocity: 1.0 } },
            SlotEvent { offset: 150, kind: SlotEventKind::NoteOn { key: 64, velocity: 0.0 } },
        ];
        // A window the shared counter was reset in runs again (`rt_allocations`); count the runs.
        let mut runs = 0;
        let allocations = engine_slot::rt_allocations(|| {
            runs += 1;
            unit.process(200, &input, &notes, &mut out)
        });
        assert_eq!(allocations, 0, "process allocates nothing");
        assert!(out.iter().all(|&x| x == 0.5), "both outputs at 0.5, their mean to mono");
        unit.stop();
        (unit, runs)
    })
    .join()
    .unwrap();
    assert_eq!(probe.processes.load(Relaxed), 1 + 4 * runs, "200 frames at 64 a block: 64 + 64 + 64 + 8");
    assert_eq!(probe.last_frames.load(Relaxed), 8);
    assert_eq!((probe.note_ons.load(Relaxed), probe.event_calls.load(Relaxed)), (2 * runs, 2 * runs), "effProcessEvents only for a slice with notes");
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
    assert_eq!(probe.opcodes().len(), 5, "the unit's stop called nothing: start and stop are the owner's");
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
