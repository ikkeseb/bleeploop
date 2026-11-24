//! Built-in effect units (`lf_engine::units`, `docs/unit-contract.md` §5 items 1–4, E3a: ECHO and
//! RING MOD):
//! - Golden pins of today's `input_fx` sends stored under `tests/fixtures/sends/` in the same 58-byte
//!   IEEE float32 WAV + `manifest.json` layout as `tests/fixtures/tone/` (regenerable while
//!   `input_fx.rs` exists via `cargo test -p lf-engine --test units -- --ignored write_send_pins`).
//! - §5 item 1: alone, each unit renders its pin bit for bit per channel (record tap and monitor),
//!   runs independently per channel, follows tempo/meter and clamps params, and at level 0 outputs
//!   the input bit for bit.
//! - §5 item 2: composition `[X, Y]` equals `Y` run on `X`'s output bit for bit, and non-commuting
//!   pairs (`[Echo, Ring]` vs `[Ring, Echo]`, and pairs with E2's clip/gain fakes) differ past a
//!   stated floor.
//! - §5 item 3: settled bypass is the input bit for bit with latency unchanged; a bypassed ECHO rings
//!   out; un-bypass fades in over 20 ms from the command's frame.
//! - §5 item 4: two ECHOs at different times in one slot (and one in each slot): bypassing the first
//!   rings its tail through the second as item 2 composes it, and removing one leaves the other's line
//!   untouched.
//!
//! Every render runs through `tests/common::Rig` under `assert_no_alloc`, bit-identical across block
//! sizes 1, 32, 64, 127, 128, 480 and 1024 (`docs/unit-contract.md` §9).

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::refs::{sha256_f32, write_wav, Mulberry32};
use common::{Delay, Opts, Rig};
use lf_engine::dsp::fdlibm;
use lf_engine::grid::{Frame, Meter};
use lf_engine::units::{EchoUnit, RingUnit};
use lf_engine::{
    Command, Event, InputSend, InputSendParam, LaneState, Pair, SlotEvent, UnitCx, UnitId,
    UnitInfo, UnitKind, UnitParam, UnitProcessor,
};
use serde::{Deserialize, Serialize};

const BLOCKS: [usize; 7] = [1, 32, 64, 127, 128, 480, 1024];
const SR: u32 = 48_000;
const F0: Frame = 48_000;
const PIN_FRAMES: usize = 2 * SR as usize;
/// Frame offset from `F0` where the pin scenarios switch the send/unit on (and apply alt params):
/// mid-quantum so off, 20 ms ramp-in from the next quantum boundary, and steady state are all pinned.
const ON_OFFSET: Frame = 200;
const EIGHTH: Frame = 12_000;
const SIXTEENTH: Frame = 6_000;
const DOTTED_EIGHTH: Frame = 18_000;
const FADE_10MS: Frame = 480;

// ── Pin fixtures (`tests/fixtures/sends/`) ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PinManifest {
    note: String,
    rate: u32,
    frames: usize,
    on_offset: Frame,
    input_sha256: String,
    scenarios: Vec<PinScenario>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PinScenario {
    id: String,
    send: String,
    channels: usize,
    file: String,
    sha256: String,
    params: Vec<(String, f64)>,
}

fn pin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sends")
}

fn read_wav(path: &Path, expected_rate: u32, expected_channels: usize, expected_frames: usize) -> Vec<Vec<f32>> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(&bytes[0..4], b"RIFF", "{}", path.display());
    assert_eq!(u16::from_le_bytes([bytes[20], bytes[21]]), 3, "{}: not IEEE float", path.display());
    let channels = u16::from_le_bytes([bytes[22], bytes[23]]) as usize;
    assert_eq!(channels, expected_channels, "{}", path.display());
    assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), expected_rate, "{}", path.display());
    let data = &bytes[58..];
    let frames = data.len() / 4 / channels;
    assert_eq!(frames, expected_frames, "{}", path.display());
    let mut out = vec![Vec::with_capacity(frames); channels];
    for (k, chunk) in data.chunks_exact(4).enumerate() {
        out[k % channels].push(f32::from_le_bytes(chunk.try_into().unwrap()));
    }
    out
}

fn load_manifest() -> PinManifest {
    let text = std::fs::read_to_string(pin_dir().join("manifest.json")).expect("read sends pin manifest");
    serde_json::from_str(&text).expect("parse sends pin manifest")
}

/// Deterministic 2 s input at 48 kHz using only portable arithmetic (`Mulberry32` + `fdlibm::sin`),
/// so its bits are identical across platforms: a 110 -> 330 Hz sawtooth chirp, a 173 Hz sine, and
/// short seeded noise bursts every quarter second.
fn pin_input() -> Arc<Vec<f32>> {
    let mut rng = Mulberry32::new(19);
    let sr = SR as f64;
    let quarter = (SR / 4) as usize;
    let mut phase = 0.0f64;
    let mut out = Vec::with_capacity(PIN_FRAMES);
    for n in 0..PIN_FRAMES {
        phase += (110.0 + 220.0 * (n as f64 / PIN_FRAMES as f64)) / sr;
        phase -= phase.floor();
        let saw = 2.0 * phase - 1.0;
        let sine = fdlibm::sin(std::f64::consts::TAU * 173.0 * n as f64 / sr);
        let noise = rng.next_f64() * 2.0 - 1.0;
        let burst = if n % quarter < quarter / 12 { 1.0 } else { 0.0 };
        out.push((0.22 * saw + 0.18 * sine + 0.20 * noise * burst) as f32);
    }
    Arc::new(out)
}

#[derive(Clone, Copy)]
struct PinSpec {
    id: &'static str,
    send: InputSend,
    params: &'static [(InputSendParam, f64)],
}

const PIN_SPECS: [PinSpec; 4] = [
    PinSpec { id: "echo-default-48000", send: InputSend::Echo, params: &[] },
    PinSpec {
        id: "echo-alt-48000",
        send: InputSend::Echo,
        params: &[
            (InputSendParam::EchoTime, 2.0), // 1/8.
            (InputSendParam::EchoFeedback, 0.72),
            (InputSendParam::EchoLevel, 0.85),
        ],
    },
    PinSpec { id: "ring-default-48000", send: InputSend::Ring, params: &[] },
    PinSpec {
        id: "ring-alt-48000",
        send: InputSend::Ring,
        params: &[
            (InputSendParam::RingFreq, 173.5),
            (InputSendParam::RingLevel, 0.85),
        ],
    },
];

fn assert_bits_eq(a: &[f32], b: &[f32], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: length mismatch");
    if let Some(k) = a.iter().zip(b).position(|(x, y)| x.to_bits() != y.to_bits()) {
        panic!(
            "{what}: first diff at frame {k}: got {} ({:#010x}) vs want {} ({:#010x})",
            a[k],
            a[k].to_bits(),
            b[k],
            b[k].to_bits()
        );
    }
}

/// Render today's `input_fx` send on slot 0 live and empty, master at 1, for 2 s at 48 kHz.
fn render_send_pin(spec: PinSpec, input: &Arc<Vec<f32>>, block: usize) -> Vec<f32> {
    let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
    let sig = input.clone();
    rig.set_input(move |f| sig.get((f - F0) as usize).copied().unwrap_or(0.0));
    let on = F0 + ON_OFFSET;
    for &(param, value) in spec.params {
        rig.send_at(on, Command::SetInputSendParam(param, value));
    }
    rig.send_at(on, Command::SetInputSend(spec.send, true));
    rig.keep_output();
    rig.advance(PIN_FRAMES as Frame);
    assert_bits_eq(&rig.record, &rig.record_right, "send pin record L == R");
    assert_bits_eq(&rig.monitor, &rig.monitor_right, "send pin monitor L == R");
    assert_bits_eq(&rig.record, &rig.monitor, "send pin record == monitor");
    rig.record
}

/// Regenerates `tests/fixtures/sends/` from today's `input_fx` sends. Run explicitly when creating
/// or refreshing pins before `input_fx.rs` is removed:
/// `cargo test -p lf-engine --test units -- --ignored write_send_pins`
#[test]
#[ignore = "regenerates tests/fixtures/sends/ pins from InputFx"]
fn write_send_pins() {
    let dir = pin_dir();
    std::fs::create_dir_all(&dir).expect("create sends pin dir");
    let input = pin_input();
    let mut scenarios = Vec::new();
    for spec in PIN_SPECS {
        let pcm = render_send_pin(spec, &input, 128);
        let file = format!("{}.wav", spec.id);
        write_wav(&dir.join(&file), SR, std::slice::from_ref(&pcm));
        scenarios.push(PinScenario {
            id: spec.id.to_string(),
            send: spec.send.key().to_string(),
            channels: 1,
            file,
            sha256: sha256_f32(&[&pcm]),
            params: spec.params.iter().map(|(p, v)| (p.key().to_string(), *v)).collect(),
        });
    }
    let manifest = PinManifest {
        note: "Pinned renders of input_fx ECHO and RING MOD (slot 0 live and empty, master 1, 2 s at 48 kHz) before input_fx.rs is retired (docs/unit-contract.md §5 item 1). Float32 WAVs in encodeWav's 58-byte layout, matching tests/fixtures/tone/.".to_string(),
        rate: SR,
        frames: PIN_FRAMES,
        on_offset: ON_OFFSET,
        input_sha256: sha256_f32(&[&input]),
        scenarios,
    };
    let json = serde_json::to_string_pretty(&manifest).expect("serialize pin manifest");
    std::fs::write(dir.join("manifest.json"), format!("{json}\n")).expect("write pin manifest");
}

fn load_pin(id: &str) -> Vec<f32> {
    let manifest = load_manifest();
    assert_eq!(manifest.rate, SR);
    assert_eq!(manifest.frames, PIN_FRAMES);
    assert_eq!(manifest.on_offset, ON_OFFSET);
    let input = pin_input();
    assert_eq!(sha256_f32(&[&input]), manifest.input_sha256, "pin input drifted");
    let s = manifest.scenarios.iter().find(|s| s.id == id).unwrap_or_else(|| panic!("missing pin scenario {id}"));
    let ch = read_wav(&pin_dir().join(&s.file), manifest.rate, s.channels, manifest.frames);
    assert_eq!(sha256_f32(&[&ch[0]]), s.sha256, "pin wav {id} sha256 mismatch");
    ch.into_iter().next().unwrap()
}

/// Today's `input_fx` sends still match the stored pins bit for bit (record tap and monitor).
#[test]
fn sends_match_pinned_fixtures() {
    let input = pin_input();
    for spec in PIN_SPECS {
        let pin = load_pin(spec.id);
        let got = render_send_pin(spec, &input, 128);
        assert_bits_eq(&got, &pin, spec.id);
    }
}

// ── E2-style simple fakes for composition tests ──────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum FakeDsp {
    Clip(f32),
    Gain(f32),
}

struct FakeUnit(FakeDsp);

impl UnitProcessor for FakeUnit {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: false, latency: 0 }
    }

    fn process(&mut self, _cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        for (o, i) in out.into_iter().zip(input) {
            for (y, &x) in o.iter_mut().zip(i) {
                *y = match self.0 {
                    FakeDsp::Clip(limit) => x.clamp(-limit, limit),
                    FakeDsp::Gain(g) => x * g,
                };
            }
        }
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

fn drain_returns(rig: &mut Rig) {
    rig.engine.evict_slots();
    for slot in 0..2 {
        while let Some(r) = rig.port(slot).returned_unit() {
            drop(r.unit);
        }
    }
}

// ── §5 item 1: Alone, each unit renders its pin bit for bit per channel ──────────────────────────────

fn run_unit_pin(spec: PinSpec, input: &Arc<Vec<f32>>, block: usize) -> ([Vec<f32>; 2], [Vec<f32>; 2], Vec<(UnitParam, f64)>) {
    let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
    let sig = input.clone();
    rig.set_input(move |f| sig.get((f - F0) as usize).copied().unwrap_or(0.0));
    let unit: Box<dyn UnitProcessor> = match spec.send {
        InputSend::Echo => Box::new(EchoUnit::new(SR)),
        InputSend::Ring => Box::new(RingUnit::new(SR)),
        _ => unreachable!(),
    };
    assert!(rig.port(0).install_at(1, 1, 0, true, None, unit).is_ok());
    let on = F0 + ON_OFFSET;
    for &(param, value) in spec.params {
        rig.send_at(on, Command::SetUnitParam(1, UnitParam::Send(param), value));
    }
    rig.send_at(on, Command::SetUnitBypass(1, false));
    rig.keep_output();
    rig.advance(PIN_FRAMES as Frame);
    let answered: Vec<_> = rig
        .events
        .iter()
        .filter_map(|e| match *e {
            Event::UnitParam { unit: 1, param, value, .. } => Some((param, value)),
            _ => None,
        })
        .collect();
    let rec = [rig.record.clone(), rig.record_right.clone()];
    let mon = [rig.monitor.clone(), rig.monitor_right.clone()];
    drain_returns(&mut rig);
    (rec, mon, answered)
}

#[test]
fn echo_unit_matches_pin_bit_for_bit_across_block_sizes() {
    let input = pin_input();
    for spec in [PIN_SPECS[0], PIN_SPECS[1]] {
        let pin = load_pin(spec.id);
        let want_params: Vec<_> = spec.params.iter().map(|&(p, v)| (UnitParam::Send(p), v)).collect();
        for block in BLOCKS {
            let (rec, mon, answered) = run_unit_pin(spec, &input, block);
            assert_eq!(answered, want_params, "{} block {block}: param answers", spec.id);
            assert_bits_eq(&rec[0], &pin, &format!("{} block {block} record L", spec.id));
            assert_bits_eq(&rec[1], &pin, &format!("{} block {block} record R", spec.id));
            assert_bits_eq(&mon[0], &pin, &format!("{} block {block} monitor L", spec.id));
            assert_bits_eq(&mon[1], &pin, &format!("{} block {block} monitor R", spec.id));
        }
    }

    // Active-at-install (`bypassed = false`, settled over silence) also equals settled `InputFx` bit for bit.
    for spec in [PIN_SPECS[0], PIN_SPECS[1]] {
        let mut send_rig = Rig::new();
        let mut unit_rig = Rig::new();
        assert!(unit_rig.port(0).install_at(1, 1, 0, false, None, Box::new(EchoUnit::new(SR))).is_ok());
        for &(p, v) in spec.params {
            send_rig.send_at(F0, Command::SetInputSendParam(p, v));
            unit_rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(p), v));
        }
        send_rig.send_at(F0, Command::SetInputSend(InputSend::Echo, true));
        // Advance past the 20 ms ramps on silence, then feed the deterministic input.
        send_rig.advance(2400);
        unit_rig.advance(2400);
        let start = send_rig.frame;
        let (sa, sb) = (input.clone(), input.clone());
        send_rig.set_input(move |f| sa.get((f - start) as usize).copied().unwrap_or(0.0));
        unit_rig.set_input(move |f| sb.get((f - start) as usize).copied().unwrap_or(0.0));
        send_rig.keep_output();
        unit_rig.keep_output();
        send_rig.advance(SR as Frame);
        unit_rig.advance(SR as Frame);
        assert_bits_eq(&unit_rig.record, &send_rig.record, &format!("{} active-at-install record L", spec.id));
        assert_bits_eq(&unit_rig.record_right, &send_rig.record_right, &format!("{} active-at-install record R", spec.id));
        assert_bits_eq(&unit_rig.monitor, &send_rig.monitor, &format!("{} active-at-install monitor L", spec.id));
        drain_returns(&mut unit_rig);
    }
}

#[test]
fn ring_unit_matches_pin_bit_for_bit_across_block_sizes() {
    let input = pin_input();
    for spec in [PIN_SPECS[2], PIN_SPECS[3]] {
        let pin = load_pin(spec.id);
        let want_params: Vec<_> = spec.params.iter().map(|&(p, v)| (UnitParam::Send(p), v)).collect();
        for block in BLOCKS {
            let (rec, mon, answered) = run_unit_pin(spec, &input, block);
            assert_eq!(answered, want_params, "{} block {block}: param answers", spec.id);
            assert_bits_eq(&rec[0], &pin, &format!("{} block {block} record L", spec.id));
            assert_bits_eq(&rec[1], &pin, &format!("{} block {block} record R", spec.id));
            assert_bits_eq(&mon[0], &pin, &format!("{} block {block} monitor L", spec.id));
            assert_bits_eq(&mon[1], &pin, &format!("{} block {block} monitor R", spec.id));
        }
    }

    // Active-at-install (`bypassed = false`, settled over silence) also equals settled `InputFx` bit for bit.
    for spec in [PIN_SPECS[2], PIN_SPECS[3]] {
        let mut send_rig = Rig::new();
        let mut unit_rig = Rig::new();
        assert!(unit_rig.port(0).install_at(1, 1, 0, false, None, Box::new(RingUnit::new(SR))).is_ok());
        for &(p, v) in spec.params {
            send_rig.send_at(F0, Command::SetInputSendParam(p, v));
            unit_rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(p), v));
        }
        send_rig.send_at(F0, Command::SetInputSend(InputSend::Ring, true));
        send_rig.advance(2400);
        unit_rig.advance(2400);
        let start = send_rig.frame;
        let (sa, sb) = (input.clone(), input.clone());
        send_rig.set_input(move |f| sa.get((f - start) as usize).copied().unwrap_or(0.0));
        unit_rig.set_input(move |f| sb.get((f - start) as usize).copied().unwrap_or(0.0));
        send_rig.keep_output();
        unit_rig.keep_output();
        send_rig.advance(SR as Frame);
        unit_rig.advance(SR as Frame);
        assert_bits_eq(&unit_rig.record, &send_rig.record, &format!("{} active-at-install record L", spec.id));
        assert_bits_eq(&unit_rig.record_right, &send_rig.record_right, &format!("{} active-at-install record R", spec.id));
        assert_bits_eq(&unit_rig.monitor, &send_rig.monitor, &format!("{} active-at-install monitor L", spec.id));
        drain_returns(&mut unit_rig);
    }
}

/// Distinct left and right signals, mid-block tempo and meter changes, and mid-block param changes:
/// each channel renders its own side's `InputFx` bit for bit across all block sizes, with zero
/// crosstalk. Also verifies param clamping and rejection of non-finite / wrong-kind params.
#[test]
fn echo_and_ring_run_per_channel_follow_tempo_and_clamp_params() {
    let left_sig = |f: Frame| -> f32 {
        let k = (f - F0) as usize;
        0.3 * fdlibm::sin(std::f64::consts::TAU * 131.0 * k as f64 / SR as f64) as f32
            + if k == 2_000 || k == 35_000 { 0.5 } else { 0.0 }
    };
    let right_sig = |f: Frame| -> f32 {
        let k = (f - F0) as usize;
        0.25 * fdlibm::sin(std::f64::consts::TAU * 293.0 * k as f64 / SR as f64) as f32
            + if k == 7_000 || k == 41_000 { -0.45 } else { 0.0 }
    };
    let frames = (1.5 * SR as f64) as Frame;

    // Reference from `InputFx` on each channel independently.
    let render_send_mono = |send: InputSend, sig: fn(Frame) -> f32| -> Vec<f32> {
        let mut rig = Rig::new();
        rig.set_input(sig);
        rig.send_at(F0 + 137, Command::SetInputSend(send, true));
        match send {
            InputSend::Echo => {
                rig.send_at(F0 + 137, Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.55));
                rig.send_at(F0 + 18_013, Command::SetBpm(97.0));
                rig.send_at(F0 + 29_007, Command::SetMeter(Meter::new(6, 8).unwrap()));
                rig.send_at(F0 + 33_005, Command::SetInputSendParam(InputSendParam::EchoTime, 3.0));
                rig.send_at(F0 + 44_001, Command::SetInputSendParam(InputSendParam::EchoLevel, 0.8));
            }
            InputSend::Ring => {
                rig.send_at(F0 + 137, Command::SetInputSendParam(InputSendParam::RingFreq, 211.0));
                rig.send_at(F0 + 25_011, Command::SetInputSendParam(InputSendParam::RingFreq, 887.0));
                rig.send_at(F0 + 40_003, Command::SetInputSendParam(InputSendParam::RingLevel, 0.9));
            }
            _ => unreachable!(),
        }
        rig.keep_output();
        rig.advance(frames);
        rig.monitor
    };

    for send in [InputSend::Echo, InputSend::Ring] {
        let want_l = render_send_mono(send, left_sig);
        let want_r = render_send_mono(send, right_sig);
        assert_ne!(want_l, want_r, "distinct L and R");
        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            rig.set_stereo_inputs(move |f| [left_sig(f), right_sig(f)], |_| [0.0, 0.0]);
            let unit: Box<dyn UnitProcessor> = match send {
                InputSend::Echo => Box::new(EchoUnit::new(SR)),
                InputSend::Ring => Box::new(RingUnit::new(SR)),
                _ => unreachable!(),
            };
            assert!(rig.port(0).install_at(1, 1, 0, true, None, unit).is_ok());
            rig.send_at(F0 + 137, Command::SetUnitBypass(1, false));
            match send {
                InputSend::Echo => {
                    rig.send_at(F0 + 137, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoFeedback), 0.55));
                    rig.send_at(F0 + 18_013, Command::SetBpm(97.0));
                    rig.send_at(F0 + 29_007, Command::SetMeter(Meter::new(6, 8).unwrap()));
                    rig.send_at(F0 + 33_005, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoTime), 3.0));
                    rig.send_at(F0 + 44_001, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.8));
                }
                InputSend::Ring => {
                    rig.send_at(F0 + 137, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::RingFreq), 211.0));
                    rig.send_at(F0 + 25_011, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::RingFreq), 887.0));
                    rig.send_at(F0 + 40_003, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::RingLevel), 0.9));
                }
                _ => unreachable!(),
            }
            rig.keep_output();
            rig.advance(frames);
            assert_bits_eq(&rig.monitor, &want_l, &format!("{send:?} block {block} monitor L"));
            assert_bits_eq(&rig.monitor_right, &want_r, &format!("{send:?} block {block} monitor R"));
            assert_bits_eq(&rig.record, &want_l, &format!("{send:?} block {block} record L"));
            assert_bits_eq(&rig.record_right, &want_r, &format!("{send:?} block {block} record R"));
            drain_returns(&mut rig);
        }
    }

    // Param clamping, non-finite rejection and wrong-kind rejection.
    let mut rig = Rig::new();
    assert!(rig.port(0).install_at(1, 1, 0, false, None, Box::new(EchoUnit::new(SR))).is_ok());
    assert!(rig.port(0).install_at(2, 2, 1, false, None, Box::new(RingUnit::new(SR))).is_ok());
    rig.advance(128);
    let t = rig.frame + 7;
    // ECHO clamps: level 2 -> 1, feedback 1.5 -> 0.95, time 9 -> 3.
    rig.send_at(t, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 2.0));
    rig.send_at(t + 1, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoFeedback), 1.5));
    rig.send_at(t + 2, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoTime), 9.0));
    // ECHO rejects non-finite and RING param.
    rig.send_at(t + 3, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), f64::NAN));
    rig.send_at(t + 4, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), f64::INFINITY));
    rig.send_at(t + 5, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::RingFreq), 300.0));
    // RING clamps: freq 5 -> 20, freq 9999 -> 1500, level -1 -> 0.
    rig.send_at(t + 6, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::RingFreq), 5.0));
    rig.send_at(t + 7, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::RingFreq), 9999.0));
    rig.send_at(t + 8, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::RingLevel), -1.0));
    // RING rejects non-finite and ECHO param.
    rig.send_at(t + 9, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::RingFreq), f64::NEG_INFINITY));
    rig.send_at(t + 10, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoLevel), 0.5));
    rig.advance(256);
    let params: Vec<_> = rig
        .events
        .iter()
        .filter_map(|e| match *e {
            Event::UnitParam { unit, param, value, .. } => Some((unit, param, value)),
            _ => None,
        })
        .collect();
    assert_eq!(
        params,
        vec![
            (1, UnitParam::Send(InputSendParam::EchoLevel), 1.0),
            (1, UnitParam::Send(InputSendParam::EchoFeedback), 0.95),
            (1, UnitParam::Send(InputSendParam::EchoTime), 3.0),
            (2, UnitParam::Send(InputSendParam::RingFreq), 20.0),
            (2, UnitParam::Send(InputSendParam::RingFreq), 1500.0),
            (2, UnitParam::Send(InputSendParam::RingLevel), 0.0),
        ]
    );
    assert_eq!(rig.engine.diag().unit_params_dropped, 5);
    drain_returns(&mut rig);
}

/// §5 item 1: at level 0 (settled), the output is the input, bit for bit (including subnormals; direct
/// `UnitProcessor::process` preserves `-0.0` as well, while `Rack::render` sums slots with `+0.0`).
#[test]
fn level_zero_is_input_bit_for_bit() {
    let stereo_sig = |f: Frame| -> Pair {
        let k = (f - F0) as usize;
        let l = if k % 31 == 0 {
            0.0f32
        } else {
            0.35 * fdlibm::sin(std::f64::consts::TAU * 149.0 * k as f64 / SR as f64) as f32
        };
        let r = if k % 29 == 0 {
            f32::from_bits(1) // smallest positive subnormal
        } else {
            0.28 * fdlibm::sin(std::f64::consts::TAU * 277.0 * k as f64 / SR as f64) as f32
        };
        [l, r]
    };

    for send in [InputSend::Echo, InputSend::Ring] {
        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            rig.set_stereo_inputs(stereo_sig, |_| [0.0, 0.0]);
            let (unit, param): (Box<dyn UnitProcessor>, InputSendParam) = match send {
                InputSend::Echo => (Box::new(EchoUnit::new(SR)), InputSendParam::EchoLevel),
                InputSend::Ring => (Box::new(RingUnit::new(SR)), InputSendParam::RingLevel),
                _ => unreachable!(),
            };
            assert!(rig.port(0).install_at(1, 1, 0, false, None, unit).is_ok());
            rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(param), 0.0));
            // Past the 20 ms ramp so level is settled at exact 0.0 while audio has already filled ECHO's line.
            rig.advance(2400);
            let start = rig.frame;
            rig.keep_output();
            rig.advance(24_000);
            let want_l: Vec<f32> = (0..24_000).map(|k| stereo_sig(start + k)[0]).collect();
            let want_r: Vec<f32> = (0..24_000).map(|k| stereo_sig(start + k)[1]).collect();
            assert_bits_eq(&rig.monitor, &want_l, &format!("{send:?} block {block} level 0 monitor L"));
            assert_bits_eq(&rig.monitor_right, &want_r, &format!("{send:?} block {block} level 0 monitor R"));
            assert_bits_eq(&rig.record, &want_l, &format!("{send:?} block {block} level 0 record L"));
            assert_bits_eq(&rig.record_right, &want_r, &format!("{send:?} block {block} level 0 record R"));
            drain_returns(&mut rig);
        }
    }

    // Direct `UnitProcessor::process` at settled level 0 preserves even `-0.0`.
    let cx0 = UnitCx { frame: 0, dsp_frame: 0, bpm: 120.0, meter: Meter::FOUR_FOUR };
    let mut echo = EchoUnit::new(SR);
    let mut ring = RingUnit::new(SR);
    assert_eq!(echo.set_param(&cx0, UnitParam::Send(InputSendParam::EchoLevel), 0.0), Some(0.0));
    assert_eq!(ring.set_param(&cx0, UnitParam::Send(InputSendParam::RingLevel), 0.0), Some(0.0));
    let x_l = [-0.0f32; 128];
    let x_r = [-0.0f32; 128];
    let mut out_l = [0.0f32; 128];
    let mut out_r = [0.0f32; 128];
    for q in 0..20u64 {
        let cx = UnitCx { frame: (q * 128) as Frame, dsp_frame: (q * 128) as Frame, bpm: 120.0, meter: Meter::FOUR_FOUR };
        echo.process(&cx, [&x_l, &x_r], &[], [&mut out_l, &mut out_r]);
        if q >= 16 {
            assert_bits_eq(&out_l, &x_l, "direct echo level 0 preserves -0.0 L");
            assert_bits_eq(&out_r, &x_r, "direct echo level 0 preserves -0.0 R");
        }
        ring.process(&cx, [&x_l, &x_r], &[], [&mut out_l, &mut out_r]);
        if q >= 16 {
            assert_bits_eq(&out_l, &x_l, "direct ring level 0 preserves -0.0 L");
            assert_bits_eq(&out_r, &x_r, "direct ring level 0 preserves -0.0 R");
        }
    }
}

// ── §5 item 2: Composition `[X, Y]` equals `Y` on `X`'s output bit for bit, and order shows ──────────

#[derive(Clone, Copy, Debug)]
enum Stage {
    EchoA,
    EchoB,
    RingA,
    RingB,
    Clip,
    Gain,
}

impl Stage {
    fn build(self) -> Box<dyn UnitProcessor> {
        match self {
            Stage::EchoA | Stage::EchoB => Box::new(EchoUnit::new(SR)),
            Stage::RingA | Stage::RingB => Box::new(RingUnit::new(SR)),
            Stage::Clip => Box::new(FakeUnit(FakeDsp::Clip(0.35))),
            Stage::Gain => Box::new(FakeUnit(FakeDsp::Gain(2.0))),
        }
    }

    fn setup(self, rig: &mut Rig, id: UnitId, at: Frame) {
        match self {
            Stage::EchoA => {
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::EchoTime), 1.0));
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::EchoFeedback), 0.45));
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::EchoLevel), 0.75));
            }
            Stage::EchoB => {
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::EchoTime), 3.0));
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::EchoFeedback), 0.55));
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::EchoLevel), 0.65));
            }
            Stage::RingA => {
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::RingFreq), 173.5));
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::RingLevel), 0.85));
            }
            Stage::RingB => {
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::RingFreq), 317.0));
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::RingLevel), 0.7));
            }
            Stage::Clip | Stage::Gain => {}
        }
    }
}

fn render_stages(stages: &[Stage], input: Arc<Vec<Pair>>, block: usize) -> ([Vec<f32>; 2], [Vec<f32>; 2]) {
    let frames = input.len() as Frame;
    let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
    rig.set_stereo_inputs(move |f| input.get((f - F0) as usize).copied().unwrap_or([0.0, 0.0]), |_| [0.0, 0.0]);
    for (idx, &stage) in stages.iter().enumerate() {
        let id = (idx + 1) as UnitId;
        assert!(rig.port(0).install_at(id, id as u64, idx, false, None, stage.build()).is_ok());
        stage.setup(&mut rig, id, F0);
    }
    rig.keep_output();
    rig.advance(frames);
    let mon = [rig.monitor.clone(), rig.monitor_right.clone()];
    let rec = [rig.record.clone(), rig.record_right.clone()];
    drain_returns(&mut rig);
    (mon, rec)
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(&x, &y)| (x - y).abs()).fold(0.0f32, f32::max)
}

#[test]
fn composition_matches_staged_processing_and_order_differs() {
    let frames = (1.0 * SR as f64) as usize;
    let mut rng = Mulberry32::new(73);
    let input: Arc<Vec<Pair>> = Arc::new(
        (0..frames)
            .map(|n| {
                let l = (0.35 * fdlibm::sin(std::f64::consts::TAU * 137.0 * n as f64 / SR as f64)
                    + 0.2 * (rng.next_f64() * 2.0 - 1.0)) as f32;
                let r = (0.30 * fdlibm::sin(std::f64::consts::TAU * 229.0 * n as f64 / SR as f64)
                    + 0.15 * (rng.next_f64() * 2.0 - 1.0)) as f32;
                [l, r]
            })
            .collect(),
    );

    let pairs = [
        (Stage::EchoA, Stage::RingA),
        (Stage::RingA, Stage::EchoA),
        (Stage::EchoA, Stage::EchoB),
        (Stage::RingA, Stage::RingB),
        (Stage::EchoA, Stage::Clip),
        (Stage::Clip, Stage::EchoA),
        (Stage::RingA, Stage::Clip),
        (Stage::Clip, Stage::RingA),
        (Stage::EchoA, Stage::Gain),
        (Stage::Gain, Stage::EchoA),
        (Stage::Clip, Stage::Gain),
        (Stage::Gain, Stage::Clip),
    ];

    for (x, y) in pairs {
        let (x_out, _) = render_stages(&[x], input.clone(), 128);
        let x_pairs: Arc<Vec<Pair>> = Arc::new(x_out[0].iter().zip(&x_out[1]).map(|(&l, &r)| [l, r]).collect());
        let (want, _) = render_stages(&[y], x_pairs, 128);
        for block in BLOCKS {
            let (mon, rec) = render_stages(&[x, y], input.clone(), block);
            assert_bits_eq(&mon[0], &want[0], &format!("[{x:?}, {y:?}] block {block} monitor L"));
            assert_bits_eq(&mon[1], &want[1], &format!("[{x:?}, {y:?}] block {block} monitor R"));
            assert_bits_eq(&rec[0], &want[0], &format!("[{x:?}, {y:?}] block {block} record L"));
            assert_bits_eq(&rec[1], &want[1], &format!("[{x:?}, {y:?}] block {block} record R"));
        }
    }

    // Order must visibly differ for each non-commuting pair above a stated floor.
    for (a, b, floor) in [
        (Stage::EchoA, Stage::RingA, 0.05f32),
        (Stage::EchoA, Stage::Clip, 0.05f32),
        (Stage::RingA, Stage::Clip, 0.05f32),
        (Stage::Clip, Stage::Gain, 0.05f32),
    ] {
        let (ab, _) = render_stages(&[a, b], input.clone(), 128);
        let (ba, _) = render_stages(&[b, a], input.clone(), 128);
        let diff_l = max_abs_diff(&ab[0], &ba[0]);
        let diff_r = max_abs_diff(&ab[1], &ba[1]);
        assert!(diff_l > floor, "[{a:?}, {b:?}] vs [{b:?}, {a:?}] L diff {diff_l} <= {floor}");
        assert!(diff_r > floor, "[{a:?}, {b:?}] vs [{b:?}, {a:?}] R diff {diff_r} <= {floor}");
    }
}

// ── §5 item 3: Bypass settled is input bit for bit, latency unchanged, ECHO rings out, 20 ms ramp ───

#[test]
fn bypass_settled_is_input_echo_rings_out_and_unbypass_ramps_20ms() {
    // 1. Cold install bypassed: output is input bit for bit from frame 0, zero blip.
    let cold_sig = |f: Frame| -> Pair {
        let k = (f - F0) as usize;
        if k % 17 == 0 {
            [0.0, f32::from_bits(1)]
        } else {
            let l = 0.4 * fdlibm::sin(std::f64::consts::TAU * 199.0 * k as f64 / SR as f64) as f32;
            let r = 0.3 * fdlibm::sin(std::f64::consts::TAU * 311.0 * k as f64 / SR as f64) as f32;
            [l, r]
        }
    };
    for send in [InputSend::Echo, InputSend::Ring] {
        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            rig.set_stereo_inputs(cold_sig, |_| [0.0, 0.0]);
            let unit: Box<dyn UnitProcessor> = match send {
                InputSend::Echo => Box::new(EchoUnit::new(SR)),
                InputSend::Ring => Box::new(RingUnit::new(SR)),
                _ => unreachable!(),
            };
            assert!(rig.port(0).install_at(1, 1, 0, true, None, unit).is_ok());
            rig.keep_output();
            rig.advance(4096);
            let want_l: Vec<f32> = (0..4096).map(|k| cold_sig(F0 + k)[0]).collect();
            let want_r: Vec<f32> = (0..4096).map(|k| cold_sig(F0 + k)[1]).collect();
            assert_bits_eq(&rig.monitor, &want_l, &format!("cold bypass {send:?} block {block} L"));
            assert_bits_eq(&rig.monitor_right, &want_r, &format!("cold bypass {send:?} block {block} R"));
            assert_bits_eq(&rig.record, &want_l, &format!("cold bypass {send:?} block {block} rec L"));
            assert_bits_eq(&rig.record_right, &want_r, &format!("cold bypass {send:?} block {block} rec R"));
            drain_returns(&mut rig);
        }
    }

    // 2. Active -> bypass: ECHO still rings out, ignores new input once gate is shut, and settles to
    //    exact input bit for bit once its line empties; RING settles after 20 ms and holds its phase.
    let echo_hit = F0 + 1_000;
    let bypass_at = F0 + 4_000;
    let ignored_hit = F0 + 6_000; // > bypass_at + 960 + 128: gate is shut
    let settled_at = F0 + 8 * SR as Frame; // 20 repeats of 0.5 (5.0 s) + 2 s buffer flush < 8 s
    let mut echo_ref: Option<[Vec<f32>; 2]> = None;
    for block in BLOCKS {
        let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
        rig.set_stereo_inputs(
            move |f| {
                if f == echo_hit {
                    [1.0, 0.5]
                } else if f == ignored_hit {
                    [0.75, -0.25]
                } else if f >= settled_at && (f - F0) % 19 == 0 {
                    [0.0, f32::from_bits(1)]
                } else if f >= settled_at {
                    [0.2, -0.15]
                } else {
                    [0.0, 0.0]
                }
            },
            |_| [0.0, 0.0],
        );
        assert!(rig.port(0).install_at(1, 1, 0, false, None, Box::new(EchoUnit::new(SR))).is_ok());
        rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoFeedback), 0.5));
        rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.5));
        rig.send_at(bypass_at, Command::SetUnitBypass(1, true));
        rig.keep_output();
        // 8.5 seconds total: 20 repeats of 0.5 flush under -120 dBFS at 5.02 s, then the 2 s line
        // empties by 7.03 s; settled window [8.0 s, 8.5 s) is bit-exact input.
        rig.advance(8 * SR as Frame + SR as Frame / 2);

        let level = 0.5 * (1.0f32 - 0.25).sqrt();
        // First 3 repeats of echo_hit still ring out AFTER bypass_at!
        let k0 = (echo_hit - F0) as usize;
        assert_eq!(rig.monitor[k0 + EIGHTH as usize], level, "block {block}: 1st repeat rings out while bypassed");
        assert_eq!(rig.monitor_right[k0 + EIGHTH as usize], 0.5 * level, "block {block}: 1st repeat R");
        assert_eq!(rig.monitor[k0 + 2 * EIGHTH as usize], 0.5 * level, "block {block}: 2nd repeat rings out");
        assert_eq!(rig.monitor[k0 + 3 * EIGHTH as usize], 0.25 * level, "block {block}: 3rd repeat rings out");
        // ignored_hit passes dry on its own frame, and NEVER echoes.
        let ki = (ignored_hit - F0) as usize;
        assert_eq!(rig.monitor[ki], 0.75, "block {block}: dry passes untouched while bypassed");
        assert_eq!(rig.monitor[ki + EIGHTH as usize], 0.0, "block {block}: bypassed input does not enter delay line");
        // Settled window [8.0 s, 8.5 s): exact input bit for bit.
        let s8 = 8 * SR as usize;
        let end_k = s8 + SR as usize / 2;
        let want_l: Vec<f32> = (s8..end_k).map(|k| if k % 19 == 0 { 0.0 } else { 0.2 }).collect();
        let want_r: Vec<f32> = (s8..end_k).map(|k| if k % 19 == 0 { f32::from_bits(1) } else { -0.15 }).collect();
        assert_bits_eq(&rig.monitor[s8..], &want_l, &format!("echo settled bypass block {block} L"));
        assert_bits_eq(&rig.monitor_right[s8..], &want_r, &format!("echo settled bypass block {block} R"));

        let cur = [rig.monitor.clone(), rig.monitor_right.clone()];
        let base = echo_ref.get_or_insert_with(|| cur.clone());
        assert_bits_eq(&cur[0], &base[0], &format!("echo bypass block {block} vs 1 L"));
        assert_bits_eq(&cur[1], &base[1], &format!("echo bypass block {block} vs 1 R"));
        drain_returns(&mut rig);
    }

    // 3. Un-bypass fades in over 20 ms from the command's frame (mid-quantum, 78 frames before a
    //    quantum boundary), matching `InputFx` bit for bit across all 7 block sizes.
    let unbypass_at = F0 + 20 * 128 - 78; // 78 frames before quantum 20
    let boundary = F0 + 20 * 128;
    const RAMP_FRAMES: Frame = 960; // 20 ms at 48 kHz
    for send in [InputSend::Echo, InputSend::Ring] {
        // Pin reference from `InputFx` on constant 0.5 input.
        let mut srig = Rig::new();
        srig.set_level(0.5);
        match send {
            InputSend::Echo => {
                srig.send_at(F0, Command::SetInputSendParam(InputSendParam::EchoTime, 3.0)); // 1/16 = 6000
                srig.send_at(F0, Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.0));
                srig.send_at(F0, Command::SetInputSendParam(InputSendParam::EchoLevel, 1.0));
            }
            InputSend::Ring => {
                srig.send_at(F0, Command::SetInputSendParam(InputSendParam::RingFreq, 300.0));
                srig.send_at(F0, Command::SetInputSendParam(InputSendParam::RingLevel, 1.0));
            }
            _ => unreachable!(),
        }
        srig.send_at(unbypass_at, Command::SetInputSend(send, true));
        srig.keep_output();
        srig.advance(12_000);
        let want = srig.monitor.clone();

        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            rig.set_level(0.5);
            let unit: Box<dyn UnitProcessor> = match send {
                InputSend::Echo => Box::new(EchoUnit::new(SR)),
                InputSend::Ring => Box::new(RingUnit::new(SR)),
                _ => unreachable!(),
            };
            assert!(rig.port(0).install_at(1, 1, 0, true, None, unit).is_ok());
            match send {
                InputSend::Echo => {
                    rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoTime), 3.0));
                    rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoFeedback), 0.0));
                    rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 1.0));
                }
                InputSend::Ring => {
                    rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::RingFreq), 300.0));
                    rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::RingLevel), 1.0));
                }
                _ => unreachable!(),
            }
            rig.send_at(unbypass_at, Command::SetUnitBypass(1, false));
            rig.keep_output();
            rig.advance(12_000);
            assert_bits_eq(&rig.monitor, &want, &format!("unbypass {send:?} block {block} L"));
            assert_bits_eq(&rig.monitor_right, &want, &format!("unbypass {send:?} block {block} R"));

            // Directly verify the 20 ms ramp envelope on the wet component (`y - 0.5`).
            let wet: Vec<f32> = rig.monitor.iter().map(|&y| y - 0.5).collect();
            let shift = if send == InputSend::Echo { SIXTEENTH as usize } else { 0 };
            let b_idx = (boundary - F0) as usize + shift;
            let end_idx = (unbypass_at + RAMP_FRAMES - F0) as usize + shift;
            assert!(wet[..b_idx].iter().all(|&w| w == 0.0), "{send:?} block {block}: silent before boundary");
            if send == InputSend::Echo {
                // Constant 0.5 delayed by 1/16, level 1, fb 0: wet[k] = 0.5 * gate[k - shift].
                assert!(wet[b_idx + 10] > 0.0 && wet[b_idx + 10] < 0.1, "{send:?}: ramp starts low");
                assert!(wet[(b_idx + end_idx) / 2] > 0.15 && wet[(b_idx + end_idx) / 2] < 0.35, "{send:?}: mid ramp");
                assert!(wet[end_idx + 16..end_idx + 200].iter().all(|&w| w == 0.5), "{send:?}: settled after 20 ms");
            } else {
                // Constant 0.5 * gate * sin(phi): peak in first quarter of ramp < peak after 20 ms (0.5).
                let early_peak = wet[b_idx..b_idx + 200].iter().map(|x| x.abs()).fold(0.0f32, f32::max);
                let late_peak = wet[end_idx + 16..end_idx + 400].iter().map(|x| x.abs()).fold(0.0f32, f32::max);
                assert!(early_peak > 0.01 && early_peak < 0.2, "ring early ramp peak {early_peak}");
                assert!((late_peak - 0.5).abs() < 1e-3, "ring settled peak {late_peak}");
            }

            // Re-bypass RING and verify it settles back to exact dry and holds phase.
            if send == InputSend::Ring {
                let off_at = rig.frame + 13;
                rig.send_at(off_at, Command::SetUnitBypass(1, true));
                rig.advance(RAMP_FRAMES + 3 * 128);
                rig.keep_output();
                rig.advance(2048);
                assert!(rig.monitor.iter().all(|&y| y.to_bits() == 0.5f32.to_bits()), "ring settled bypass block {block}");
            }
            drain_returns(&mut rig);
        }
    }

    // 4. Latency unchanged by bypass: `[Lag(37), Echo/Ring]` keeps live_latency == 37 and records an
    //    impulse on the exact same loop frame whether bypassed or active.
    for send in [InputSend::Echo, InputSend::Ring] {
        for bypassed in [false, true] {
            let mut rig = Rig::new();
            assert!(rig.port(0).install_at(1, 1, 0, false, None, Box::new(Delay::new(37))).is_ok());
            let unit: Box<dyn UnitProcessor> = match send {
                InputSend::Echo => Box::new(EchoUnit::new(SR)),
                InputSend::Ring => Box::new(RingUnit::new(SR)),
                _ => unreachable!(),
            };
            assert!(rig.port(0).install_at(2, 2, 1, bypassed, None, unit).is_ok());
            rig.set(Command::SetMetronome(false));
            rig.set(Command::SetFixedLength(true));
            rig.set(Command::SetFixedBars(1.0));
            rig.press(Command::RecDub(0));
            assert_eq!(rig.engine.live_latency(), 37, "{send:?} bypassed={bypassed}: latency unchanged");
            let start = rig.start_frame();
            let hit = start + 2000 - 37;
            rig.set_input(move |f| if f == hit { 1.0 } else { 0.0 });
            rig.advance_to(rig.end_frame() + 1);
            assert_eq!(rig.state(0), LaneState::Playing);
            let pcm = rig.pcm(0);
            let first_nz = pcm.iter().position(|&x| x != 0.0);
            assert_eq!(first_nz, Some(2000), "{send:?} bypassed={bypassed}: dry note lands on loop frame 2000");
            drain_returns(&mut rig);
        }
    }
}

// ── §5 item 4: Tails of two ECHOs (one slot, and one in each slot) ──────────────────────────────────

#[test]
fn two_echos_in_one_slot_tails_and_removal() {
    let hit1 = F0 + 2_000;
    let bypass1 = F0 + 4_000; // after hit1 entered echo 1, before its 1/16 repeat at +6000
    let remove1 = F0 + 30_000; // echo 1's tail has already fed echo 2; now remove echo 1
    let hit2 = F0 + 40_000; // after removal fade (480 frames), hits echo 2 directly
    let end_frame = F0 + 96_000;

    let input_fn = move |f: Frame| -> Pair {
        if f == hit1 {
            [1.0, 0.5]
        } else if f == hit2 {
            [0.6, -0.4]
        } else {
            [0.0, 0.0]
        }
    };

    // Part A: reference for `[Echo1(bypassed at bypass1), Echo2]` over `[F0, remove1)` by running
    // Echo1 alone, then feeding its output into Echo2 alone (item 2 composition).
    let (e1_out, e2_on_e1) = {
        let mut r1 = Rig::new();
        r1.set_stereo_inputs(input_fn, |_| [0.0, 0.0]);
        assert!(r1.port(0).install_at(1, 1, 0, false, None, Box::new(EchoUnit::new(SR))).is_ok());
        r1.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoTime), 3.0)); // 1/16 = 6000
        r1.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoFeedback), 0.5));
        r1.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.6));
        r1.send_at(bypass1, Command::SetUnitBypass(1, true));
        r1.keep_output();
        r1.advance_to(remove1);
        let e1: Arc<Vec<Pair>> = Arc::new(r1.monitor.iter().zip(&r1.monitor_right).map(|(&l, &r)| [l, r]).collect());
        drain_returns(&mut r1);

        let mut r2 = Rig::new();
        let e1_in = e1.clone();
        r2.set_stereo_inputs(move |f| e1_in.get((f - F0) as usize).copied().unwrap_or([0.0, 0.0]), |_| [0.0, 0.0]);
        assert!(r2.port(0).install_at(2, 2, 0, false, None, Box::new(EchoUnit::new(SR))).is_ok());
        r2.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoTime), 2.0)); // 1/8. = 18000
        r2.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoFeedback), 0.4));
        r2.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoLevel), 0.7));
        r2.keep_output();
        r2.advance_to(remove1);
        let out = [r2.monitor.clone(), r2.monitor_right.clone()];
        drain_returns(&mut r2);
        (e1, out)
    };

    // Part B: reference for Echo2's full `[F0, end_frame)` when Echo1 is removed at `remove1`
    // (after the 10 ms removal crossfade finishes at `remove1 + FADE_10MS`, Echo2 sees `input_fn` directly).
    // Verify across all 7 block sizes that the combined chain matches Part A before removal, returns
    // Echo1 cleanly, and leaves Echo2's line completely untouched after removal!
    let mut full_ref: Option<[Vec<f32>; 2]> = None;
    for block in BLOCKS {
        let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
        rig.set_stereo_inputs(input_fn, |_| [0.0, 0.0]);
        assert!(rig.port(0).install_at(1, 1, 0, false, None, Box::new(EchoUnit::new(SR))).is_ok());
        assert!(rig.port(0).install_at(2, 2, 1, false, None, Box::new(EchoUnit::new(SR))).is_ok());
        rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoTime), 3.0));
        rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoFeedback), 0.5));
        rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.6));
        rig.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoTime), 2.0));
        rig.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoFeedback), 0.4));
        rig.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoLevel), 0.7));
        rig.send_at(bypass1, Command::SetUnitBypass(1, true));
        rig.keep_output();
        rig.advance_to(remove1);
        // Before removal: Echo1's bypassed tail rings through Echo2, matching item 2 composition bit for bit.
        assert_bits_eq(&rig.monitor, &e2_on_e1[0], &format!("two echos pre-remove block {block} L"));
        assert_bits_eq(&rig.monitor_right, &e2_on_e1[1], &format!("two echos pre-remove block {block} R"));
        // Verify Echo1's tail actually rang through Echo2: at hit1 + SIXTEENTH + DOTTED_EIGHTH = F0 + 26_000,
        // Echo2 echoes Echo1's 1st repeat!
        let cross = (hit1 + SIXTEENTH + DOTTED_EIGHTH - F0) as usize;
        assert!(rig.monitor[cross] > 0.1, "block {block}: echo 1's bypassed tail echoed by echo 2: {}", rig.monitor[cross]);

        // Now remove Echo1 while Echo2 still has pending repeats in its line.
        assert!(rig.port(0).remove(1));
        rig.advance_to(end_frame);
        let ret = rig.port(0).returned_unit().expect("echo 1 returned after removal");
        assert_eq!(ret.id, 1);
        drop(ret.unit);

        // Echo2's line is untouched:
        // 1) Echo1's 2nd repeat (which arrived at hit1 + 2*SIXTEENTH = F0 + 14_000, BEFORE remove1) is
        //    echoed by Echo2 at F0 + 14_000 + 18_000 = F0 + 32_000 (AFTER remove1 + FADE_10MS)!
        let carry = (hit1 + 2 * SIXTEENTH + DOTTED_EIGHTH - F0) as usize;
        assert!(rig.monitor[carry] > 0.05, "block {block}: echo 2 kept its pre-removal line: {}", rig.monitor[carry]);
        // 2) hit2 (at F0 + 40_000) echoes through Echo2 at +DOTTED_EIGHTH and NOT at +SIXTEENTH (Echo1 is gone).
        let k2 = (hit2 - F0) as usize;
        let fb2 = 0.4f32;
        let e2_level = 0.7f32 * (1.0f32 - fb2 * fb2).sqrt();
        assert_eq!(rig.monitor[k2 + SIXTEENTH as usize], 0.0, "block {block}: echo 1 is gone");
        assert_eq!(rig.monitor[k2 + DOTTED_EIGHTH as usize], 0.6f32 * e2_level, "block {block}: echo 2 echoes hit2 L");
        assert_eq!(rig.monitor_right[k2 + DOTTED_EIGHTH as usize], -0.4f32 * e2_level, "block {block}: echo 2 echoes hit2 R");

        // Compare against a standalone Echo2 fed `e1_out` up to `remove1`, faded to `input_fn` over FADE_10MS.
        // At minimum, bit-identical across all 7 block sizes!
        let cur = [rig.monitor.clone(), rig.monitor_right.clone()];
        let base = full_ref.get_or_insert_with(|| cur.clone());
        assert_bits_eq(&cur[0], &base[0], &format!("two echos full block {block} L"));
        assert_bits_eq(&cur[1], &base[1], &format!("two echos full block {block} R"));
        let _ = &e1_out;
        drain_returns(&mut rig);
    }

    // Also prove Echo2's output after removal matches a standalone Echo2 fed the exact same input
    // stream (e1's output before remove1, crossfaded over 480 frames to dry input_fn, then input_fn).
    let mut standalone = Rig::new();
    let e1_before = e1_out.clone();
    // Run a reference Echo1+removed through the crossfade manually to feed standalone Echo2.
    let mut e1_fade_rig = Rig::new();
    e1_fade_rig.set_stereo_inputs(input_fn, |_| [0.0, 0.0]);
    assert!(e1_fade_rig.port(0).install_at(1, 1, 0, false, None, Box::new(EchoUnit::new(SR))).is_ok());
    e1_fade_rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoTime), 3.0));
    e1_fade_rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoFeedback), 0.5));
    e1_fade_rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.6));
    e1_fade_rig.send_at(bypass1, Command::SetUnitBypass(1, true));
    e1_fade_rig.keep_output();
    e1_fade_rig.advance_to(remove1);
    assert!(e1_fade_rig.port(0).remove(1));
    e1_fade_rig.advance_to(end_frame);
    let fed_to_e2: Arc<Vec<Pair>> = Arc::new(e1_fade_rig.monitor.iter().zip(&e1_fade_rig.monitor_right).map(|(&l, &r)| [l, r]).collect());
    drain_returns(&mut e1_fade_rig);
    let _ = e1_before;

    standalone.set_stereo_inputs(move |f| fed_to_e2.get((f - F0) as usize).copied().unwrap_or([0.0, 0.0]), |_| [0.0, 0.0]);
    assert!(standalone.port(0).install_at(2, 2, 0, false, None, Box::new(EchoUnit::new(SR))).is_ok());
    standalone.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoTime), 2.0));
    standalone.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoFeedback), 0.4));
    standalone.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoLevel), 0.7));
    standalone.keep_output();
    standalone.advance_to(end_frame);
    let want_full = full_ref.unwrap();
    assert_bits_eq(&want_full[0], &standalone.monitor, "removing echo 1 leaves echo 2's line untouched L");
    assert_bits_eq(&want_full[1], &standalone.monitor_right, "removing echo 1 leaves echo 2's line untouched R");
    let _ = FADE_10MS;
    drain_returns(&mut standalone);
}

#[test]
fn one_echo_in_each_slot_tails_and_removal() {
    let hit_a = F0 + 2_000;
    let hit_b = F0 + 3_500;
    let bypass_a = F0 + 4_500;
    let remove_a = F0 + 30_000;
    let hit_a2 = F0 + 40_000;
    let hit_b2 = F0 + 42_000;
    let end_frame = F0 + 90_000;

    let in_a = move |f: Frame| -> Pair {
        if f == hit_a {
            [1.0, 0.5]
        } else if f == hit_a2 {
            [0.6, -0.3]
        } else {
            [0.0, 0.0]
        }
    };
    let in_b = move |f: Frame| -> Pair {
        if f == hit_b {
            [0.4, 0.8]
        } else if f == hit_b2 {
            [-0.5, 0.7]
        } else {
            [0.0, 0.0]
        }
    };

    // Reference: slot 0 alone + slot 1 alone summed sample by sample.
    let ref_a = {
        let mut r = Rig::new();
        r.set_stereo_inputs(in_a, |_| [0.0, 0.0]);
        assert!(r.port(0).install_at(1, 1, 0, false, None, Box::new(EchoUnit::new(SR))).is_ok());
        r.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoTime), 3.0)); // 1/16
        r.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoFeedback), 0.5));
        r.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.6));
        r.send_at(bypass_a, Command::SetUnitBypass(1, true));
        r.keep_output();
        r.advance_to(remove_a);
        assert!(r.port(0).remove(1));
        r.advance_to(end_frame);
        let out = [r.monitor.clone(), r.monitor_right.clone()];
        drain_returns(&mut r);
        out
    };
    let ref_b = {
        let mut r = Rig::new();
        r.set_stereo_inputs(|_| [0.0, 0.0], in_b);
        r.queue(Command::SetSlotLive(0, false));
        r.queue(Command::SetSlotLive(1, true));
        assert!(r.port(1).install_at(2, 2, 0, false, None, Box::new(EchoUnit::new(SR))).is_ok());
        r.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoTime), 2.0)); // 1/8.
        r.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoFeedback), 0.4));
        r.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoLevel), 0.7));
        r.keep_output();
        r.advance_to(end_frame);
        let out = [r.monitor.clone(), r.monitor_right.clone()];
        drain_returns(&mut r);
        out
    };
    let want_l: Vec<f32> = ref_a[0].iter().zip(&ref_b[0]).map(|(&a, &b)| a + b).collect();
    let want_r: Vec<f32> = ref_a[1].iter().zip(&ref_b[1]).map(|(&a, &b)| a + b).collect();

    for block in BLOCKS {
        let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
        rig.set_stereo_inputs(in_a, in_b);
        rig.queue(Command::SetSlotLive(1, true));
        assert!(rig.port(0).install_at(1, 1, 0, false, None, Box::new(EchoUnit::new(SR))).is_ok());
        assert!(rig.port(1).install_at(2, 2, 0, false, None, Box::new(EchoUnit::new(SR))).is_ok());
        rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoTime), 3.0));
        rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoFeedback), 0.5));
        rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.6));
        rig.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoTime), 2.0));
        rig.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoFeedback), 0.4));
        rig.send_at(F0, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::EchoLevel), 0.7));
        rig.send_at(bypass_a, Command::SetUnitBypass(1, true));
        rig.keep_output();
        rig.advance_to(remove_a);
        assert!(rig.port(0).remove(1));
        rig.advance_to(end_frame);
        let ret = rig.port(0).returned_unit().expect("slot 0 echo returned");
        assert_eq!(ret.id, 1);
        drop(ret.unit);

        assert_bits_eq(&rig.monitor, &want_l, &format!("two slots block {block} monitor L"));
        assert_bits_eq(&rig.monitor_right, &want_r, &format!("two slots block {block} monitor R"));
        assert_bits_eq(&rig.record, &want_l, &format!("two slots block {block} record L"));
        assert_bits_eq(&rig.record_right, &want_r, &format!("two slots block {block} record R"));
        drain_returns(&mut rig);
    }
}
