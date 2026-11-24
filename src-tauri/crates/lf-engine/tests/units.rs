//! Built-in effect units (`lf_engine::units`, `docs/unit-contract.md` §5 items 1–5: ECHO, RING
//! MOD, REVERB and OTT):
//! - Golden pins of today's `input_fx` sends stored under `tests/fixtures/sends/` in the same 58-byte
//!   IEEE float32 WAV + `manifest.json` layout as `tests/fixtures/tone/` (regenerable while
//!   `input_fx.rs` exists via `cargo test -p lf-engine --test units -- --ignored write_send_pins`),
//!   including REVERB's unsummed left and right convolver outputs (2-channel WAVs).
//! - §5 item 1: alone, each unit renders its pin (bit for bit per channel for ECHO and RING MOD;
//!   REVERB each channel against its own side within 1e-6 of full scale, failing on a swapped pair),
//!   follows tempo/meter (ECHO) and clamps params, and at level 0 outputs the input bit for bit.
//! - §5 item 2: composition `[X, Y]` equals `Y` run on `X`'s output bit for bit (ECHO, RING MOD,
//!   REVERB, OTT and E2's clip/gain fakes), and non-commuting pairs holding RING MOD, OTT or Clip
//!   differ past a stated floor.
//! - §5 item 3: settled bypass is the input bit for bit with latency unchanged; a bypassed ECHO or
//!   REVERB rings out; un-bypass fades in from the command's frame (20 ms for ECHO/RING/REVERB,
//!   10 ms linear crossfade for OTT).
//! - §5 item 4: two ECHOs at different times in one slot (and one in each slot): bypassing the first
//!   rings its tail through the second as item 2 composes it, and removing one leaves the other's line
//!   untouched.
//! - §5 item 5: `tests/ott.rs`'s cases run on `OttUnit`, and `OttUnit::take_meter`'s `BandMeter`
//!   matches `StereoOtt`'s own gains and linked followers sample by sample and over folded windows
//!   (direct `take_meter`; the engine's 4 ms meter ring arrives in a later E3 slice).
//!
//! Every render runs through `tests/common::Rig` or the local `assert_no_alloc` wrapper, bit-identical
//! across block sizes 1, 32, 64, 127, 128, 480 and 1024 (`docs/unit-contract.md` §9).

mod common;
#[allow(dead_code)]
#[path = "listen/pluck.rs"]
mod pluck;

use std::f64::consts::{FRAC_1_SQRT_2, LN_10, PI};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use common::refs::{sha256_f32, write_wav, Mulberry32};
use common::{Delay, Opts, Rig};
use lf_engine::dsp::fdlibm;
use lf_engine::dsp::ott::{
    release_scale, Setting, StereoOtt, CEILING, CEILING_KNEE, DEFAULT_DEPTH,
    FADE_SECONDS as OTT_FADE_SECONDS, HIGH_SPLIT, LOW_SPLIT, MAKEUP_DB,
};
use lf_engine::grid::{Frame, Meter};
use lf_engine::units::{default_reverb_ir, EchoUnit, OttUnit, ReverbUnit, RingUnit};
use lf_engine::{
    Action, BandMeter, Command, Event, InputSend, InputSendParam, LaneState, Pair, SlotEvent,
    UnitCx, UnitId, UnitInfo, UnitKind, UnitParam, UnitProcessor, BAND_LEVEL_FLOOR_DB,
};
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use serde::{Deserialize, Serialize};

const BLOCKS: [usize; 7] = [1, 32, 64, 127, 128, 480, 1024];
const SR: u32 = 48_000;

/// A direct unit call under `assert_no_alloc` that fails on an allocation: with `warn_debug` the crate
/// only counts violations, so the count must not move (as `Rig` checks its renders).
fn assert_no_alloc<T, F: FnOnce() -> T>(f: F) -> T {
    let before = common::violation_count();
    let out = ::assert_no_alloc::assert_no_alloc(f);
    assert_eq!(common::violation_count(), before, "a direct unit call allocated");
    out
}
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

const PIN_SPECS: [PinSpec; 6] = [
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
    PinSpec { id: "reverb-default-48000", send: InputSend::Reverb, params: &[] },
    PinSpec {
        id: "reverb-alt-48000",
        send: InputSend::Reverb,
        params: &[(InputSendParam::ReverbLevel, 0.85)],
    },
];

/// Cached standard 48 kHz reverb IR so tests do not regenerate Tone's noise tables per unit.
fn cached_reverb_ir() -> &'static [Vec<f32>; 2] {
    static IR: OnceLock<[Vec<f32>; 2]> = OnceLock::new();
    IR.get_or_init(|| default_reverb_ir(SR))
}

fn new_reverb_unit() -> ReverbUnit {
    let ir = cached_reverb_ir();
    ReverbUnit::with_ir(SR, [&ir[0], &ir[1]])
}

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

/// Render today's `input_fx` REVERB send at `block` frames per call and capture the convolver's
/// left and right wet outputs BEFORE they are summed (`dry + conv_l * level`, `dry + conv_r * level`).
/// Also checks that their mean equals today's summed record tap within 1e-6 on every frame.
fn render_reverb_stereo_pin(spec: PinSpec, input: &Arc<Vec<f32>>, block: usize) -> [Vec<f32>; 2] {
    assert_eq!(spec.send, InputSend::Reverb);
    let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
    rig.engine.enable_reverb_stereo_tap(block);
    let sig = input.clone();
    rig.set_input(move |f| sig.get((f - F0) as usize).copied().unwrap_or(0.0));
    let on = F0 + ON_OFFSET;
    for &(param, value) in spec.params {
        rig.send_at(on, Command::SetInputSendParam(param, value));
    }
    rig.send_at(on, Command::SetInputSend(InputSend::Reverb, true));
    rig.keep_output();
    let mut left = Vec::with_capacity(PIN_FRAMES);
    let mut right = Vec::with_capacity(PIN_FRAMES);
    let mut pos = 0usize;
    while pos < PIN_FRAMES {
        let n = block.min(PIN_FRAMES - pos);
        rig.engine.clear_reverb_stereo_tap();
        rig.advance(n as Frame);
        let [wl, wr] = rig.engine.reverb_stereo_tap();
        assert_eq!(wl.len(), n, "{}: stereo tap length at block {block}", spec.id);
        for k in 0..n {
            let dry = input[pos + k];
            left.push(if wl[k] == 0.0 { dry } else { dry + wl[k] });
            right.push(if wr[k] == 0.0 { dry } else { dry + wr[k] });
        }
        pos += n;
    }
    let folded: Vec<f32> = left.iter().zip(&right).map(|(&l, &r)| lf_engine::fold(l, r)).collect();
    let err = max_abs_diff(&folded, &rig.record);
    assert!(err < 1e-6, "{}: stereo pin block {block} mean vs mono send diff {err}", spec.id);
    [left, right]
}

fn stereo_pin_id(mono_id: &str) -> String {
    mono_id.replacen("-48000", "-stereo-48000", 1)
}

/// Regenerates `tests/fixtures/sends/` from today's `input_fx` sends (including REVERB's unsummed
/// left and right convolver outputs). Run explicitly when creating or refreshing pins before
/// `input_fx.rs` is removed:
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
        if spec.send == InputSend::Reverb {
            let sid = stereo_pin_id(spec.id);
            let [l, r] = render_reverb_stereo_pin(spec, &input, 128);
            let sfile = format!("{sid}.wav");
            write_wav(&dir.join(&sfile), SR, &[l.clone(), r.clone()]);
            scenarios.push(PinScenario {
                id: sid,
                send: spec.send.key().to_string(),
                channels: 2,
                file: sfile,
                sha256: sha256_f32(&[&l, &r]),
                params: spec.params.iter().map(|(p, v)| (p.key().to_string(), *v)).collect(),
            });
        }
    }
    let manifest = PinManifest {
        note: "Pinned renders of input_fx ECHO, RING MOD and REVERB (mono record/monitor and REVERB's unsummed stereo convolver L/R; slot 0 live and empty, master 1, 2 s at 48 kHz) before input_fx.rs is retired (docs/unit-contract.md §5 item 1). Float32 WAVs in encodeWav's 58-byte layout, matching tests/fixtures/tone/.".to_string(),
        rate: SR,
        frames: PIN_FRAMES,
        on_offset: ON_OFFSET,
        input_sha256: sha256_f32(&[&input]),
        scenarios,
    };
    let json = serde_json::to_string_pretty(&manifest).expect("serialize pin manifest");
    std::fs::write(dir.join("manifest.json"), format!("{json}\n")).expect("write pin manifest");
}

fn load_pin_channels(id: &str, expected_channels: usize) -> Vec<Vec<f32>> {
    let manifest = load_manifest();
    assert_eq!(manifest.rate, SR);
    assert_eq!(manifest.frames, PIN_FRAMES);
    assert_eq!(manifest.on_offset, ON_OFFSET);
    let input = pin_input();
    assert_eq!(sha256_f32(&[&input]), manifest.input_sha256, "pin input drifted");
    let s = manifest.scenarios.iter().find(|s| s.id == id).unwrap_or_else(|| panic!("missing pin scenario {id}"));
    assert_eq!(s.channels, expected_channels, "pin {id} channel count");
    let ch = read_wav(&pin_dir().join(&s.file), manifest.rate, s.channels, manifest.frames);
    let refs: Vec<&[f32]> = ch.iter().map(Vec::as_slice).collect();
    assert_eq!(sha256_f32(&refs), s.sha256, "pin wav {id} sha256 mismatch");
    ch
}

fn load_pin(id: &str) -> Vec<f32> {
    load_pin_channels(id, 1).into_iter().next().unwrap()
}

fn load_stereo_pin(id: &str) -> [Vec<f32>; 2] {
    let mut ch = load_pin_channels(id, 2);
    let r = ch.pop().unwrap();
    let l = ch.pop().unwrap();
    [l, r]
}

/// Today's `input_fx` sends still match the stored pins bit for bit (record tap and monitor, plus
/// REVERB's unsummed stereo convolver outputs) across all block sizes.
#[test]
fn sends_match_pinned_fixtures() {
    let input = pin_input();
    for spec in PIN_SPECS {
        let pin = load_pin(spec.id);
        for block in BLOCKS {
            let got = render_send_pin(spec, &input, block);
            assert_bits_eq(&got, &pin, &format!("{} block {block}", spec.id));
        }
        if spec.send == InputSend::Reverb {
            let sid = stereo_pin_id(spec.id);
            let [want_l, want_r] = load_stereo_pin(&sid);
            for block in BLOCKS {
                let [got_l, got_r] = render_reverb_stereo_pin(spec, &input, block);
                assert_bits_eq(&got_l, &want_l, &format!("{sid} block {block} L"));
                assert_bits_eq(&got_r, &want_r, &format!("{sid} block {block} R"));
            }
        }
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
    if spec.send == InputSend::Reverb {
        rig.set_stereo_inputs(
            move |f| {
                let x = sig.get((f - F0) as usize).copied().unwrap_or(0.0);
                [x, x]
            },
            |_| [0.0, 0.0],
        );
    } else {
        rig.set_input(move |f| sig.get((f - F0) as usize).copied().unwrap_or(0.0));
    }
    let unit: Box<dyn UnitProcessor> = match spec.send {
        InputSend::Echo => Box::new(EchoUnit::new(SR)),
        InputSend::Ring => Box::new(RingUnit::new(SR)),
        InputSend::Reverb => Box::new(new_reverb_unit()),
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

#[test]
fn reverb_unit_matches_pin_across_block_sizes() {
    let input = pin_input();
    for spec in [PIN_SPECS[4], PIN_SPECS[5]] {
        let mono_pin = load_pin(spec.id);
        let sid = stereo_pin_id(spec.id);
        let [pin_l, pin_r] = load_stereo_pin(&sid);
        let want_params: Vec<_> = spec.params.iter().map(|&(p, v)| (UnitParam::Send(p), v)).collect();
        let mut base_rec: Option<[Vec<f32>; 2]> = None;
        let mut base_mon: Option<[Vec<f32>; 2]> = None;
        for block in BLOCKS {
            let (rec, mon, answered) = run_unit_pin(spec, &input, block);
            assert_eq!(answered, want_params, "{} block {block}: param answers", spec.id);
            let err_rec_l = max_abs_diff(&rec[0], &pin_l);
            let err_rec_r = max_abs_diff(&rec[1], &pin_r);
            let err_mon_l = max_abs_diff(&mon[0], &pin_l);
            let err_mon_r = max_abs_diff(&mon[1], &pin_r);
            assert!(err_rec_l < 1e-6, "{} block {block} rec L diff {err_rec_l}", spec.id);
            assert!(err_rec_r < 1e-6, "{} block {block} rec R diff {err_rec_r}", spec.id);
            assert!(err_mon_l < 1e-6, "{} block {block} mon L diff {err_mon_l}", spec.id);
            assert!(err_mon_r < 1e-6, "{} block {block} mon R diff {err_mon_r}", spec.id);

            // A swapped pair (or mono mean) must fail decisively.
            let swap_l = max_abs_diff(&rec[0], &pin_r);
            let swap_r = max_abs_diff(&rec[1], &pin_l);
            assert!(swap_l > 0.01, "{} block {block}: swapped L vs R too close ({swap_l})", spec.id);
            assert!(swap_r > 0.01, "{} block {block}: swapped R vs L too close ({swap_r})", spec.id);

            // Folding L and R reproduces today's mono REVERB send pin within 1e-6.
            let folded: Vec<f32> = rec[0].iter().zip(&rec[1]).map(|(&l, &r)| lf_engine::fold(l, r)).collect();
            let err_mono = max_abs_diff(&folded, &mono_pin);
            assert!(err_mono < 1e-6, "{} block {block} mean vs mono pin diff {err_mono}", spec.id);

            // Bit-identical across all 7 block sizes.
            let br = base_rec.get_or_insert_with(|| rec.clone());
            let bm = base_mon.get_or_insert_with(|| mon.clone());
            assert_bits_eq(&rec[0], &br[0], &format!("{} block {block} rec L vs 1", spec.id));
            assert_bits_eq(&rec[1], &br[1], &format!("{} block {block} rec R vs 1", spec.id));
            assert_bits_eq(&mon[0], &bm[0], &format!("{} block {block} mon L vs 1", spec.id));
            assert_bits_eq(&mon[1], &bm[1], &format!("{} block {block} mon R vs 1", spec.id));
        }
    }

    // Active-at-install (`bypassed = false`, settled over silence, exercising `ReverbUnit::new`)
    // matches settled `InputFx` mono mean within 1e-6 and keeps distinct L and R tails.
    for spec in [PIN_SPECS[4], PIN_SPECS[5]] {
        let mut send_rig = Rig::new();
        let mut unit_rig = Rig::new();
        assert!(unit_rig.port(0).install_at(1, 1, 0, false, None, Box::new(ReverbUnit::new(SR))).is_ok());
        for &(p, v) in spec.params {
            send_rig.send_at(F0, Command::SetInputSendParam(p, v));
            unit_rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(p), v));
        }
        send_rig.send_at(F0, Command::SetInputSend(InputSend::Reverb, true));
        send_rig.advance(2400);
        unit_rig.advance(2400);
        let start = send_rig.frame;
        let (sa, sb) = (input.clone(), input.clone());
        send_rig.set_input(move |f| sa.get((f - start) as usize).copied().unwrap_or(0.0));
        unit_rig.set_stereo_inputs(
            move |f| {
                let x = sb.get((f - start) as usize).copied().unwrap_or(0.0);
                [x, x]
            },
            |_| [0.0, 0.0],
        );
        send_rig.keep_output();
        unit_rig.keep_output();
        send_rig.advance(SR as Frame);
        unit_rig.advance(SR as Frame);
        let folded: Vec<f32> = unit_rig.record.iter().zip(&unit_rig.record_right).map(|(&l, &r)| lf_engine::fold(l, r)).collect();
        assert!(max_abs_diff(&folded, &send_rig.record) < 1e-6, "{} active-at-install mean vs InputFx", spec.id);
        assert!(max_abs_diff(&unit_rig.record, &unit_rig.record_right) > 0.01, "{} active-at-install L != R", spec.id);
        drain_returns(&mut unit_rig);
    }

    // Stereo input: REVERB feeds the pair's mean (`fold(L, R)`) to its convolver and adds L and R
    // tails to their own channels. An opposite-polarity signal (`L = -R`) folds to 0 and leaves dry
    // untouched; an asymmetric stereo signal's wet tail equals the mono fold's wet tail bit for bit.
    let stereo_sig = |f: Frame| -> Pair {
        let k = (f - F0) as usize;
        let l = 0.4 * fdlibm::sin(std::f64::consts::TAU * 131.0 * k as f64 / SR as f64) as f32
            + if k == 1_000 { 0.5 } else { 0.0 };
        let r = -0.2 * fdlibm::sin(std::f64::consts::TAU * 293.0 * k as f64 / SR as f64) as f32
            + if k == 2_500 { -0.3 } else { 0.0 };
        [l, r]
    };
    let frames = SR as Frame / 2;
    let mut mono_ref = Rig::new();
    mono_ref.set_stereo_inputs(
        move |f| {
            let [l, r] = stereo_sig(f);
            let m = lf_engine::fold(l, r);
            [m, m]
        },
        |_| [0.0, 0.0],
    );
    assert!(mono_ref.port(0).install_at(1, 1, 0, false, None, Box::new(new_reverb_unit())).is_ok());
    mono_ref.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::ReverbLevel), 0.75));
    mono_ref.keep_output();
    mono_ref.advance(frames);
    let want_wet_l: Vec<f32> = mono_ref.monitor.iter().enumerate().map(|(k, &y)| {
        let [l, r] = stereo_sig(F0 + k as Frame);
        y - lf_engine::fold(l, r)
    }).collect();
    let want_wet_r: Vec<f32> = mono_ref.monitor_right.iter().enumerate().map(|(k, &y)| {
        let [l, r] = stereo_sig(F0 + k as Frame);
        y - lf_engine::fold(l, r)
    }).collect();
    drain_returns(&mut mono_ref);

    for block in BLOCKS {
        let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
        rig.set_stereo_inputs(stereo_sig, |_| [0.0, 0.0]);
        assert!(rig.port(0).install_at(1, 1, 0, false, None, Box::new(new_reverb_unit())).is_ok());
        rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::ReverbLevel), 0.75));
        rig.keep_output();
        rig.advance(frames);
        let got_wet_l: Vec<f32> = rig.monitor.iter().enumerate().map(|(k, &y)| y - stereo_sig(F0 + k as Frame)[0]).collect();
        let got_wet_r: Vec<f32> = rig.monitor_right.iter().enumerate().map(|(k, &y)| y - stereo_sig(F0 + k as Frame)[1]).collect();
        assert!(max_abs_diff(&got_wet_l, &want_wet_l) < 1e-6, "stereo fold wet L block {block}");
        assert!(max_abs_diff(&got_wet_r, &want_wet_r) < 1e-6, "stereo fold wet R block {block}");
        assert!(max_abs_diff(&got_wet_l, &got_wet_r) > 0.005, "stereo fold wet L != R block {block}");
        drain_returns(&mut rig);
    }

    // Param clamping, non-finite rejection and wrong-kind rejection.
    let mut rig = Rig::new();
    assert!(rig.port(0).install_at(1, 1, 0, false, None, Box::new(new_reverb_unit())).is_ok());
    rig.advance(128);
    let t = rig.frame + 5;
    rig.send_at(t, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::ReverbLevel), 2.5));
    rig.send_at(t + 1, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::ReverbLevel), -0.5));
    rig.send_at(t + 2, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::ReverbLevel), f64::NAN));
    rig.send_at(t + 3, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::ReverbLevel), f64::INFINITY));
    rig.send_at(t + 4, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.5));
    rig.send_at(t + 5, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDepth), 0.5));
    rig.advance(256);
    let params: Vec<_> = rig
        .events
        .iter()
        .filter_map(|e| match *e {
            Event::UnitParam { unit: 1, param, value, .. } => Some((param, value)),
            _ => None,
        })
        .collect();
    assert_eq!(
        params,
        vec![
            (UnitParam::Send(InputSendParam::ReverbLevel), 1.0),
            (UnitParam::Send(InputSendParam::ReverbLevel), 0.0),
        ]
    );
    assert_eq!(rig.engine.diag().unit_params_dropped, 4);
    drain_returns(&mut rig);
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

    for send in [InputSend::Echo, InputSend::Ring, InputSend::Reverb] {
        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            rig.set_stereo_inputs(stereo_sig, |_| [0.0, 0.0]);
            let (unit, param): (Box<dyn UnitProcessor>, InputSendParam) = match send {
                InputSend::Echo => (Box::new(EchoUnit::new(SR)), InputSendParam::EchoLevel),
                InputSend::Ring => (Box::new(RingUnit::new(SR)), InputSendParam::RingLevel),
                InputSend::Reverb => (Box::new(new_reverb_unit()), InputSendParam::ReverbLevel),
                _ => unreachable!(),
            };
            assert!(rig.port(0).install_at(1, 1, 0, false, None, unit).is_ok());
            rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(param), 0.0));
            // Past the 20 ms ramp so level is settled at exact 0.0 while audio has already filled ECHO's line / REVERB's convolver.
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

    // Direct `UnitProcessor::process` at settled level 0 preserves even `-0.0` (under `assert_no_alloc`).
    let cx0 = UnitCx { frame: 0, dsp_frame: 0, bpm: 120.0, meter: Meter::FOUR_FOUR };
    let mut echo = EchoUnit::new(SR);
    let mut ring = RingUnit::new(SR);
    let mut reverb = new_reverb_unit();
    assert_eq!(assert_no_alloc(|| echo.set_param(&cx0, UnitParam::Send(InputSendParam::EchoLevel), 0.0)), Some(0.0));
    assert_eq!(assert_no_alloc(|| ring.set_param(&cx0, UnitParam::Send(InputSendParam::RingLevel), 0.0)), Some(0.0));
    assert_eq!(assert_no_alloc(|| reverb.set_param(&cx0, UnitParam::Send(InputSendParam::ReverbLevel), 0.0)), Some(0.0));
    let x_l = [-0.0f32; 128];
    let x_r = [-0.0f32; 128];
    let mut out_l = [0.0f32; 128];
    let mut out_r = [0.0f32; 128];
    for q in 0..20u64 {
        let cx = UnitCx { frame: (q * 128) as Frame, dsp_frame: (q * 128) as Frame, bpm: 120.0, meter: Meter::FOUR_FOUR };
        assert_no_alloc(|| echo.process(&cx, [&x_l, &x_r], &[], [&mut out_l, &mut out_r]));
        if q >= 16 {
            assert_bits_eq(&out_l, &x_l, "direct echo level 0 preserves -0.0 L");
            assert_bits_eq(&out_r, &x_r, "direct echo level 0 preserves -0.0 R");
        }
        assert_no_alloc(|| ring.process(&cx, [&x_l, &x_r], &[], [&mut out_l, &mut out_r]));
        if q >= 16 {
            assert_bits_eq(&out_l, &x_l, "direct ring level 0 preserves -0.0 L");
            assert_bits_eq(&out_r, &x_r, "direct ring level 0 preserves -0.0 R");
        }
        assert_no_alloc(|| reverb.process(&cx, [&x_l, &x_r], &[], [&mut out_l, &mut out_r]));
        if q >= 16 {
            assert_bits_eq(&out_l, &x_l, "direct reverb level 0 preserves -0.0 L");
            assert_bits_eq(&out_r, &x_r, "direct reverb level 0 preserves -0.0 R");
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
    ReverbA,
    ReverbB,
    OttA,
    OttB,
    Clip,
    Gain,
}

impl Stage {
    fn build(self) -> Box<dyn UnitProcessor> {
        match self {
            Stage::EchoA | Stage::EchoB => Box::new(EchoUnit::new(SR)),
            Stage::RingA | Stage::RingB => Box::new(RingUnit::new(SR)),
            Stage::ReverbA | Stage::ReverbB => Box::new(new_reverb_unit()),
            Stage::OttA | Stage::OttB => Box::new(OttUnit::new(SR)),
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
            Stage::ReverbA => {
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::ReverbLevel), 0.85));
            }
            Stage::ReverbB => {
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::ReverbLevel), 0.6));
            }
            Stage::OttA => {
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::OttDepth), 0.9));
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::OttTime), 0.35));
            }
            Stage::OttB => {
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::OttDepth), 0.7));
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::OttUpward), 0.8));
                rig.send_at(at, Command::SetUnitParam(id, UnitParam::Send(InputSendParam::OttDownward), 0.6));
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

/// §5 item 2: Composition pairs holding REVERB and OTT equal staged processing bit for bit across
/// all 7 block sizes, and every non-commuting pair's two orders differ above a stated floor.
#[test]
fn composition_with_reverb_and_ott_matches_staged_and_order_differs() {
    let frames = SR as usize / 2; // 0.5 s = 24_000 frames (> 1/8 = 12_000, full REVERB + OTT interaction)
    let mut rng = Mulberry32::new(107);
    let input: Arc<Vec<Pair>> = Arc::new(
        (0..frames)
            .map(|n| {
                // Burst then quiet then louder so OTT dynamics and REVERB/ECHO tails differ sharply by order.
                let env = if n < 4_000 { 0.7 } else if n < 12_000 { 0.05 } else { 0.45 };
                let l = (env * (0.55 * fdlibm::sin(std::f64::consts::TAU * 137.0 * n as f64 / SR as f64)
                    + 0.35 * (rng.next_f64() * 2.0 - 1.0))) as f32;
                let r = (env * (0.45 * fdlibm::sin(std::f64::consts::TAU * 229.0 * n as f64 / SR as f64)
                    + 0.25 * (rng.next_f64() * 2.0 - 1.0))) as f32;
                [l, r]
            })
            .collect(),
    );

    let pairs = [
        (Stage::ReverbA, Stage::EchoA),
        (Stage::EchoA, Stage::ReverbA),
        (Stage::ReverbA, Stage::RingA),
        (Stage::RingA, Stage::ReverbA),
        (Stage::ReverbA, Stage::Clip),
        (Stage::Clip, Stage::ReverbA),
        (Stage::ReverbA, Stage::Gain),
        (Stage::Gain, Stage::ReverbA),
        (Stage::ReverbA, Stage::ReverbB),
        (Stage::ReverbB, Stage::ReverbA),
        (Stage::OttA, Stage::EchoA),
        (Stage::EchoA, Stage::OttA),
        (Stage::OttA, Stage::ReverbA),
        (Stage::ReverbA, Stage::OttA),
        (Stage::OttA, Stage::RingA),
        (Stage::RingA, Stage::OttA),
        (Stage::OttA, Stage::Clip),
        (Stage::Clip, Stage::OttA),
        (Stage::OttA, Stage::Gain),
        (Stage::Gain, Stage::OttA),
        (Stage::OttA, Stage::OttB),
        (Stage::OttB, Stage::OttA),
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

    // Non-commuting pairs must differ past their stated floor on both channels.
    for (a, b, floor) in [
        (Stage::ReverbA, Stage::RingA, 0.02f32),
        (Stage::ReverbA, Stage::Clip, 0.02f32),
        (Stage::OttA, Stage::EchoA, 0.05f32),
        (Stage::OttA, Stage::ReverbA, 0.02f32),
        (Stage::OttA, Stage::RingA, 0.05f32),
        (Stage::OttA, Stage::Clip, 0.05f32),
        (Stage::OttA, Stage::Gain, 0.05f32),
        (Stage::OttA, Stage::OttB, 0.02f32),
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
    //    impulse on the exact same loop frame whether bypassed or active, at every block size.
    for send in [InputSend::Echo, InputSend::Ring] {
        for bypassed in [false, true] {
            for block in BLOCKS {
                let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
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
                assert_eq!(rig.engine.live_latency(), 37, "{send:?} bypassed={bypassed} block={block}: latency unchanged");
                let start = rig.start_frame();
                let hit = start + 2000 - 37;
                rig.set_input(move |f| if f == hit { 1.0 } else { 0.0 });
                rig.advance_to(rig.end_frame() + 1);
                assert_eq!(rig.state(0), LaneState::Playing);
                let pcm = rig.pcm(0);
                let first_nz = pcm.iter().position(|&x| x != 0.0);
                assert_eq!(first_nz, Some(2000), "{send:?} bypassed={bypassed} block={block}: dry note lands on loop frame 2000");
                drain_returns(&mut rig);
            }
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

// ── Review round fixes: same-frame tempo+time, first-frame un-bypass, per-frame -0.0, split guard, held BPM ──

/// Same-frame `SetBpm(60)` and ECHO time change (1/8 -> 1/16) in both ring orders: `EchoUnit` matches
/// today's `InputFx` send bit for bit across all 7 block sizes (both on a quantum boundary and
/// mid-quantum, after > 0.5 s of audio fills the delay line).
#[test]
fn same_frame_bpm_and_echo_time_match_input_fx_in_both_orders() {
    let sig = |f: Frame| -> f32 {
        let k = (f - F0) as usize;
        0.3 * fdlibm::sin(std::f64::consts::TAU * 131.0 * k as f64 / SR as f64) as f32
            + if k == 2_000 || k == 14_000 { 0.5 } else { 0.0 }
    };
    let on_at = F0 + 137;
    // Both a quantum boundary (25_600 = 200 * 128, > 0.5 s into the signal so 0.5 s vs 0.25 s reads
    // distinct non-zero samples) and a mid-quantum frame (25_613) with a second tempo.
    let cases = [
        ("qbound_bpm60_16n", F0 + 25_600, 60.0, 3.0),
        ("midq_bpm80_16n", F0 + 25_613, 80.0, 3.0),
    ];
    let total = SR as Frame;

    for (case, change_at, new_bpm, new_time) in cases {
        for bpm_first in [true, false] {
            let tag = format!("{case}_{}", if bpm_first { "bpm_then_time" } else { "time_then_bpm" });
            let mut srig = Rig::new();
            srig.set_input(sig);
            srig.send_at(on_at, Command::SetInputSend(InputSend::Echo, true));
            srig.send_at(on_at, Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.5));
            srig.send_at(on_at, Command::SetInputSendParam(InputSendParam::EchoLevel, 0.7));
            if bpm_first {
                srig.send_at(change_at, Command::SetBpm(new_bpm));
                srig.send_at(change_at, Command::SetInputSendParam(InputSendParam::EchoTime, new_time));
            } else {
                srig.send_at(change_at, Command::SetInputSendParam(InputSendParam::EchoTime, new_time));
                srig.send_at(change_at, Command::SetBpm(new_bpm));
            }
            srig.keep_output();
            srig.advance(total);
            let want = srig.monitor.clone();

            for block in BLOCKS {
                let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
                rig.set_input(sig);
                assert!(rig.port(0).install_at(1, 1, 0, true, None, Box::new(EchoUnit::new(SR))).is_ok());
                rig.send_at(on_at, Command::SetUnitBypass(1, false));
                rig.send_at(on_at, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoFeedback), 0.5));
                rig.send_at(on_at, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.7));
                if bpm_first {
                    rig.send_at(change_at, Command::SetBpm(new_bpm));
                    rig.send_at(change_at, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoTime), new_time));
                } else {
                    rig.send_at(change_at, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoTime), new_time));
                    rig.send_at(change_at, Command::SetBpm(new_bpm));
                }
                rig.keep_output();
                rig.advance(total);
                assert_bits_eq(&rig.monitor, &want, &format!("{tag} block {block} monitor L"));
                assert_bits_eq(&rig.monitor_right, &want, &format!("{tag} block {block} monitor R"));
                assert_bits_eq(&rig.record, &want, &format!("{tag} block {block} record L"));
                assert_bits_eq(&rig.record_right, &want, &format!("{tag} block {block} record R"));
                drain_returns(&mut rig);
            }
        }
    }
}

/// Un-bypassing on (or before) the first rendered frame after a bypassed install fades in over 20 ms
/// from the command's frame, never snapping open, for ECHO, RING MOD and REVERB across all block sizes.
#[test]
fn unbypass_on_first_frame_after_bypassed_install_fades_20ms() {
    const RAMP_FRAMES: usize = 960; // 20 ms at 48 kHz
    for send in [InputSend::Echo, InputSend::Ring, InputSend::Reverb] {
        // Reference from `InputFx` switched on at `F0` (its first rendered frame).
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
            InputSend::Reverb => {
                srig.send_at(F0, Command::SetInputSendParam(InputSendParam::ReverbLevel, 1.0));
            }
            _ => unreachable!(),
        }
        srig.send_at(F0, Command::SetInputSend(send, true));
        srig.keep_output();
        srig.advance(10_000);
        let want = srig.monitor.clone();

        for idle_install in [false, true] {
            for block in BLOCKS {
                let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
                if send == InputSend::Reverb {
                    rig.set_stereo_inputs(|_| [0.5, 0.5], |_| [0.0, 0.0]);
                } else {
                    rig.set_level(0.5);
                }
                let unit: Box<dyn UnitProcessor> = match send {
                    InputSend::Echo => Box::new(EchoUnit::new(SR)),
                    InputSend::Ring => Box::new(RingUnit::new(SR)),
                    InputSend::Reverb => Box::new(new_reverb_unit()),
                    _ => unreachable!(),
                };
                assert!(rig.port(0).install_at(1, 1, 0, true, None, unit).is_ok());
                if idle_install {
                    // Seat bypassed before any frame renders.
                    rig.engine.service_slots_idle();
                }
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
                    InputSend::Reverb => {
                        rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::ReverbLevel), 1.0));
                    }
                    _ => unreachable!(),
                }
                rig.send_at(F0, Command::SetUnitBypass(1, false));
                rig.keep_output();
                rig.advance(10_000);

                let tag = format!("{send:?} idle={idle_install} block={block}");
                let wet: Vec<f32> = rig.monitor.iter().map(|&y| y - 0.5).collect();
                let shift = if send == InputSend::Echo { SIXTEENTH as usize } else { 0 };
                match send {
                    InputSend::Echo => {
                        assert!(wet[shift] < 1e-4, "{tag}: must start near 0, not snapped (got {})", wet[shift]);
                        assert!(wet[shift + 10] > 0.0 && wet[shift + 10] < 0.05, "{tag}: early ramp {}", wet[shift + 10]);
                        assert!(wet[shift + RAMP_FRAMES / 2] > 0.20 && wet[shift + RAMP_FRAMES / 2] < 0.30, "{tag}: mid ramp {}", wet[shift + RAMP_FRAMES / 2]);
                        assert!(wet[shift + RAMP_FRAMES..shift + RAMP_FRAMES + 200].iter().all(|&w| w == 0.5), "{tag}: settled after 20 ms");
                        assert_bits_eq(&rig.monitor, &want, &format!("{tag} monitor L vs InputFx"));
                        assert_bits_eq(&rig.monitor_right, &want, &format!("{tag} monitor R vs InputFx"));
                    }
                    InputSend::Ring => {
                        let early_peak = wet[0..100].iter().map(|x| x.abs()).fold(0.0f32, f32::max);
                        let mid_peak = wet[400..560].iter().map(|x| x.abs()).fold(0.0f32, f32::max);
                        let late_peak = wet[RAMP_FRAMES..RAMP_FRAMES + 400].iter().map(|x| x.abs()).fold(0.0f32, f32::max);
                        assert!(early_peak > 1e-3 && early_peak < 0.08, "{tag}: early ramp peak {early_peak}");
                        assert!(mid_peak > 0.18 && mid_peak < 0.32, "{tag}: mid ramp peak {mid_peak}");
                        assert!((late_peak - 0.5).abs() < 1e-3, "{tag}: settled peak {late_peak}");
                        assert_bits_eq(&rig.monitor, &want, &format!("{tag} monitor L vs InputFx"));
                        assert_bits_eq(&rig.monitor_right, &want, &format!("{tag} monitor R vs InputFx"));
                    }
                    InputSend::Reverb => {
                        // Starts at 0 (not snapped): wet[0] is exact 0 because gate(0) == 0.
                        assert!(wet[0].abs() < 1e-6, "{tag}: reverb must start at 0, not snapped (got {})", wet[0]);
                        let folded: Vec<f32> = rig.monitor.iter().zip(&rig.monitor_right).map(|(&l, &r)| lf_engine::fold(l, r)).collect();
                        assert!(max_abs_diff(&folded, &want) < 1e-6, "{tag}: folded reverb vs InputFx");
                    }
                    _ => unreachable!(),
                }
                drain_returns(&mut rig);
            }
        }
    }
}

/// `-0.0` input through a level ramp reaching 0 mid-quantum (frame 960, inside quantum 7 `[896, 1024)`),
/// checked from the ramp's start (and with live ECHO delay line / REVERB tail at frame 960..1024),
/// under `assert_no_alloc`, across all block sizes.
#[test]
fn level_ramp_to_zero_preserves_signed_zero_per_frame_inside_quantum() {
    let cx0 = UnitCx { frame: 0, dsp_frame: 0, bpm: 120.0, meter: Meter::FOUR_FOUR };
    let total = 1536usize; // covers quanta 0..12, including frame 960 inside quantum 7 [896, 1024)
    let neg_zero = vec![-0.0f32; 1024];

    for block in BLOCKS {
        let mut echo = EchoUnit::new(SR);
        let mut ring = RingUnit::new(SR);
        let mut reverb = new_reverb_unit();
        assert_eq!(assert_no_alloc(|| echo.set_param(&cx0, UnitParam::Send(InputSendParam::EchoLevel), 0.0)), Some(0.0));
        assert_eq!(assert_no_alloc(|| ring.set_param(&cx0, UnitParam::Send(InputSendParam::RingLevel), 0.0)), Some(0.0));
        assert_eq!(assert_no_alloc(|| reverb.set_param(&cx0, UnitParam::Send(InputSendParam::ReverbLevel), 0.0)), Some(0.0));

        let mut out_l = vec![0.0f32; block];
        let mut out_r = vec![0.0f32; block];
        let mut pos = 0usize;
        while pos < total {
            let n = block.min(total - pos);
            let cx = UnitCx { frame: pos as Frame, dsp_frame: pos as Frame, bpm: 120.0, meter: Meter::FOUR_FOUR };
            let x = &neg_zero[..n];
            assert_no_alloc(|| echo.process(&cx, [x, x], &[], [&mut out_l[..n], &mut out_r[..n]]));
            assert_bits_eq(&out_l[..n], x, &format!("echo ramp-to-0 block {block} frame {pos} L"));
            assert_bits_eq(&out_r[..n], x, &format!("echo ramp-to-0 block {block} frame {pos} R"));

            assert_no_alloc(|| ring.process(&cx, [x, x], &[], [&mut out_l[..n], &mut out_r[..n]]));
            assert_bits_eq(&out_l[..n], x, &format!("ring ramp-to-0 block {block} frame {pos} L"));
            assert_bits_eq(&out_r[..n], x, &format!("ring ramp-to-0 block {block} frame {pos} R"));

            assert_no_alloc(|| reverb.process(&cx, [x, x], &[], [&mut out_l[..n], &mut out_r[..n]]));
            assert_bits_eq(&out_l[..n], x, &format!("reverb ramp-to-0 block {block} frame {pos} L"));
            assert_bits_eq(&out_r[..n], x, &format!("reverb ramp-to-0 block {block} frame {pos} R"));
            pos += n;
        }
    }

    // Even when ECHO's delay line or REVERB's convolver holds live non-zero audio while the level
    // ramp hits 0 mid-quantum, every frame from the ramp's zero point through the quantum's end
    // passes `-0.0` untouched.
    let mut echo = EchoUnit::new(SR);
    let mut reverb = new_reverb_unit();
    assert_eq!(assert_no_alloc(|| echo.set_param(&cx0, UnitParam::Send(InputSendParam::EchoTime), 3.0)), Some(3.0)); // 1/16 = 6000
    let fill_in = [0.5f32; 128];
    let neg_in = [-0.0f32; 128];
    let mut out_l = [0.0f32; 128];
    let mut out_r = [0.0f32; 128];
    // 55 quanta = 7040 frames > 6000 so ECHO's line and REVERB's convolver both output non-zero on every frame.
    for q in 0..55u64 {
        let f = (q * 128) as Frame;
        let cx = UnitCx { frame: f, dsp_frame: f, bpm: 120.0, meter: Meter::FOUR_FOUR };
        assert_no_alloc(|| echo.process(&cx, [&fill_in, &fill_in], &[], [&mut out_l, &mut out_r]));
        assert_no_alloc(|| reverb.process(&cx, [&fill_in, &fill_in], &[], [&mut out_l, &mut out_r]));
    }
    let ramp_start = 55 * 128;
    let cx_ramp = UnitCx { frame: ramp_start, dsp_frame: ramp_start, bpm: 120.0, meter: Meter::FOUR_FOUR };
    assert_eq!(assert_no_alloc(|| echo.set_param(&cx_ramp, UnitParam::Send(InputSendParam::EchoLevel), 0.0)), Some(0.0));
    assert_eq!(assert_no_alloc(|| reverb.set_param(&cx_ramp, UnitParam::Send(InputSendParam::ReverbLevel), 0.0)), Some(0.0));
    // Quantum 7 after ramp_start spans [ramp_start + 896, ramp_start + 1024); level hits 0 at +960 (index 64).
    for q in 0..9u64 {
        let f = ramp_start + (q * 128) as Frame;
        let cx = UnitCx { frame: f, dsp_frame: f, bpm: 120.0, meter: Meter::FOUR_FOUR };
        assert_no_alloc(|| echo.process(&cx, [&neg_in, &neg_in], &[], [&mut out_l, &mut out_r]));
        if q == 7 {
            assert!(out_l[0] > 0.0, "echo first half of quantum 7 still has non-zero wet tail");
            assert_bits_eq(&out_l[64..], &neg_in[64..], "echo live-line mid-quantum level-0 preserves -0.0 L");
            assert_bits_eq(&out_r[64..], &neg_in[64..], "echo live-line mid-quantum level-0 preserves -0.0 R");
        }
        assert_no_alloc(|| reverb.process(&cx, [&neg_in, &neg_in], &[], [&mut out_l, &mut out_r]));
        if q == 7 {
            assert!(out_l[..64].iter().any(|&v| v != 0.0), "reverb first half of quantum 7 still has non-zero wet tail");
            assert_bits_eq(&out_l[64..], &neg_in[64..], "reverb live-tail mid-quantum level-0 preserves -0.0 L");
            assert_bits_eq(&out_r[64..], &neg_in[64..], "reverb live-tail mid-quantum level-0 preserves -0.0 R");
        }
    }
}

/// Plugin fake that logs `(cx.frame, out_len)` of every `process` call into a preallocated buffer.
struct CallLogPlugin {
    calls: Vec<(Frame, usize)>,
}

impl CallLogPlugin {
    fn new(cap: usize) -> Self {
        CallLogPlugin { calls: Vec::with_capacity(cap) }
    }
}

impl UnitProcessor for CallLogPlugin {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: false, latency: 0 }
    }

    fn process(&mut self, cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        if self.calls.len() < self.calls.capacity() {
            self.calls.push((cx.frame, out[0].len()));
        }
        out[0].copy_from_slice(input[0]);
        out[1].copy_from_slice(input[1]);
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// A plugins-only rack (or plugins + non-tempo built-ins like RING MOD, REVERB, OTT) does not
/// split `process` at mid-block `SetBpm` / `SetMeter` commands; seating an ECHO does split so ECHO
/// tracks the exact frame.
#[test]
fn plugins_only_rack_does_not_split_process_at_midblock_tempo_commands() {
    for extra in [None, Some(UnitKind::Ring), Some(UnitKind::Reverb), Some(UnitKind::Ott)] {
        let mut rig = Rig::with(Opts { sr: SR, start: F0, block: 128, ..Opts::default() });
        assert!(rig.port(0).install_at(1, 1, 0, false, None, Box::new(CallLogPlugin::new(16))).is_ok());
        if let Some(kind) = extra {
            let unit: Box<dyn UnitProcessor> = match kind {
                UnitKind::Ring => Box::new(RingUnit::new(SR)),
                UnitKind::Reverb => Box::new(new_reverb_unit()),
                UnitKind::Ott => Box::new(OttUnit::new(SR)),
                _ => unreachable!(),
            };
            assert!(rig.port(1).install_at(2, 2, 0, false, None, unit).is_ok());
        }
        rig.send_at(F0 + 17, Command::SetBpm(95.0));
        rig.send_at(F0 + 61, Command::SetMeter(Meter::new(3, 4).unwrap()));
        rig.advance(128);
        rig.engine.evict_slots();
        let ret = rig.port(0).returned_unit().expect("plugin returned");
        let log = ret.unit.into_any().downcast::<CallLogPlugin>().unwrap();
        assert_eq!(log.calls, vec![(F0, 128)], "extra={extra:?}: mid-block tempo commands must not split plugin process");
        while let Some(r) = rig.port(1).returned_unit() {
            drop(r.unit);
        }
    }

    // With an ECHO seated, mid-block `SetBpm` DOES split so ECHO receives the new tempo on its frame.
    let mut rig = Rig::with(Opts { sr: SR, start: F0, block: 128, ..Opts::default() });
    assert!(rig.port(0).install_at(1, 1, 0, false, None, Box::new(CallLogPlugin::new(16))).is_ok());
    assert!(rig.port(0).install_at(2, 2, 1, false, None, Box::new(EchoUnit::new(SR))).is_ok());
    rig.send_at(F0 + 17, Command::SetBpm(95.0));
    rig.advance(128);
    rig.engine.evict_slots();
    let mut plugin_calls = None;
    while let Some(r) = rig.port(0).returned_unit() {
        if r.id == 1 {
            plugin_calls = Some(r.unit.into_any().downcast::<CallLogPlugin>().unwrap().calls);
        } else {
            drop(r.unit);
        }
    }
    assert_eq!(plugin_calls, Some(vec![(F0, 17), (F0 + 17, 111)]), "ECHO seated: splits at SetBpm frame");
}

/// A `SetBpm` held behind a waiting `ClearAll` (while a COPY block job is in flight) applies when the
/// job finishes, not at its stamp: `EchoUnit` follows the tempo on the applied frame, matching
/// `InputFx` bit for bit across all block sizes.
#[test]
fn held_bpm_behind_waiting_clear_all_updates_echo_on_applied_frame() {
    let run = |use_unit: bool, block: usize| -> ([Vec<f32>; 2], [Vec<f32>; 2], u32) {
        let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
        rig.set(Command::SetMetronome(false));
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(1.0));
        if use_unit {
            assert!(rig.port(0).install_at(1, 1, 0, true, None, Box::new(EchoUnit::new(SR))).is_ok());
            rig.send_at(F0 + 137, Command::SetUnitBypass(1, false));
            rig.send_at(F0 + 137, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoFeedback), 0.5));
            rig.send_at(F0 + 137, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 0.7));
        } else {
            rig.send_at(F0 + 137, Command::SetInputSend(InputSend::Echo, true));
            rig.send_at(F0 + 137, Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.5));
            rig.send_at(F0 + 137, Command::SetInputSendParam(InputSendParam::EchoLevel, 0.7));
        }
        // Continuous signal from F0 so the ECHO delay line holds distinct non-zero samples at both
        // 0.25 s (120 BPM 1/8) and 0.50 s (60 BPM 1/8) when the held SetBpm applies mid-block.
        rig.set_input(|f| {
            let k = (f - F0).max(0) as usize;
            0.25 * fdlibm::sin(std::f64::consts::TAU * 173.0 * k as f64 / SR as f64) as f32
                + 0.1 * fdlibm::sin(std::f64::consts::TAU * 293.0 * k as f64 / SR as f64) as f32
        });
        // Record a 1-bar loop (96_000 frames at 120 BPM -> COPY job takes 94 frames).
        rig.press(Command::RecDub(0));
        rig.advance_to(rig.end_frame() + 1);
        assert_eq!(rig.state(0), LaneState::Playing);

        // Align to a known absolute frame so all block sizes send commands at identical frames.
        // At block=1024, base + 10..base + 104 + 256 all lie inside one 1024-frame block, so the
        // quantum boundary at base + 128 (after ClearAll+SetBpm apply at base + 104) falls inside it.
        let base = F0 + 2 * SR as Frame;
        rig.advance_to(base);
        let t_copy = base + 10;
        let t_clear = base + 20;
        let t_bpm = base + 30;
        rig.send_at(t_copy, Command::Copy(0));
        rig.send_at(t_clear, Command::ClearAll);
        rig.send_at(t_bpm, Command::SetBpm(60.0));
        rig.keep_output();
        rig.advance(2048);
        let bpm = rig.engine.clock().bpm();
        let mon = [rig.monitor.clone(), rig.monitor_right.clone()];
        let rec = [rig.record.clone(), rig.record_right.clone()];
        drain_returns(&mut rig);
        (mon, rec, bpm)
    };

    let (want_mon, want_rec, want_bpm) = run(false, 128);
    assert_eq!(want_bpm, 60, "held SetBpm applied once ClearAll finished");
    for block in BLOCKS {
        let (mon, rec, bpm) = run(true, block);
        assert_eq!(bpm, 60, "block {block}: held SetBpm applied");
        assert_bits_eq(&mon[0], &want_mon[0], &format!("held SetBpm block {block} monitor L"));
        assert_bits_eq(&mon[1], &want_mon[1], &format!("held SetBpm block {block} monitor R"));
        assert_bits_eq(&rec[0], &want_rec[0], &format!("held SetBpm block {block} record L"));
        assert_bits_eq(&rec[1], &want_rec[1], &format!("held SetBpm block {block} record R"));
    }
}

// ── E3b: REVERB bypass (§5 item 3) and same-frame commands ───────────────────────────────────────────

#[test]
fn reverb_bypass_settled_is_input_rings_out_and_unbypass_ramps_20ms() {
    // 1. Cold install bypassed: output is input bit for bit from frame 0 across all 7 block sizes.
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
    for block in BLOCKS {
        let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
        rig.set_stereo_inputs(cold_sig, |_| [0.0, 0.0]);
        assert!(rig.port(0).install_at(1, 1, 0, true, None, Box::new(new_reverb_unit())).is_ok());
        rig.keep_output();
        rig.advance(4096);
        let want_l: Vec<f32> = (0..4096).map(|k| cold_sig(F0 + k)[0]).collect();
        let want_r: Vec<f32> = (0..4096).map(|k| cold_sig(F0 + k)[1]).collect();
        assert_bits_eq(&rig.monitor, &want_l, &format!("cold bypass reverb block {block} L"));
        assert_bits_eq(&rig.monitor_right, &want_r, &format!("cold bypass reverb block {block} R"));
        assert_bits_eq(&rig.record, &want_l, &format!("cold bypass reverb block {block} rec L"));
        assert_bits_eq(&rig.record_right, &want_r, &format!("cold bypass reverb block {block} rec R"));
        drain_returns(&mut rig);
    }

    // 2. Active -> bypass: drive continuous input straight through the 20 ms closing edge so a
    //    shorter (e.g. 10 ms) closing ramp diverges from `InputFx` and fails; REVERB still rings
    //    out past bypass on both channels (with L != R), ignores a new impulse once the 20 ms gate
    //    is shut, and settles to exact dry input bit for bit once the 2.62 s IR tail has passed.
    let settle_on = 2400 as Frame;
    let start_at = F0 + settle_on;
    let bypass_at = start_at + 2_000; // mid-quantum
    let drive_end = bypass_at + 1_500; // > bypass_at + 960 + 128: drives across the whole closing ramp
    let ignored_hit = start_at + 4_500; // > bypass_at + 960 + 128: gate is shut
    let settled_at = start_at + 4 * SR as Frame; // > bypass_at + 20 ms + 2.62 s IR tail
    let total = 4 * SR as Frame + SR as Frame / 2;
    let active_sig = move |f: Frame| -> Pair {
        if f >= start_at && f < drive_end {
            let k = (f - start_at) as f64;
            let s = (0.4 * fdlibm::sin(std::f64::consts::TAU * 199.0 * k / SR as f64)) as f32;
            [s, s]
        } else if f == ignored_hit {
            [0.75, -0.25]
        } else if f >= settled_at && (f - start_at) % 19 == 0 {
            [0.0, f32::from_bits(1)]
        } else if f >= settled_at {
            [0.2, -0.15]
        } else {
            [0.0, 0.0]
        }
    };
    let mut srig = Rig::new();
    srig.send_at(F0, Command::SetInputSendParam(InputSendParam::ReverbLevel, 1.0));
    srig.send_at(F0, Command::SetInputSend(InputSend::Reverb, true));
    srig.advance(settle_on);
    srig.set_input(move |f| {
        let [l, r] = active_sig(f);
        lf_engine::fold(l, r)
    });
    srig.send_at(bypass_at, Command::SetInputSend(InputSend::Reverb, false));
    srig.keep_output();
    srig.advance(total);
    let want_close_mono = srig.monitor.clone();

    let mut rev_ref: Option<[Vec<f32>; 2]> = None;
    for block in BLOCKS {
        let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
        assert!(rig.port(0).install_at(1, 1, 0, false, None, Box::new(new_reverb_unit())).is_ok());
        rig.send_at(F0, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::ReverbLevel), 1.0));
        rig.advance(settle_on);
        rig.set_stereo_inputs(active_sig, |_| [0.0, 0.0]);
        rig.send_at(bypass_at, Command::SetUnitBypass(1, true));
        rig.keep_output();
        rig.advance(total);

        // Closing edge + full tail match `InputFx`'s 20 ms closing ramp within 1e-6.
        let folded: Vec<f32> = rig.monitor.iter().zip(&rig.monitor_right).map(|(&l, &r)| lf_engine::fold(l, r)).collect();
        let close_err = max_abs_diff(&folded, &want_close_mono);
        assert!(close_err < 1e-6, "block {block}: reverb closing ramp + tail vs InputFx diff {close_err}");

        // Tail rings out well after bypass_at (e.g. [0.25 s, 1.0 s)) on both L and R, distinctly.
        let ring_win = (SR as usize / 4)..(SR as usize);
        let energy_l: f64 = rig.monitor[ring_win.clone()].iter().map(|&x| (x as f64) * (x as f64)).sum();
        let energy_r: f64 = rig.monitor_right[ring_win.clone()].iter().map(|&x| (x as f64) * (x as f64)).sum();
        assert!(energy_l > 1e-4, "block {block}: reverb L tail must ring out while bypassed (energy {energy_l})");
        assert!(energy_r > 1e-4, "block {block}: reverb R tail must ring out while bypassed (energy {energy_r})");
        assert!(max_abs_diff(&rig.monitor[ring_win.clone()], &rig.monitor_right[ring_win]) > 1e-3, "block {block}: bypassed tail stays stereo");

        // ignored_hit passes dry on its own frame; its tail never re-triggers after settle.
        let ki = (ignored_hit - start_at) as usize;
        assert!((rig.monitor[ki] - 0.75).abs() < 0.25, "block {block}: dry passes at ignored_hit");
        let s4 = 4 * SR as usize;
        let end_k = total as usize;
        let want_l: Vec<f32> = (s4..end_k).map(|k| if k % 19 == 0 { 0.0 } else { 0.2 }).collect();
        let want_r: Vec<f32> = (s4..end_k).map(|k| if k % 19 == 0 { f32::from_bits(1) } else { -0.15 }).collect();
        assert_bits_eq(&rig.monitor[s4..], &want_l, &format!("reverb settled bypass block {block} L"));
        assert_bits_eq(&rig.monitor_right[s4..], &want_r, &format!("reverb settled bypass block {block} R"));

        let cur = [rig.monitor.clone(), rig.monitor_right.clone()];
        let base = rev_ref.get_or_insert_with(|| cur.clone());
        assert_bits_eq(&cur[0], &base[0], &format!("reverb bypass block {block} vs 1 L"));
        assert_bits_eq(&cur[1], &base[1], &format!("reverb bypass block {block} vs 1 R"));
        drain_returns(&mut rig);
    }

    // 3. Un-bypass mid-quantum fades in over 20 ms from the command's frame, matching `InputFx`
    //    (mean within 1e-6, bit-identical across all 7 block sizes). Also covers same-frame
    //    `SetUnitBypass(false)` + `SetUnitParam(ReverbLevel, 0.85)` in both ring orders.
    let unbypass_at = F0 + 20 * 128 - 78; // 78 frames before quantum 20
    let boundary = F0 + 20 * 128;
    for bypass_first in [true, false] {
        let mut srig = Rig::new();
        srig.set_level(0.5);
        if bypass_first {
            srig.send_at(unbypass_at, Command::SetInputSend(InputSend::Reverb, true));
            srig.send_at(unbypass_at, Command::SetInputSendParam(InputSendParam::ReverbLevel, 0.85));
        } else {
            srig.send_at(unbypass_at, Command::SetInputSendParam(InputSendParam::ReverbLevel, 0.85));
            srig.send_at(unbypass_at, Command::SetInputSend(InputSend::Reverb, true));
        }
        srig.keep_output();
        srig.advance(12_000);
        let want_mono = srig.monitor.clone();

        let mut base_l: Option<Vec<f32>> = None;
        let mut base_r: Option<Vec<f32>> = None;
        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            rig.set_stereo_inputs(|_| [0.5, 0.5], |_| [0.0, 0.0]);
            assert!(rig.port(0).install_at(1, 1, 0, true, None, Box::new(new_reverb_unit())).is_ok());
            if bypass_first {
                rig.send_at(unbypass_at, Command::SetUnitBypass(1, false));
                rig.send_at(unbypass_at, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::ReverbLevel), 0.85));
            } else {
                rig.send_at(unbypass_at, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::ReverbLevel), 0.85));
                rig.send_at(unbypass_at, Command::SetUnitBypass(1, false));
            }
            rig.keep_output();
            rig.advance(12_000);

            let b_idx = (boundary - F0) as usize;
            // Silent before quantum boundary: exact dry 0.5.
            assert!(rig.monitor[..b_idx].iter().all(|&y| y.to_bits() == 0.5f32.to_bits()), "reverb unbypass silent before boundary");
            assert!(rig.monitor_right[..b_idx].iter().all(|&y| y.to_bits() == 0.5f32.to_bits()), "reverb unbypass R silent before boundary");
            // Wet tail appears after boundary (and ramps in smoothly over 20 ms, matching InputFx mean).
            let folded: Vec<f32> = rig.monitor.iter().zip(&rig.monitor_right).map(|(&l, &r)| lf_engine::fold(l, r)).collect();
            assert!(max_abs_diff(&folded, &want_mono) < 1e-6, "reverb unbypass bypass_first={bypass_first} block {block} mean vs InputFx");
            assert!(max_abs_diff(&rig.monitor[b_idx..], &rig.monitor_right[b_idx..]) > 1e-3, "reverb unbypass stereo L != R");
            let bl = base_l.get_or_insert_with(|| rig.monitor.clone());
            let br = base_r.get_or_insert_with(|| rig.monitor_right.clone());
            assert_bits_eq(&rig.monitor, bl, &format!("reverb unbypass bypass_first={bypass_first} block {block} L"));
            assert_bits_eq(&rig.monitor_right, br, &format!("reverb unbypass bypass_first={bypass_first} block {block} R"));
            drain_returns(&mut rig);
        }
    }

    // 4. Latency unchanged by bypass: `[Delay(37), ReverbUnit]` keeps live_latency == 37 and
    //    records an impulse on the exact same loop frame whether bypassed or active across all block sizes.
    for bypassed in [false, true] {
        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            assert!(rig.port(0).install_at(1, 1, 0, false, None, Box::new(Delay::new(37))).is_ok());
            assert!(rig.port(0).install_at(2, 2, 1, bypassed, None, Box::new(new_reverb_unit())).is_ok());
            rig.set(Command::SetMetronome(false));
            rig.set(Command::SetFixedLength(true));
            rig.set(Command::SetFixedBars(1.0));
            rig.press(Command::RecDub(0));
            assert_eq!(rig.engine.live_latency(), 37, "reverb bypassed={bypassed} block={block}: latency unchanged");
            let start = rig.start_frame();
            let hit = start + 2000 - 37;
            rig.set_stereo_inputs(move |f| if f == hit { [1.0, 1.0] } else { [0.0, 0.0] }, |_| [0.0, 0.0]);
            rig.advance_to(rig.end_frame() + 1);
            assert_eq!(rig.state(0), LaneState::Playing);
            let pcm = rig.pcm2(0);
            let first_nz_l = pcm.iter().position(|&[l, _]| l != 0.0);
            let first_nz_r = pcm.iter().position(|&[_, r]| r != 0.0);
            assert_eq!(first_nz_l, Some(2000), "reverb bypassed={bypassed} block={block}: dry note lands on loop frame 2000 L");
            assert_eq!(first_nz_r, Some(2000), "reverb bypassed={bypassed} block={block}: dry note lands on loop frame 2000 R");
            drain_returns(&mut rig);
        }
    }
}

// ── E3b: OTT unit (`tests/ott.rs` cases on `OttUnit` + `BandMeter`) ──────────────────────────────────

#[derive(Clone, Copy)]
struct OttSection {
    b: [f64; 3],
    a: [f64; 2],
    s: [f64; 2],
}

impl OttSection {
    fn allpass(f0: f64, sr: f64) -> Self {
        let w0 = 2.0 * PI * f0 / sr;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        OttSection {
            b: [(1.0 - alpha) / a0, -2.0 * cos / a0, (1.0 + alpha) / a0],
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            s: [0.0; 2],
        }
    }

    fn tick(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.s[0];
        self.s[0] = self.b[1] * x - self.a[0] * y + self.s[1];
        self.s[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

struct OttNoise(u64);

impl OttNoise {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (x >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

fn ott_db(x: f64) -> f64 {
    20.0 * x.max(1e-300).log10()
}

fn ott_rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| v as f64 * v as f64).sum::<f64>() / x.len() as f64).sqrt()
}

fn ott_peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

fn ott_sine(freq: f64, db: f64, frames: usize, sr: u32) -> Vec<f32> {
    let a = 10f64.powf(db / 20.0);
    (0..frames).map(|k| (a * (2.0 * PI * freq * k as f64 / sr as f64).sin()) as f32).collect()
}

fn ott_makeup(depth: f64) -> f64 {
    (depth * MAKEUP_DB * (LN_10 / 20.0)).exp()
}

fn ott_cx(frame: Frame) -> UnitCx {
    UnitCx { frame, dsp_frame: frame, bpm: 120.0, meter: Meter::FOUR_FOUR }
}

fn ott_unit_run(unit: &mut OttUnit, in_l: &[f32], in_r: &[f32], block: usize) -> (Vec<f32>, Vec<f32>) {
    assert_eq!(in_l.len(), in_r.len());
    let mut out_l = vec![0.0f32; in_l.len()];
    let mut out_r = vec![0.0f32; in_r.len()];
    let mut pos = 0usize;
    while pos < in_l.len() {
        let n = block.min(in_l.len() - pos);
        let cx = ott_cx(pos as Frame);
        assert_no_alloc(|| {
            unit.process(
                &cx,
                [&in_l[pos..pos + n], &in_r[pos..pos + n]],
                &[],
                [&mut out_l[pos..pos + n], &mut out_r[pos..pos + n]],
            );
        });
        pos += n;
    }
    (out_l, out_r)
}

fn settled_ott_unit_at_block(sr: u32, depth: f64, active: bool, block: usize) -> OttUnit {
    let mut unit = OttUnit::new(sr);
    let cx0 = ott_cx(0);
    if !active {
        assert!(assert_no_alloc(|| unit.set_bypass(&cx0, true)));
    }
    assert_eq!(assert_no_alloc(|| unit.set_param(&cx0, UnitParam::Send(InputSendParam::OttDepth), depth)), Some(depth));
    let silence = vec![0.0f32; sr as usize];
    let _ = ott_unit_run(&mut unit, &silence, &silence, block);
    unit
}

fn ott_unit_run_with_params(x: &[f32], params: &[(InputSendParam, f64)], block: usize) -> (Vec<f32>, Vec<f32>) {
    let mut unit = OttUnit::new(SR);
    let cx0 = ott_cx(0);
    assert!(assert_no_alloc(|| unit.set_bypass(&cx0, true)));
    for &(p, v) in params {
        assert!(assert_no_alloc(|| unit.set_param(&cx0, UnitParam::Send(p), v)).is_some());
    }
    assert!(assert_no_alloc(|| unit.set_bypass(&cx0, false)));
    ott_unit_run(&mut unit, x, x, block)
}

fn ott_unit_band_gains(x: &[f32], params: &[(InputSendParam, f64)]) -> (Vec<[f64; 3]>, Vec<f32>, Vec<f32>) {
    let mut unit = OttUnit::new(SR);
    let cx0 = ott_cx(0);
    // Cold bypassed so glided params snap before switching active (matching `tests/ott.rs::band_gains`).
    assert!(assert_no_alloc(|| unit.set_bypass(&cx0, true)));
    for &(p, v) in params {
        assert!(assert_no_alloc(|| unit.set_param(&cx0, UnitParam::Send(p), v)).is_some());
    }
    assert!(assert_no_alloc(|| unit.set_bypass(&cx0, false)));
    let mut ol = [0.0f32; 1];
    let mut or = [0.0f32; 1];
    let mut out_l = Vec::with_capacity(x.len());
    let mut out_r = Vec::with_capacity(x.len());
    let gains = x
        .iter()
        .enumerate()
        .map(|(k, &v)| {
            let s = [v];
            let cx = ott_cx(k as Frame);
            assert_no_alloc(|| unit.process(&cx, [&s, &s], &[], [&mut ol, &mut or]));
            out_l.push(ol[0]);
            out_r.push(or[0]);
            unit.gains()
        })
        .collect();
    (gains, out_l, out_r)
}

/// §5 item 5 (processor half): every DSP test from `tests/ott.rs` run directly on `OttUnit` under
/// `assert_no_alloc`, bit-identical across all 7 `BLOCKS` sizes.
#[test]
fn ott_unit_dsp_properties_from_ott_suite() {
    // 1. Unity bands null against AP88 · AP2500 (< -130 dB) and are flat (< 0.05 dB) at 44.1 & 48 kHz.
    for sr in [44_100u32, 48_000] {
        let n = 2 * sr as usize;
        let mut noise = OttNoise(7);
        let mut x: Vec<f32> = (0..n).map(|_| (0.25 * noise.next()) as f32).collect();
        x[n / 3] = 0.9;
        let mut base_null: Option<(Vec<f32>, Vec<f32>)> = None;
        for block in BLOCKS {
            let mut unit = settled_ott_unit_at_block(sr, 0.0, true, block);
            let (l, r) = ott_unit_run(&mut unit, &x, &x, block);
            assert!(l.iter().chain(&r).all(|v| v.is_finite()), "sr={sr} block={block}: non-finite");
            if let Some((bl, br)) = &base_null {
                assert_bits_eq(&l, bl, &format!("sr={sr} block={block} unity null L"));
                assert_bits_eq(&r, br, &format!("sr={sr} block={block} unity null R"));
            } else {
                let (mut ap_lo, mut ap_hi) = (OttSection::allpass(LOW_SPLIT, sr as f64), OttSection::allpass(HIGH_SPLIT, sr as f64));
                let mut worst = 0.0f64;
                for (k, &v) in x.iter().enumerate() {
                    let want = ap_hi.tick(ap_lo.tick(v as f64)) as f32;
                    worst = worst.max((l[k] - want).abs() as f64).max((r[k] - want).abs() as f64);
                }
                let null_db = ott_db(worst / ott_peak(&x) as f64);
                assert!(null_db < -130.0, "sr={sr}: OttUnit null depth {null_db:.1} dB");
                base_null = Some((l, r));
            }
        }

        let fft_n = 1 << 17;
        let mut imp = vec![0.0f32; fft_n];
        imp[0] = 0.5;
        let mut base_flat: Option<Vec<f32>> = None;
        for block in BLOCKS {
            let mut flat_u = settled_ott_unit_at_block(sr, 0.0, true, block);
            let (fl, _) = ott_unit_run(&mut flat_u, &imp, &imp, block);
            if let Some(bf) = &base_flat {
                assert_bits_eq(&fl, bf, &format!("sr={sr} block={block} flatness impulse"));
            } else {
                let mut spectrum: Vec<Complex<f64>> = fl.iter().map(|&v| Complex::new(v as f64 / 0.5, 0.0)).collect();
                FftPlanner::new().plan_fft_forward(fft_n).process(&mut spectrum);
                let bin = |f: f64| (f * fft_n as f64 / sr as f64).round() as usize;
                let flat = (bin(20.0)..=bin(20000.0f64.min(0.45 * sr as f64)))
                    .map(|k| ott_db(spectrum[k].norm()).abs())
                    .fold(0.0, f64::max);
                assert!(flat < 0.05, "sr={sr}: OttUnit flatness {flat:.4} dB");
                base_flat = Some(fl);
            }
        }
    }

    // 2. Switch inside a fade turns from the weight reached; full-block ON/OFF fades reach `block`
    //    frames (so 480 and 1024 get distinct call patterns and a 1024-frame call carries an entire
    //    fade), fade lands on exact last frame (480), and off fade's last frame preserves dry bits
    //    including `-0.0` across all 7 block sizes.
    {
        let fade = (OTT_FADE_SECONDS * SR as f64) as usize;
        let step = 1.0 / fade as f64;

        // 2a. Fade reversal on outer `block`-sized calls over 2048 frames (t_on = 0, reverse at 160,
        //     inspect at 200, then run through the rest of the off-fade and settled tail so 480 and
        //     1024 get distinct call sequences and `max_call == block`).
        let rev_total = 2048usize;
        let sig_rev = vec![0.1f32; rev_total];
        let mut base_rev: Option<(Vec<f32>, Vec<f32>)> = None;
        let mut rev_patterns: Vec<(usize, Vec<usize>)> = Vec::new();
        for block in BLOCKS {
            let mut unit = settled_ott_unit_at_block(SR, DEFAULT_DEPTH, false, block);
            let mut ol = vec![0.0f32; rev_total];
            let mut or = vec![0.0f32; rev_total];
            let mut calls = Vec::new();
            let mut max_call = 0usize;
            assert!(assert_no_alloc(|| unit.set_bypass(&ott_cx(0), false)));
            let mut block_start = 0usize;
            while block_start < rev_total {
                let block_end = (block_start + block).min(rev_total);
                let mut pos = block_start;
                while pos < block_end {
                    let stop = if pos < 160 {
                        160
                    } else if pos < 200 {
                        200
                    } else {
                        rev_total
                    };
                    let end = block_end.min(stop);
                    let len = end - pos;
                    calls.push(len);
                    max_call = max_call.max(len);
                    assert_no_alloc(|| {
                        unit.process(
                            &ott_cx(pos as Frame),
                            [&sig_rev[pos..end], &sig_rev[pos..end]],
                            &[],
                            [&mut ol[pos..end], &mut or[pos..end]],
                        );
                    });
                    pos = end;
                    if pos == 160 {
                        assert!((unit.weight() - 160.0 * step).abs() < 1e-12, "block {block}");
                        assert!(assert_no_alloc(|| unit.set_bypass(&ott_cx(160), true)));
                    } else if pos == 200 {
                        assert!((unit.weight() - 120.0 * step).abs() < 1e-12, "block {block} weight {}", unit.weight());
                    }
                }
                block_start = block_end;
            }
            assert_eq!(unit.weight(), 0.0, "block {block}: reversed off-fade settled");
            assert_eq!(max_call, block, "fade reversal block {block}: full block size must reach OttUnit::process");
            let (bl, br) = base_rev.get_or_insert_with(|| (ol.clone(), or.clone()));
            assert_bits_eq(&ol, bl, &format!("fade reversal block {block} L"));
            assert_bits_eq(&or, br, &format!("fade reversal block {block} R"));
            rev_patterns.push((block, calls));
        }
        for i in 0..rev_patterns.len() {
            for j in (i + 1)..rev_patterns.len() {
                assert_ne!(
                    rev_patterns[i].1, rev_patterns[j].1,
                    "fade reversal must not feed identical call patterns to blocks {} and {}",
                    rev_patterns[i].0, rev_patterns[j].0
                );
            }
        }

        // 2b. Full-block uninterrupted ON/OFF fades (`[0, 1024)` ON fade, `[1024, 2048)` OFF fade
        //     with `-0.0` / `0.123` on the exact last fade frame `1024 + fade - 1`) followed by
        //     exact-frame weight edge checks (`[2048, 2048 + 2*fade)`), all split from outer
        //     `block`-sized calls so every block size reaches `block` frames *during* a fade and
        //     all 7 block sizes produce distinct call patterns.
        let edge_start = 2048usize;
        let fade_total = edge_start + 2 * fade;
        let mut in_l = vec![0.2f32; fade_total];
        let mut in_r = vec![0.2f32; fade_total];
        let off_last_full = 1024 + fade - 1;
        let off_last_edge = edge_start + 2 * fade - 1;
        in_l[off_last_full] = -0.0;
        in_r[off_last_full] = 0.123;
        in_l[off_last_edge] = -0.0;
        in_r[off_last_edge] = 0.123;

        let events = [
            1024usize,
            edge_start,
            edge_start + fade - 1,
            edge_start + fade,
            edge_start + 2 * fade - 1,
            fade_total,
        ];
        // Intervals where a fade is active: [0, fade), [1024, 1024 + fade), [edge_start, fade_total).
        let overlaps_fade = |a: usize, b: usize| -> bool {
            (a < fade && b > 0)
                || (a < 1024 + fade && b > 1024)
                || (a < fade_total && b > edge_start)
        };

        let mut base_fade: Option<(Vec<f32>, Vec<f32>)> = None;
        let mut fade_patterns: Vec<(usize, Vec<usize>)> = Vec::new();
        for block in BLOCKS {
            let mut u2 = settled_ott_unit_at_block(SR, DEFAULT_DEPTH, false, block);
            let mut fl = vec![0.0f32; fade_total];
            let mut fr = vec![0.0f32; fade_total];
            let mut calls = Vec::new();
            let mut max_fade_call = 0usize;
            assert!(assert_no_alloc(|| u2.set_bypass(&ott_cx(0), false)));
            let mut ei = 0usize;
            let mut block_start = 0usize;
            while block_start < fade_total {
                let block_end = (block_start + block).min(fade_total);
                let mut pos = block_start;
                while pos < block_end {
                    let next_ev = events[ei];
                    let end = block_end.min(next_ev);
                    let len = end - pos;
                    calls.push(len);
                    if overlaps_fade(pos, end) {
                        max_fade_call = max_fade_call.max(len);
                    }
                    assert_no_alloc(|| {
                        u2.process(
                            &ott_cx(pos as Frame),
                            [&in_l[pos..end], &in_r[pos..end]],
                            &[],
                            [&mut fl[pos..end], &mut fr[pos..end]],
                        );
                    });
                    pos = end;
                    if pos == next_ev {
                        match pos {
                            1024 => {
                                assert_eq!(u2.weight(), 1.0, "block {block}: full-block ON fade settled");
                                assert!(assert_no_alloc(|| u2.set_bypass(&ott_cx(pos as Frame), true)));
                            }
                            x if x == edge_start => {
                                assert_eq!(u2.weight(), 0.0, "block {block}: full-block OFF fade settled");
                                assert!(assert_no_alloc(|| u2.set_bypass(&ott_cx(pos as Frame), false)));
                            }
                            x if x == edge_start + fade - 1 => {
                                assert!(u2.weight() < 1.0, "block {block}: ON fade not settled at fade-1");
                            }
                            x if x == edge_start + fade => {
                                assert_eq!(u2.weight(), 1.0, "block {block}: ON fade lands on exact frame fade");
                                assert!(assert_no_alloc(|| u2.set_bypass(&ott_cx(pos as Frame), true)));
                            }
                            x if x == edge_start + 2 * fade - 1 => {
                                assert!(u2.weight() > 0.0, "block {block}: OFF fade not settled at fade-1");
                            }
                            x if x == fade_total => {
                                assert_eq!(u2.weight(), 0.0, "block {block}: OFF fade lands on exact frame fade");
                            }
                            _ => unreachable!(),
                        }
                        ei += 1;
                    }
                }
                block_start = block_end;
            }
            assert_eq!(ei, events.len(), "block {block}: all fade events ran");
            assert_eq!(max_fade_call, block, "fade edge block {block}: longest fade call must reach block size");
            assert_ne!(fl[off_last_full - 1].to_bits(), in_l[off_last_full - 1].to_bits(), "block {block}: full-block off fade still active one frame before end");
            assert_eq!(fl[off_last_full].to_bits(), (-0.0f32).to_bits(), "block {block}: full-block off fade last frame preserves -0.0");
            assert_eq!(fr[off_last_full].to_bits(), 0.123f32.to_bits(), "block {block}: full-block off fade last frame is dry bit for bit");
            assert_eq!(fl[off_last_edge].to_bits(), (-0.0f32).to_bits(), "block {block}: off fade last frame preserves -0.0");
            assert_eq!(fr[off_last_edge].to_bits(), 0.123f32.to_bits(), "block {block}: off fade last frame is dry bit for bit");
            let (bfl, bfr) = base_fade.get_or_insert_with(|| (fl.clone(), fr.clone()));
            assert_bits_eq(&fl, bfl, &format!("fade exact end block {block} L"));
            assert_bits_eq(&fr, bfr, &format!("fade exact end block {block} R"));
            fade_patterns.push((block, calls));
        }
        for i in 0..fade_patterns.len() {
            for j in (i + 1)..fade_patterns.len() {
                assert_ne!(
                    fade_patterns[i].1, fade_patterns[j].1,
                    "fade edge must not feed identical call patterns to blocks {} and {}",
                    fade_patterns[i].0, fade_patterns[j].0
                );
            }
        }
    }

    // 3. Silence, full-scale square (<= CEILING), near-denormal stay finite; 20 s decay -> exact 0.0.
    {
        let square: Vec<f32> = (0..2 * SR as usize).map(|k| if (k / 240) % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let tiny: Vec<f32> = (0..SR as usize).map(|k| if k % 2 == 0 { 1e-38 } else { -1e-40 }).collect();
        let silence = vec![0.0f32; 20 * SR as usize];
        let mut base_sq: Option<Vec<f32>> = None;
        let mut base_tiny: Option<Vec<f32>> = None;
        for block in BLOCKS {
            let mut unit = settled_ott_unit_at_block(SR, 1.0, true, block);
            for (name, x) in [("silence", &silence[..SR as usize]), ("square", &square[..]), ("denormal", &tiny[..]), ("decay", &silence[..])] {
                let (l, r) = ott_unit_run(&mut unit, x, x, block);
                assert!(l.iter().chain(&r).all(|v| v.is_finite()), "block {block} {name}: non-finite");
                assert!(ott_peak(&l).max(ott_peak(&r)) as f64 <= CEILING, "block {block} {name}: peak {}", ott_peak(&l));
                if name == "square" {
                    let bs = base_sq.get_or_insert_with(|| l.clone());
                    assert_bits_eq(&l, bs, &format!("square block {block}"));
                } else if name == "denormal" {
                    let bt = base_tiny.get_or_insert_with(|| l.clone());
                    assert_bits_eq(&l, bt, &format!("denormal block {block}"));
                }
            }
            let (l, r) = ott_unit_run(&mut unit, &[0.0; 128], &[0.0; 128], block);
            assert!(l.iter().chain(&r).all(|&v| v == 0.0), "block {block}: 20 s decay flushes to exact zero");
        }
    }

    // 4. Quiet-to-loud onset (< 3 dB overshoot) and -66 dBFS hiss (< 6 dB lift) at DEPTH 1.
    {
        let quiet = pluck::once(&[pluck::n(0.0, 52, 0.9), pluck::n(1.0, 55, 0.9), pluck::n(2.0, 57, 0.9), pluck::n(3.0, 59, 0.9)], 24000.0, SR, 2 * SR as usize, 0.01);
        let loud = pluck::once(&[pluck::accent(0.0, 40, 2.0)], 24000.0, SR, SR as usize, 0.708);
        let onset = quiet.len();
        let x: Vec<f32> = quiet.iter().chain(loud.iter()).copied().collect();
        let mut base_onset: Option<Vec<f32>> = None;
        for block in BLOCKS {
            let mut unit = settled_ott_unit_at_block(SR, 1.0, true, block);
            let (l, _) = ott_unit_run(&mut unit, &x, &x, block);
            let w = onset..onset + SR as usize / 20;
            let overshoot = ott_db(ott_peak(&l[w.clone()]) as f64) - ott_db(ott_peak(&x[w]) as f64);
            assert!(overshoot < 3.0, "block {block}: OttUnit onset overshoot {overshoot:+.1} dB");
            let bo = base_onset.get_or_insert_with(|| l.clone());
            assert_bits_eq(&l, bo, &format!("onset overshoot block {block}"));
        }

        let mut noise = OttNoise(5);
        let a = 10f64.powf(-66.0 / 20.0) * 3f64.sqrt();
        let hiss: Vec<f32> = (0..4 * SR as usize).map(|_| (a * noise.next()) as f32).collect();
        let mut base_hiss: Option<Vec<f32>> = None;
        for block in BLOCKS {
            let mut hu = settled_ott_unit_at_block(SR, 1.0, true, block);
            let (hl, _) = ott_unit_run(&mut hu, &hiss, &hiss, block);
            let lift = ott_db(ott_rms(&hl[2 * SR as usize..])) - ott_db(ott_rms(&hiss[2 * SR as usize..]));
            assert!(lift < 6.0, "block {block}: OttUnit hiss lift {lift:+.1} dB");
            let bh = base_hiss.get_or_insert_with(|| hl.clone());
            assert_bits_eq(&hl, bh, &format!("hiss lift block {block}"));
        }
    }

    // 5. DEPTH 0 unity, UP 0 no lift, DOWN 0 no cut, TIME scales every band's release (0.5x & 4x),
    //    and each configuration is bit-identical across all 7 `BLOCKS` sizes.
    {
        let riff_b: Vec<f32> = pluck::periodic(&pluck::RIFF_B, 12000.0, SR, 8.0, 0.9).iter().copied().collect();
        let (d0_gains, d0_l, d0_r) = ott_unit_band_gains(&riff_b, &[(InputSendParam::OttDepth, 0.0)]);
        for (k, g) in d0_gains.iter().enumerate() {
            assert_eq!(*g, [1.0; 3], "DEPTH 0 frame {k}");
        }
        for block in BLOCKS {
            let (l, r) = ott_unit_run_with_params(&riff_b, &[(InputSendParam::OttDepth, 0.0)], block);
            assert_bits_eq(&l, &d0_l, &format!("DEPTH 0 block {block} L"));
            assert_bits_eq(&r, &d0_r, &format!("DEPTH 0 block {block} R"));
        }

        let quiet_sine = ott_sine(1000.0, -50.0, 2 * SR as usize, SR);
        let settled_from = SR as usize;
        let up0_params = [(InputSendParam::OttDepth, 1.0), (InputSendParam::OttUpward, 0.0)];
        let (flat, flat_l, _) = ott_unit_band_gains(&quiet_sine, &up0_params);
        for (k, g) in flat.iter().enumerate().skip(settled_from) {
            assert_eq!(*g, [ott_makeup(1.0); 3], "UP 0 frame {k}");
        }
        let (lifted, lifted_l, _) = ott_unit_band_gains(&quiet_sine, &[(InputSendParam::OttDepth, 1.0)]);
        let mid_lift = lifted[settled_from..].iter().map(|g| g[1]).fold(f64::INFINITY, f64::min);
        assert!(ott_db(mid_lift / ott_makeup(1.0)) > 6.0, "UP 1 mid lift");
        for block in BLOCKS {
            let (l0, _) = ott_unit_run_with_params(&quiet_sine, &up0_params, block);
            let (l1, _) = ott_unit_run_with_params(&quiet_sine, &[(InputSendParam::OttDepth, 1.0)], block);
            assert_bits_eq(&l0, &flat_l, &format!("UP 0 block {block}"));
            assert_bits_eq(&l1, &lifted_l, &format!("UP 1 block {block}"));
        }

        let loud_sine = ott_sine(1000.0, -6.0, 2 * SR as usize, SR);
        let down0_params = [(InputSendParam::OttDepth, 1.0), (InputSendParam::OttDownward, 0.0)];
        let (flat_d, flat_d_l, _) = ott_unit_band_gains(&loud_sine, &down0_params);
        for (k, g) in flat_d.iter().enumerate().skip(settled_from) {
            assert!(g.iter().all(|&v| v >= ott_makeup(1.0)), "DOWN 0 frame {k}");
            assert_eq!(g[1], ott_makeup(1.0), "DOWN 0 mid frame {k}");
        }
        let (cut, cut_l, _) = ott_unit_band_gains(&loud_sine, &[(InputSendParam::OttDepth, 1.0)]);
        let mid_cut = cut[settled_from..].iter().map(|g| g[1]).fold(0.0, f64::max);
        assert!(ott_db(mid_cut / ott_makeup(1.0)) < -10.0, "DOWN 1 mid cut");
        for block in BLOCKS {
            let (l0, _) = ott_unit_run_with_params(&loud_sine, &down0_params, block);
            let (l1, _) = ott_unit_run_with_params(&loud_sine, &[(InputSendParam::OttDepth, 1.0)], block);
            assert_bits_eq(&l0, &flat_d_l, &format!("DOWN 0 block {block}"));
            assert_bits_eq(&l1, &cut_l, &format!("DOWN 1 block {block}"));
        }

        assert_eq!([release_scale(0.0), release_scale(0.5), release_scale(1.0)], [0.25, 1.0, 4.0]);
        for (name, freq, band) in [("low", 50.0, 0), ("mid", 1000.0, 1), ("high", 6000.0, 2)] {
            let step = SR as usize;
            let x: Vec<f32> = (0..7 * step)
                .map(|k| {
                    let amp = if k < step { 10f64.powf(-6.0 / 20.0) } else { 10f64.powf(-40.0 / 20.0) };
                    (amp * (2.0 * PI * freq * k as f64 / SR as f64).sin()) as f32
                })
                .collect();
            let mut ref_outs = Vec::new();
            let times = [0.25, 0.5, 1.0].map(|t| {
                let (gains, out_l, _) = ott_unit_band_gains(&x, &[(InputSendParam::OttDepth, 1.0), (InputSendParam::OttTime, t)]);
                ref_outs.push((t, out_l));
                let back = (step..x.len()).find(|&k| gains[k][band] >= ott_makeup(1.0)).expect("recovered");
                (back - step) as f64 / SR as f64
            });
            assert!(times[0] < times[1] && times[1] < times[2], "{name}: {times:?}");
            for (i, want) in [(0, 0.5), (2, 4.0)] {
                let ratio = times[i] / times[1];
                assert!((ratio / want - 1.0).abs() < 0.02, "{name}: ratio {ratio:.3} vs {want}");
            }
            for (t, want_l) in &ref_outs {
                for block in BLOCKS {
                    let (got_l, _) = ott_unit_run_with_params(&x, &[(InputSendParam::OttDepth, 1.0), (InputSendParam::OttTime, *t)], block);
                    assert_bits_eq(&got_l, want_l, &format!("TIME {name} t={t} block {block}"));
                }
            }
        }
    }

    // 6. Glided controls set while bypassed and settled take effect immediately on un-bypass.
    {
        let signal = ott_sine(220.0, -12.0, 4_800, SR);
        let silence = vec![0.0f32; SR as usize];
        let params = [
            (InputSendParam::OttDepth, 1.0),
            (InputSendParam::OttUpward, 0.3),
            (InputSendParam::OttDownward, 0.6),
            (InputSendParam::OttTime, 0.9),
        ];
        let cx0 = ott_cx(0);
        let mut base_out: Option<(Vec<f32>, Vec<f32>)> = None;
        for block in BLOCKS {
            let mut at_once = OttUnit::new(SR);
            assert!(assert_no_alloc(|| at_once.set_bypass(&cx0, true)));
            for (p, v) in params {
                assert_eq!(assert_no_alloc(|| at_once.set_param(&cx0, UnitParam::Send(p), v)), Some(v));
            }
            assert!(assert_no_alloc(|| at_once.set_bypass(&cx0, false)));
            let (a_l, a_r) = ott_unit_run(&mut at_once, &signal, &signal, block);

            let mut glided = OttUnit::new(SR);
            for (p, v) in params {
                assert_eq!(assert_no_alloc(|| glided.set_param(&cx0, UnitParam::Send(p), v)), Some(v));
            }
            let _ = ott_unit_run(&mut glided, &silence, &silence, block);
            assert!(assert_no_alloc(|| glided.set_bypass(&cx0, true)));
            let _ = ott_unit_run(&mut glided, &silence, &silence, block);
            assert_eq!(glided.weight(), 0.0);
            assert!(assert_no_alloc(|| glided.set_bypass(&cx0, false)));
            let (b_l, b_r) = ott_unit_run(&mut glided, &signal, &signal, block);
            assert_bits_eq(&a_l, &b_l, &format!("glided controls set while off block {block} L"));
            assert_bits_eq(&a_r, &b_r, &format!("glided controls set while off block {block} R"));
            let (bl, br) = base_out.get_or_insert_with(|| (a_l.clone(), a_r.clone()));
            assert_bits_eq(&a_l, bl, &format!("glided controls block {block} vs 1 L"));
            assert_bits_eq(&a_r, br, &format!("glided controls block {block} vs 1 R"));
        }
    }

    // 7. Non-finite controls dropped, out-of-range clamped, wrong-kind rejected.
    {
        let mut unit = OttUnit::new(SR);
        let cx0 = ott_cx(0);
        for (p, v) in [
            (InputSendParam::OttDepth, 0.8),
            (InputSendParam::OttTime, 0.7),
            (InputSendParam::OttUpward, 0.6),
            (InputSendParam::OttDownward, 0.4),
        ] {
            assert_eq!(assert_no_alloc(|| unit.set_param(&cx0, UnitParam::Send(p), v)), Some(v));
        }
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for p in [InputSendParam::OttDepth, InputSendParam::OttTime, InputSendParam::OttUpward, InputSendParam::OttDownward] {
                assert_eq!(assert_no_alloc(|| unit.set_param(&cx0, UnitParam::Send(p), bad)), None);
            }
        }
        for wrong in [
            UnitParam::Send(InputSendParam::EchoLevel),
            UnitParam::Send(InputSendParam::ReverbLevel),
            UnitParam::Send(InputSendParam::RingFreq),
            UnitParam::Instrument,
        ] {
            assert_eq!(assert_no_alloc(|| unit.set_param(&cx0, wrong, 0.5)), None);
        }
        assert_eq!(unit.controls(), [0.8, 0.7, 0.6, 0.4]);
        for p in [InputSendParam::OttDepth, InputSendParam::OttTime, InputSendParam::OttUpward, InputSendParam::OttDownward] {
            assert_eq!(assert_no_alloc(|| unit.set_param(&cx0, UnitParam::Send(p), 3.0)), Some(1.0));
        }
        assert_eq!(unit.controls(), [1.0; 4]);
        for p in [InputSendParam::OttDepth, InputSendParam::OttTime, InputSendParam::OttUpward, InputSendParam::OttDownward] {
            assert_eq!(assert_no_alloc(|| unit.set_param(&cx0, UnitParam::Send(p), -1.0)), Some(0.0));
        }
        assert_eq!(unit.controls(), [0.0; 4]);
    }
}

struct SquareSource {
    start: Frame,
    half: Frame,
    amp: f32,
}

impl UnitProcessor for SquareSource {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: true, latency: 0 }
    }

    fn process(&mut self, cx: &UnitCx, _input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let (frame, [ol, or]) = (cx.frame, out);
        for (k, y) in ol.iter_mut().enumerate() {
            let f = frame + k as Frame;
            *y = if f < self.start {
                0.0
            } else if ((f - self.start) / self.half) % 2 == 0 {
                self.amp
            } else {
                -self.amp
            };
        }
        or.copy_from_slice(ol);
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// §5 item 5 (engine half): `tests/ott.rs`'s engine cases run on `OttUnit` seated in a slot chain
/// across all 7 block sizes, including linked stereo detection, `ToggleUnit`, same-frame command
/// ordering, cold/active bypass, first-frame un-bypass fade, ceiling on hard square onsets, zero
/// added latency, and loops/click isolation.
#[test]
fn ott_unit_in_engine_matches_ott_suite_across_block_sizes() {
    const PHYS: Frame = 1123;
    const PLUGIN: Frame = 57;
    const LIMITER: Frame = 288;
    const PRESS: Frame = 24_000;
    let riff_buf = Arc::new(pluck::periodic(&pluck::RIFF_A, 12000.0, SR, 8.0, 0.3));

    // 1. What is heard is what is recorded (`[Delay(57), OttUnit]`): take == monitor, latency == 57,
    //    bit-identical across all 7 `BLOCKS` sizes.
    {
        let run_take = |active: bool, depth: f64, block: usize| {
            let mut rig = Rig::with(Opts { align: PHYS + LIMITER, block, ..Opts::default() });
            assert!(rig.port(0).install_at(1, 1, 0, false, None, Box::new(Delay::new(PLUGIN))).is_ok());
            assert!(rig.port(0).install_at(2, 2, 1, true, None, Box::new(OttUnit::new(SR))).is_ok());
            let r = riff_buf.clone();
            rig.set_input(move |f| r[f as usize % r.len()]);
            let t0 = rig.frame;
            rig.keep_output();
            if active {
                rig.send_at(t0 + 1_000, Command::SetUnitParam(2, UnitParam::Send(InputSendParam::OttDepth), depth));
                rig.send_at(t0 + 1_001, Command::SetUnitBypass(2, false));
            }
            rig.send_at(t0, Command::SetFixedLength(true));
            rig.send_at(t0, Command::SetFixedBars(1.0));
            rig.send_at(t0 + PRESS, Command::RecDub(0));
            rig.advance_to(t0 + PRESS + 1);
            let start = rig.start_frame();
            let downbeat = t0 + PRESS + 4 * 24_000;
            assert_eq!(start, downbeat + PHYS + PLUGIN + LIMITER, "block {block}: OttUnit adds no alignment latency");
            rig.advance_to(rig.end_frame() + 1);
            let pcm = rig.pcm(0);
            let s = (start - t0) as usize;
            let heard = rig.monitor[s..s + pcm.len()].to_vec();
            drain_returns(&mut rig);
            (pcm, heard)
        };
        let mut base_off: Option<Vec<f32>> = None;
        let mut base_on: [Option<Vec<f32>>; 2] = [None, None];
        for block in BLOCKS {
            let (off_pcm, off_heard) = run_take(false, DEFAULT_DEPTH, block);
            assert_eq!(off_pcm, off_heard, "block {block}: bypassed take == heard");
            let bo = base_off.get_or_insert_with(|| off_pcm.clone());
            assert_bits_eq(&off_pcm, bo, &format!("take bypassed block {block}"));
            for (di, depth) in [DEFAULT_DEPTH, 1.0].into_iter().enumerate() {
                let (on_pcm, on_heard) = run_take(true, depth, block);
                assert_eq!(on_pcm, on_heard, "block {block} DEPTH {depth}: take == heard");
                assert_ne!(on_pcm, off_pcm, "block {block} DEPTH {depth}: take carries OTT");
                let b = base_on[di].get_or_insert_with(|| on_pcm.clone());
                assert_bits_eq(&on_pcm, b, &format!("take DEPTH {depth} block {block}"));
            }
        }
    }

    // 2. Scripted controls + mid-fade reversal on distinct stereo L/R input: seated `OttUnit`
    //    matches standalone `StereoOtt` bit for bit across all 7 block sizes AND proves linked
    //    stereo detectors (R follows L's loud tone, differing sharply from independent mono OTTs).
    {
        let frames = 72_000usize;
        let fade = (OTT_FADE_SECONDS * SR as f64) as Frame;
        // Loud 1 kHz sine on L (-6 dBFS), quiet 1 kHz sine on R (-46 dBFS): linked mid detector cuts R!
        let stereo_sig = |f: Frame| -> Pair {
            let k = (f - F0) as usize;
            let a_l = if k % 24_000 < 16_000 { 0.5 } else { 0.005 };
            let a_r = 0.005;
            let s = fdlibm::sin(std::f64::consts::TAU * 1000.0 * k as f64 / SR as f64);
            [(a_l * s) as f32, (a_r * s) as f32]
        };
        let script: Vec<(Frame, Command)> = vec![
            (1_001, Command::SetUnitBypass(1, false)),
            (9_333, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDepth), 1.0)),
            (15_013, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttTime), 0.8)),
            (21_017, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttUpward), 0.6)),
            (27_011, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDownward), 0.3)),
            (40_077, Command::SetUnitBypass(1, true)),
            (40_077 + fade / 3, Command::SetUnitBypass(1, false)),
            (50_005, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDepth), 0.2)),
        ];
        // Reference from standalone StereoOtt.
        let mut standalone = StereoOtt::new(SR);
        let mut unlinked_r = lf_engine::dsp::ott::MonoOtt::new(SR);
        let mut want_l = Vec::with_capacity(frames);
        let mut want_r = Vec::with_capacity(frames);
        let mut mono_r = Vec::with_capacity(frames);
        for k in 0..frames {
            for &(_, cmd) in script.iter().filter(|(at, _)| *at as usize == k) {
                let s = match cmd {
                    Command::SetUnitBypass(1, b) => Setting::On(!b),
                    Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDepth), v) => Setting::Depth(v),
                    Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttTime), v) => Setting::Time(v),
                    Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttUpward), v) => Setting::Upward(v),
                    Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDownward), v) => Setting::Downward(v),
                    _ => unreachable!(),
                };
                assert_no_alloc(|| {
                    standalone.set(s);
                    unlinked_r.set(s);
                });
            }
            let [l, r] = stereo_sig(F0 + k as Frame);
            let ([ol, or], mr) = assert_no_alloc(|| (standalone.frame([l, r]), unlinked_r.frame([r])[0]));
            want_l.push(ol);
            want_r.push(or);
            mono_r.push(mr);
        }
        // Confirm the test signal actually distinguishes linked stereo detection from unlinked mono.
        assert!(max_abs_diff(&want_r, &mono_r) > 0.005, "linked detector must differ from independent mono on R");

        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            rig.set_stereo_inputs(stereo_sig, |_| [0.0, 0.0]);
            assert!(rig.port(0).install_at(1, 1, 0, true, None, Box::new(OttUnit::new(SR))).is_ok());
            for &(at, cmd) in &script {
                rig.send_at(F0 + at, cmd);
            }
            rig.keep_output();
            rig.advance(frames as Frame);
            assert_bits_eq(&rig.monitor, &want_l, &format!("ott scripted block {block} monitor L"));
            assert_bits_eq(&rig.monitor_right, &want_r, &format!("ott scripted block {block} monitor R"));
            assert_bits_eq(&rig.record, &want_l, &format!("ott scripted block {block} record L"));
            assert_bits_eq(&rig.record_right, &want_r, &format!("ott scripted block {block} record R"));
            drain_returns(&mut rig);
        }
    }

    // 3. Stamped `SetUnitBypass` and `ToggleUnit` inside a block switch `OttUnit` on their exact
    //    frame across all 7 `BLOCKS` sizes (`at = 1_019` lands 5 frames before the end of the first
    //    1024-frame call, so a late-landing setter at 1024 fails both `first_diff` and bit equality).
    {
        let at: Frame = 1_019;
        let frames = 24_000;
        let run_toggle = |block: usize, cmd: Option<Command>| {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            let r = riff_buf.clone();
            rig.set_input(move |f| r[f as usize % r.len()]);
            assert!(rig.port(0).install_at(1, 1, 0, true, None, Box::new(OttUnit::new(SR))).is_ok());
            if let Some(c) = cmd {
                rig.send_at(F0 + at, c);
            }
            rig.keep_output();
            rig.advance(frames);
            let mon = rig.monitor.clone();
            drain_returns(&mut rig);
            mon
        };
        let mut base_off: Option<Vec<f32>> = None;
        let mut base_setter: Option<Vec<f32>> = None;
        for block in BLOCKS {
            let off = run_toggle(block, None);
            let bo = base_off.get_or_insert_with(|| off.clone());
            assert_bits_eq(&off, bo, &format!("off block {block} vs 1"));
            let setter = run_toggle(block, Some(Command::SetUnitBypass(1, false)));
            let first_diff = setter.iter().zip(&off).position(|(a, b)| a.to_bits() != b.to_bits());
            assert_eq!(first_diff, Some(at as usize), "block {block}: SetUnitBypass switches on its exact frame");
            let bs = base_setter.get_or_insert_with(|| setter.clone());
            assert_bits_eq(&setter, bs, &format!("SetUnitBypass block {block} vs 1"));
            let toggled = run_toggle(block, Some(Command::Action(Action::ToggleUnit(1))));
            assert_bits_eq(&toggled, &setter, &format!("ToggleUnit block {block} matches SetUnitBypass"));
        }
    }

    // 4. Bypassed `OttUnit` (cold bypassed with params moved, and active -> bypassed settled from
    //    fade's last frame) leaves output dry bit for bit; un-bypass on frame 0 after bypassed
    //    install (idle and non-idle) fades over 10 ms (never snaps).
    {
        let fade = (OTT_FADE_SECONDS * SR as f64) as usize;
        let mut dry_rig = Rig::new();
        let r = riff_buf.clone();
        dry_rig.set_input(move |f| r[f as usize % r.len()]);
        dry_rig.keep_output();
        dry_rig.advance(30_000);
        let dry = dry_rig.monitor.clone();

        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            let r = riff_buf.clone();
            rig.set_input(move |f| r[f as usize % r.len()]);
            assert!(rig.port(0).install_at(1, 1, 0, true, None, Box::new(OttUnit::new(SR))).is_ok());
            rig.send_at(F0 + 5, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDepth), 1.0));
            rig.send_at(F0 + 7_001, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttTime), 0.9));
            rig.send_at(F0 + 8_003, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttUpward), 0.1));
            rig.send_at(F0 + 9_000, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDownward), 0.0));
            rig.keep_output();
            rig.advance(30_000);
            assert_bits_eq(&rig.monitor, &dry, &format!("cold bypassed ott params moved block {block}"));
            assert_bits_eq(&rig.record, &dry, &format!("cold bypassed ott record block {block}"));
            drain_returns(&mut rig);
        }

        // Active -> bypass settles on exact last frame (`20_000 + fade - 1`) across all 7 block sizes.
        let mut base_active_bypass: Option<Vec<f32>> = None;
        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            let r = riff_buf.clone();
            rig.set_input(move |f| r[f as usize % r.len()]);
            assert!(rig.port(0).install_at(1, 1, 0, true, None, Box::new(OttUnit::new(SR))).is_ok());
            rig.send_at(F0 + 1_000, Command::SetUnitBypass(1, false));
            rig.send_at(F0 + 20_000, Command::SetUnitBypass(1, true));
            rig.keep_output();
            rig.advance(30_000);
            assert_eq!(rig.monitor.iter().zip(&dry).position(|(a, b)| a.to_bits() != b.to_bits()), Some(1_000), "block {block}");
            assert_bits_eq(&rig.monitor[20_000 + fade - 1..], &dry[20_000 + fade - 1..], &format!("active -> bypassed block {block} settles bit for bit"));
            let b = base_active_bypass.get_or_insert_with(|| rig.monitor.clone());
            assert_bits_eq(&rig.monitor, b, &format!("active -> bypassed block {block} vs 1"));
            drain_returns(&mut rig);
        }

        // Un-bypass on frame 0 after a bypassed install fades over 10 ms (matching `InputSend::Ott`).
        let mut srig = Rig::new();
        srig.set_level(0.3);
        srig.send_at(F0, Command::SetInputSend(InputSend::Ott, true));
        srig.keep_output();
        srig.advance(2_000);
        let want = srig.monitor.clone();
        for idle_install in [false, true] {
            for block in BLOCKS {
                let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
                rig.set_level(0.3);
                assert!(rig.port(0).install_at(1, 1, 0, true, None, Box::new(OttUnit::new(SR))).is_ok());
                if idle_install {
                    rig.engine.service_slots_idle();
                }
                rig.send_at(F0, Command::SetUnitBypass(1, false));
                rig.keep_output();
                rig.advance(2_000);
                assert_bits_eq(&rig.monitor, &want, &format!("ott first-frame unbypass idle={idle_install} block {block}"));
                // Frame 0 is at weight 1/480, far from snapped weight 1.0.
                assert!((rig.monitor[0] - 0.3).abs() < 0.01, "idle={idle_install} block {block}: must fade, not snap");
                drain_returns(&mut rig);
            }
        }
    }

    // 5. Every control lands on its exact frame, AND same-frame `SetUnitParam(OttDepth, 1.0)` +
    //    `SetUnitBypass(false)` in BOTH ring orders matches `InputSend::Ott` bit for bit (and the
    //    two orders differ from each other!).
    {
        let at = 30_077usize;
        let run_ctrl = |block: usize, extra: Option<Command>| {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            let r = riff_buf.clone();
            rig.set_input(move |f| r[f as usize % r.len()]);
            assert!(rig.port(0).install_at(1, 1, 0, true, None, Box::new(OttUnit::new(SR))).is_ok());
            rig.send_at(F0 + 1_000, Command::SetUnitBypass(1, false));
            if let Some(c) = extra {
                rig.send_at(F0 + at as Frame, c);
            }
            rig.keep_output();
            rig.advance(78_000);
            let mon = rig.monitor.clone();
            drain_returns(&mut rig);
            mon
        };
        let base = run_ctrl(128, None);
        for (name, cmd) in [
            ("DEPTH", Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDepth), 1.0)),
            ("TIME", Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttTime), 1.0)),
            ("UP", Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttUpward), 0.0)),
            ("DOWN", Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDownward), 0.0)),
        ] {
            let mut ref_moved: Option<Vec<f32>> = None;
            for block in BLOCKS {
                let moved = run_ctrl(block, Some(cmd));
                let first = moved.iter().zip(&base).position(|(a, b)| a.to_bits() != b.to_bits());
                if name == "DEPTH" {
                    assert_eq!(first, Some(at), "block {block}: DEPTH lands on its exact frame");
                } else {
                    assert!(first.is_some_and(|k| (at..at + SR as usize).contains(&k)), "block {block} {name}: landed at {first:?}");
                }
                let rm = ref_moved.get_or_insert_with(|| moved.clone());
                assert_bits_eq(&moved, rm, &format!("ott control {name} block {block}"));
            }
        }

        // Same-frame param + un-bypass in both ring orders.
        let mut order_outputs: Vec<Vec<f32>> = Vec::new();
        for param_first in [true, false] {
            let mut srig = Rig::new();
            let r = riff_buf.clone();
            srig.set_input(move |f| r[f as usize % r.len()]);
            let t = F0 + 1_001;
            if param_first {
                srig.send_at(t, Command::SetInputSendParam(InputSendParam::OttDepth, 1.0));
                srig.send_at(t, Command::SetInputSend(InputSend::Ott, true));
            } else {
                srig.send_at(t, Command::SetInputSend(InputSend::Ott, true));
                srig.send_at(t, Command::SetInputSendParam(InputSendParam::OttDepth, 1.0));
            }
            srig.keep_output();
            srig.advance(6_000);
            let want = srig.monitor.clone();
            order_outputs.push(want.clone());

            for block in BLOCKS {
                let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
                let r = riff_buf.clone();
                rig.set_input(move |f| r[f as usize % r.len()]);
                assert!(rig.port(0).install_at(1, 1, 0, true, None, Box::new(OttUnit::new(SR))).is_ok());
                if param_first {
                    rig.send_at(t, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDepth), 1.0));
                    rig.send_at(t, Command::SetUnitBypass(1, false));
                } else {
                    rig.send_at(t, Command::SetUnitBypass(1, false));
                    rig.send_at(t, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDepth), 1.0));
                }
                rig.keep_output();
                rig.advance(6_000);
                assert_bits_eq(&rig.monitor, &want, &format!("same-frame ott param_first={param_first} block {block}"));
                drain_returns(&mut rig);
            }
        }
        assert!(max_abs_diff(&order_outputs[0], &order_outputs[1]) > 1e-3, "same-frame param_first vs bypass_first must differ");
    }

    // 6. Hard square onset through `OttUnit` leaves device & Share <= -0.3 dBFS and monitor <= CEILING
    //    across all 7 `BLOCKS` sizes, bit-identical across block sizes.
    {
        for (amp, freq) in [(0.8f32, 100.0), (0.8, 200.0), (1.0, 200.0)] {
            for depth in [0.0, 0.5, 1.0] {
                let half = (SR as f64 / (2.0 * freq)).round() as Frame;
                let mut base_heard: Option<Vec<f32>> = None;
                let mut base_mon: Option<Vec<f32>> = None;
                for block in BLOCKS {
                    let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
                    rig.set(Command::SetMetronome(false));
                    let start = rig.frame + rig.seconds(1.0);
                    assert!(rig.port(1).install_at(1, 1, 0, false, None, Box::new(SquareSource { start, half, amp })).is_ok());
                    assert!(rig.port(1).install_at(2, 2, 1, false, None, Box::new(OttUnit::new(SR))).is_ok());
                    rig.set(Command::SetUnitParam(2, UnitParam::Send(InputSendParam::OttDepth), depth));
                    rig.advance(rig.seconds(0.5));
                    rig.keep_output();
                    rig.advance(rig.seconds(2.5));
                    let dev = ott_peak(&rig.heard).max(ott_peak(&rig.heard_right)) as f64;
                    let share = ott_peak(&rig.share).max(ott_peak(&rig.share_right)) as f64;
                    assert!(ott_db(dev) <= -0.3, "block {block} square ±{amp} {freq} Hz DEPTH {depth}: device {:+.2} dBFS", ott_db(dev));
                    assert!(ott_db(share) <= -0.3, "block {block} square ±{amp} {freq} Hz DEPTH {depth}: share {:+.2} dBFS", ott_db(share));
                    let bh = base_heard.get_or_insert_with(|| rig.heard.clone());
                    assert_bits_eq(&rig.heard, bh, &format!("square ±{amp} {freq} Hz DEPTH {depth} block {block} heard"));
                    drain_returns(&mut rig);

                    let mut lrig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
                    lrig.set(Command::SetMetronome(false));
                    assert!(lrig.port(0).install_at(1, 1, 0, false, None, Box::new(OttUnit::new(SR))).is_ok());
                    lrig.set(Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDepth), depth));
                    let lstart = lrig.frame + lrig.seconds(1.0);
                    lrig.set_input(move |f| if f < lstart { 0.0 } else if ((f - lstart) / half) % 2 == 0 { amp } else { -amp });
                    lrig.keep_output();
                    lrig.advance(lrig.seconds(3.0));
                    let mon = ott_peak(&lrig.monitor).max(ott_peak(&lrig.monitor_right)) as f64;
                    assert!(mon <= CEILING, "block {block} live square ±{amp} {freq} Hz DEPTH {depth}: monitor {:+.2} dBFS", ott_db(mon));
                    let bm = base_mon.get_or_insert_with(|| lrig.monitor.clone());
                    assert_bits_eq(&lrig.monitor, bm, &format!("live square ±{amp} {freq} Hz DEPTH {depth} block {block} monitor"));
                    drain_returns(&mut lrig);
                }
            }
        }
    }

    // 7. Zero added latency samples (impulse & pluck arrive on exact same frame) and loops/click
    //    never pass `OttUnit`, bit-identical across all 7 `BLOCKS` sizes.
    {
        const IMPULSE: usize = 20_000;
        const PLUCK: usize = 50_000;
        let start = F0 + PRESS + 4 * 24_000 + PHYS + LIMITER;
        let note = pluck::once(&[pluck::accent(0.0, 52, 2.0)], 24000.0, SR, SR as usize, 0.3);
        let input = Rc::new(move |f: Frame| {
            let k = f - start;
            let imp = if k == IMPULSE as Frame { 0.5 } else { 0.0 };
            let p = if k >= PLUCK as Frame { note.get((k - PLUCK as Frame) as usize).copied().unwrap_or(0.0) } else { 0.0 };
            imp + p
        });
        let first_over = |x: &[f32], from: usize, len: usize| -> usize {
            x[from..from + len].iter().position(|v| v.abs() > 0.001).expect("arrived")
        };
        let mut base_lat_pcm: [Option<Vec<f32>>; 2] = [None, None];
        for block in BLOCKS {
            let mut seen = Vec::new();
            for (ai, active) in [false, true].into_iter().enumerate() {
                let mut rig = Rig::with(Opts { align: PHYS + LIMITER, block, ..Opts::default() });
                assert!(rig.port(0).install_at(1, 1, 0, !active, None, Box::new(OttUnit::new(SR))).is_ok());
                let inp = input.clone();
                rig.set_input(move |f| inp(f));
                let t0 = rig.frame;
                rig.keep_output();
                rig.send_at(t0, Command::SetFixedLength(true));
                rig.send_at(t0, Command::SetFixedBars(1.0));
                rig.send_at(t0 + PRESS, Command::RecDub(0));
                rig.advance_to(t0 + PRESS + 1);
                assert_eq!(rig.start_frame(), start, "block {block}");
                rig.advance_to(rig.end_frame() + 1);
                let pcm = rig.pcm(0);
                let s = (start - t0) as usize;
                seen.push([
                    first_over(&rig.monitor, s + IMPULSE, 4_800),
                    first_over(&pcm, IMPULSE, 4_800),
                    first_over(&rig.monitor, s + PLUCK, 9_600),
                    first_over(&pcm, PLUCK, 9_600),
                ]);
                let bp = base_lat_pcm[ai].get_or_insert_with(|| pcm.clone());
                assert_bits_eq(&pcm, bp, &format!("zero-latency active={active} block {block}"));
                drain_returns(&mut rig);
            }
            assert_eq!(seen[0], seen[1], "block {block}: OttUnit adds no latency samples");
        }

        // Loops and click never pass OttUnit across all 7 `BLOCKS` sizes.
        let count_in = |active: bool, block: usize| {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            assert!(rig.port(0).install_at(1, 1, 0, !active, None, Box::new(OttUnit::new(SR))).is_ok());
            rig.set(Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDepth), 1.0));
            rig.keep_output();
            rig.press(Command::RecDub(0));
            rig.advance(rig.seconds(1.5));
            let heard = rig.heard.clone();
            drain_returns(&mut rig);
            heard
        };
        let mut base_click: Option<Vec<f32>> = None;
        for block in BLOCKS {
            let (off_c, on_c) = (count_in(false, block), count_in(true, block));
            assert!(ott_peak(&off_c) > 0.05, "block {block}");
            assert_bits_eq(&on_c, &off_c, &format!("click never passes OttUnit block {block}"));
            let bc = base_click.get_or_insert_with(|| on_c.clone());
            assert_bits_eq(&on_c, bc, &format!("click block {block} vs 1"));
        }

        let looping = |active: bool, block: usize| {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            let r = riff_buf.clone();
            rig.set_input(move |f| r[f as usize % r.len()]);
            rig.record_first_take(0, 1, 2400);
            rig.set_level(0.0);
            rig.idle();
            rig.advance(rig.seconds(1.0));
            assert!(rig.port(0).install_at(1, 1, 0, !active, None, Box::new(OttUnit::new(SR))).is_ok());
            rig.set(Command::SetUnitParam(1, UnitParam::Send(InputSendParam::OttDepth), 1.0));
            rig.keep_output();
            rig.advance(rig.master());
            let out = (rig.heard.clone(), rig.heard_right.clone());
            drain_returns(&mut rig);
            out
        };
        let mut base_loop: Option<(Vec<f32>, Vec<f32>)> = None;
        for block in BLOCKS {
            let (off_l, on_l) = (looping(false, block), looping(true, block));
            assert!(ott_peak(&off_l.0) > 0.1, "block {block}");
            assert_bits_eq(&on_l.0, &off_l.0, &format!("loop L never passes OttUnit block {block}"));
            assert_bits_eq(&on_l.1, &off_l.1, &format!("loop R never passes OttUnit block {block}"));
            let bl = base_loop.get_or_insert_with(|| on_l.clone());
            assert_bits_eq(&on_l.0, &bl.0, &format!("loop L block {block} vs 1"));
            assert_bits_eq(&on_l.1, &bl.1, &format!("loop R block {block} vs 1"));
        }
    }
}

/// §5 item 5 (`bands` meter): `OttUnit::take_meter` matches `StereoOtt`'s own gains (dB) and linked
/// followers (dBFS, floor -120) sample by sample and over folded windows (including reset and
/// block-size invariance across all 7 block sizes), under `assert_no_alloc`.
#[test]
fn ott_unit_band_meter_matches_processor_sample_by_sample_and_folded() {
    let mut bands = [BandMeter::default(); 3];

    // 1. Non-OTT units report `false` from `take_meter` (under `assert_no_alloc`).
    let mut echo = EchoUnit::new(SR);
    let mut ring = RingUnit::new(SR);
    let mut reverb = new_reverb_unit();
    let x = [0.2f32; 64];
    let mut ol = [0.0f32; 64];
    let mut or = [0.0f32; 64];
    assert_no_alloc(|| echo.process(&ott_cx(0), [&x, &x], &[], [&mut ol, &mut or]));
    assert_no_alloc(|| ring.process(&ott_cx(0), [&x, &x], &[], [&mut ol, &mut or]));
    assert_no_alloc(|| reverb.process(&ott_cx(0), [&x, &x], &[], [&mut ol, &mut or]));
    assert!(!assert_no_alloc(|| echo.take_meter(&mut bands)), "EchoUnit has no band meter");
    assert!(!assert_no_alloc(|| ring.take_meter(&mut bands)), "RingUnit has no band meter");
    assert!(!assert_no_alloc(|| reverb.take_meter(&mut bands)), "ReverbUnit has no band meter");

    // 2. Fresh OttUnit with 0 samples processed reports `false`.
    let mut fresh = OttUnit::new(SR);
    assert!(!assert_no_alloc(|| fresh.take_meter(&mut bands)), "OttUnit with 0 samples reports false");

    // Helper matching the contract's dBFS floor (-120 dBFS at env <= 1e-6).
    let env_to_db = |e: f64| -> f32 {
        if e <= 1e-6 { BAND_LEVEL_FLOOR_DB } else { (20.0 * e.log10()) as f32 }
    };
    let gain_to_db = |g: f64| -> f32 { (20.0 * g.log10()) as f32 };
    let assert_meter_bits_eq = |got: &[BandMeter; 3], want: &[BandMeter; 3], what: &str| {
        for b in 0..3 {
            assert_eq!(got[b].gain.to_bits(), want[b].gain.to_bits(), "{what} band {b} gain: got {} vs {}", got[b].gain, want[b].gain);
            assert_eq!(got[b].gain_min.to_bits(), want[b].gain_min.to_bits(), "{what} band {b} gain_min: got {} vs {}", got[b].gain_min, want[b].gain_min);
            assert_eq!(got[b].gain_max.to_bits(), want[b].gain_max.to_bits(), "{what} band {b} gain_max: got {} vs {}", got[b].gain_max, want[b].gain_max);
            assert_eq!(got[b].level.to_bits(), want[b].level.to_bits(), "{what} band {b} level: got {} vs {}", got[b].level, want[b].level);
            assert_eq!(got[b].level_peak.to_bits(), want[b].level_peak.to_bits(), "{what} band {b} level_peak: got {} vs {}", got[b].level_peak, want[b].level_peak);
        }
    };

    // Build a dynamic stereo signal hitting all 3 bands (55 Hz low, 900 Hz mid, 6 kHz high) with
    // silence, loud attack, decay, and asymmetric L/R so linked followers and min/max ranges move.
    let n = 4096usize;
    let mut sig_l = Vec::with_capacity(n);
    let mut sig_r = Vec::with_capacity(n);
    for k in 0..n {
        let t = k as f64 / SR as f64;
        let (al, ar) = if k < 256 {
            (0.0, 0.0) // pure silence -> tests -120 dBFS floor
        } else if k < 1024 {
            (0.7, 0.05) // loud L attack -> linked follower driven by L
        } else if k < 2048 {
            (0.002, 0.002) // quiet decay -> gain rises, follower falls
        } else if k < 2600 {
            (0.05, 0.8) // loud R burst -> linked follower driven by R
        } else {
            (0.0, 0.0) // release tail
        };
        let wave_l = (std::f64::consts::TAU * 55.0 * t).sin() * 0.4
            + (std::f64::consts::TAU * 900.0 * t).sin() * 0.4
            + (std::f64::consts::TAU * 6000.0 * t).sin() * 0.4;
        let wave_r = (std::f64::consts::TAU * 60.0 * t).sin() * 0.4
            + (std::f64::consts::TAU * 1100.0 * t).sin() * 0.4
            + (std::f64::consts::TAU * 5500.0 * t).sin() * 0.4;
        sig_l.push((al * wave_l) as f32);
        sig_r.push((ar * wave_r) as f32);
    }

    // 3. Sample-by-sample equality (`block = 1`, `take_meter` every sample) against `StereoOtt`.
    let mut ref_ott = StereoOtt::new(SR);
    assert_no_alloc(|| {
        ref_ott.set(Setting::Depth(1.0));
        ref_ott.set(Setting::Time(0.0)); // fast release so gains/followers move noticeably inside windows
        ref_ott.set(Setting::On(true));
    });
    // Settle the on-fade over 480 frames of silence on both processors so `OttUnit` and `StereoOtt` match.
    let mut unit = OttUnit::new(SR);
    assert!(assert_no_alloc(|| unit.set_bypass(&ott_cx(0), true)));
    assert_eq!(assert_no_alloc(|| unit.set_param(&ott_cx(0), UnitParam::Send(InputSendParam::OttDepth), 1.0)), Some(1.0));
    assert_eq!(assert_no_alloc(|| unit.set_param(&ott_cx(0), UnitParam::Send(InputSendParam::OttTime), 0.0)), Some(0.0));
    assert!(assert_no_alloc(|| unit.set_bypass(&ott_cx(0), false)));

    let mut per_sample_gains = Vec::with_capacity(n);
    let mut per_sample_envs = Vec::with_capacity(n);
    let mut s_ol = [0.0f32; 1];
    let mut s_or = [0.0f32; 1];
    for k in 0..n {
        let [rl, rr] = assert_no_alloc(|| ref_ott.frame([sig_l[k], sig_r[k]]));
        let rg = ref_ott.gains();
        let re = ref_ott.env();
        per_sample_gains.push(rg);
        per_sample_envs.push(re);

        let sl = [sig_l[k]];
        let sr = [sig_r[k]];
        assert_no_alloc(|| unit.process(&ott_cx(k as Frame), [&sl, &sr], &[], [&mut s_ol, &mut s_or]));
        assert_eq!(s_ol[0].to_bits(), rl.to_bits(), "sample {k} out L");
        assert_eq!(s_or[0].to_bits(), rr.to_bits(), "sample {k} out R");
        assert!(assert_no_alloc(|| unit.take_meter(&mut bands)), "sample {k} take_meter");
        let want: [BandMeter; 3] = std::array::from_fn(|b| {
            let g = gain_to_db(rg[b]);
            let lv = env_to_db(re[b]);
            BandMeter { gain: g, gain_min: g, gain_max: g, level: lv, level_peak: lv }
        });
        assert_meter_bits_eq(&bands, &want, &format!("sample {k}"));
        // Immediate second take with no intervening samples returns false (fold was reset).
        assert!(!assert_no_alloc(|| unit.take_meter(&mut bands)), "sample {k}: second take must return false");
    }
    // Check silence window floored cleanly at -120 dBFS.
    assert!(per_sample_envs[0..256].iter().all(|e| *e == [0.0; 3]));

    // 4. Multi-sample window folding (4 ms = 192 frames, and one wide window spanning attack + decay
    //    so `gain_min < gain < gain_max` and `level < level_peak`), reset isolation, and block-size
    //    invariance across all 7 block sizes.
    let fold_ref = |from: usize, to: usize| -> [BandMeter; 3] {
        std::array::from_fn(|b| {
            let mut g_min = f64::INFINITY;
            let mut g_max = f64::NEG_INFINITY;
            let mut e_peak = 0.0f64;
            for k in from..to {
                g_min = g_min.min(per_sample_gains[k][b]);
                g_max = g_max.max(per_sample_gains[k][b]);
                e_peak = e_peak.max(per_sample_envs[k][b]);
            }
            BandMeter {
                gain: gain_to_db(per_sample_gains[to - 1][b]),
                gain_min: gain_to_db(g_min),
                gain_max: gain_to_db(g_max),
                level: env_to_db(per_sample_envs[to - 1][b]),
                level_peak: env_to_db(e_peak),
            }
        })
    };

    // Windows: [0, 256) pure silence -> exact -120 dBFS; [256, 2048) attack + release -> strict range;
    // [2048, 2600) loud R burst; [2600, 3200) initial release; [3200, 4096) deeper decay (verifies
    // window 2's loud burst peak does NOT leak into later windows!).
    let windows = [(0usize, 256usize), (256, 2048), (2048, 2600), (2600, 3200), (3200, 4096)];
    let want_windows: Vec<[BandMeter; 3]> = windows.iter().map(|&(a, b)| fold_ref(a, b)).collect();
    // Sanity-check the test signal exercises the floor, strict range, and non-leaking reset.
    for b in 0..3 {
        assert_eq!(want_windows[0][b].level, BAND_LEVEL_FLOOR_DB, "silence band {b} level floor");
        assert_eq!(want_windows[0][b].level_peak, BAND_LEVEL_FLOOR_DB, "silence band {b} peak floor");
        assert!(want_windows[1][b].gain_min < want_windows[1][b].gain && want_windows[1][b].gain < want_windows[1][b].gain_max,
            "window 1 band {b} must span both below and above final gain: {:?}", want_windows[1][b]);
        assert!(want_windows[1][b].level < want_windows[1][b].level_peak,
            "window 1 band {b} must decay below peak: {:?}", want_windows[1][b]);
    }
    // Window 4's peak must be strictly below window 2's peak on high band (> 1 dB below),
    // proving a missing reset in `take_meter` would fail immediately.
    assert!(want_windows[4][2].level_peak < want_windows[2][2].level_peak - 1.0,
        "high band peak decays between window 2 ({}) and window 4 ({})", want_windows[2][2].level_peak, want_windows[4][2].level_peak);

    for block in BLOCKS {
        let mut u = OttUnit::new(SR);
        assert!(assert_no_alloc(|| u.set_bypass(&ott_cx(0), true)));
        assert_eq!(assert_no_alloc(|| u.set_param(&ott_cx(0), UnitParam::Send(InputSendParam::OttDepth), 1.0)), Some(1.0));
        assert_eq!(assert_no_alloc(|| u.set_param(&ott_cx(0), UnitParam::Send(InputSendParam::OttTime), 0.0)), Some(0.0));
        assert!(assert_no_alloc(|| u.set_bypass(&ott_cx(0), false)));
        let mut out_l = vec![0.0f32; block];
        let mut out_r = vec![0.0f32; block];
        let mut wi = 0usize;
        let mut max_call = 0usize;
        let mut block_start = 0usize;
        while block_start < n {
            let block_end = (block_start + block).min(n);
            let mut pos = block_start;
            while pos < block_end {
                let win_end = windows[wi].1;
                let end = block_end.min(win_end);
                let len = end - pos;
                max_call = max_call.max(len);
                assert_no_alloc(|| {
                    u.process(
                        &ott_cx(pos as Frame),
                        [&sig_l[pos..end], &sig_r[pos..end]],
                        &[],
                        [&mut out_l[..len], &mut out_r[..len]],
                    );
                });
                pos = end;
                if pos == win_end {
                    assert!(assert_no_alloc(|| u.take_meter(&mut bands)), "block {block} window {wi}");
                    assert_meter_bits_eq(&bands, &want_windows[wi], &format!("block {block} window {wi}"));
                    assert!(!assert_no_alloc(|| u.take_meter(&mut bands)), "block {block} window {wi} second take");
                    wi += 1;
                }
            }
            block_start = block_end;
        }
        assert_eq!(wi, windows.len(), "block {block}: all windows taken");
        assert_eq!(max_call, block, "block {block}: full block size must reach OttUnit::process");
    }

    // 4 ms (192-sample) cadence across all 7 block sizes: outer blocks of size `block` are split
    // where each 192-sample take falls, so every block size (including 480 and 1024) reaches the
    // unit with its own call-length sequence instead of collapsing to identical 192-frame calls.
    let stride = 192usize;
    let total_4ms = stride * 16; // 3072 samples
    let want_4ms: Vec<[BandMeter; 3]> = (0..16).map(|i| fold_ref(i * stride, (i + 1) * stride)).collect();
    let mut call_patterns: Vec<(usize, Vec<usize>)> = Vec::new();
    for block in BLOCKS {
        let mut u = OttUnit::new(SR);
        assert!(assert_no_alloc(|| u.set_bypass(&ott_cx(0), true)));
        assert_eq!(assert_no_alloc(|| u.set_param(&ott_cx(0), UnitParam::Send(InputSendParam::OttDepth), 1.0)), Some(1.0));
        assert_eq!(assert_no_alloc(|| u.set_param(&ott_cx(0), UnitParam::Send(InputSendParam::OttTime), 0.0)), Some(0.0));
        assert!(assert_no_alloc(|| u.set_bypass(&ott_cx(0), false)));
        let mut out_l = vec![0.0f32; block];
        let mut out_r = vec![0.0f32; block];
        let mut calls = Vec::new();
        let mut wi = 0usize;
        let mut block_start = 0usize;
        while block_start < total_4ms {
            let block_end = (block_start + block).min(total_4ms);
            let mut pos = block_start;
            while pos < block_end {
                let next_take = (wi + 1) * stride;
                let end = block_end.min(next_take);
                let len = end - pos;
                calls.push(len);
                assert_no_alloc(|| {
                    u.process(
                        &ott_cx(pos as Frame),
                        [&sig_l[pos..end], &sig_r[pos..end]],
                        &[],
                        [&mut out_l[..len], &mut out_r[..len]],
                    );
                });
                pos = end;
                if pos == next_take {
                    assert!(assert_no_alloc(|| u.take_meter(&mut bands)), "4ms block {block} win {wi}");
                    assert_meter_bits_eq(&bands, &want_4ms[wi], &format!("4ms block {block} win {wi}"));
                    wi += 1;
                }
            }
            block_start = block_end;
        }
        assert_eq!(wi, want_4ms.len(), "4ms block {block}: all 16 takes ran");
        call_patterns.push((block, calls));
    }
    for i in 0..call_patterns.len() {
        for j in (i + 1)..call_patterns.len() {
            assert_ne!(
                call_patterns[i].1, call_patterns[j].1,
                "4ms meter loop must not feed identical call patterns to blocks {} and {}",
                call_patterns[i].0, call_patterns[j].0
            );
        }
    }
}

/// Review fix item 1: repeating a bypass command mid-ramp restarts the ramp from the gain reached
/// (matching legacy `InputFx::set_on` for ECHO, RING MOD and REVERB, and legacy `StereoOtt::set`
/// for OTT), while repeating a settled bypass leaves the settled state unchanged, across all 7
/// `BLOCKS` sizes.
#[test]
fn repeated_bypass_restarts_ramp_like_legacy_send_and_keeps_settled_state() {
    const MID_RAMP: Frame = 480; // halfway into a 20 ms (960-frame) send ramp

    for send in [InputSend::Echo, InputSend::Ring, InputSend::Reverb] {
        let make_unit = || -> Box<dyn UnitProcessor> {
            match send {
                InputSend::Echo => Box::new(EchoUnit::new(SR)),
                InputSend::Ring => Box::new(RingUnit::new(SR)),
                InputSend::Reverb => Box::new(new_reverb_unit()),
                _ => unreachable!(),
            }
        };
        let setup_send = |rig: &mut Rig, at: Frame| match send {
            InputSend::Echo => {
                rig.send_at(at, Command::SetInputSendParam(InputSendParam::EchoTime, 3.0)); // 1/16 = 6000
                rig.send_at(at, Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.0));
                rig.send_at(at, Command::SetInputSendParam(InputSendParam::EchoLevel, 1.0));
            }
            InputSend::Ring => {
                rig.send_at(at, Command::SetInputSendParam(InputSendParam::RingFreq, 300.0));
                rig.send_at(at, Command::SetInputSendParam(InputSendParam::RingLevel, 1.0));
            }
            InputSend::Reverb => {
                rig.send_at(at, Command::SetInputSendParam(InputSendParam::ReverbLevel, 1.0));
            }
            _ => unreachable!(),
        };
        let setup_unit = |rig: &mut Rig, at: Frame| match send {
            InputSend::Echo => {
                rig.send_at(at, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoTime), 3.0));
                rig.send_at(at, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoFeedback), 0.0));
                rig.send_at(at, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::EchoLevel), 1.0));
            }
            InputSend::Ring => {
                rig.send_at(at, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::RingFreq), 300.0));
                rig.send_at(at, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::RingLevel), 1.0));
            }
            InputSend::Reverb => {
                rig.send_at(at, Command::SetUnitParam(1, UnitParam::Send(InputSendParam::ReverbLevel), 1.0));
            }
            _ => unreachable!(),
        };

        // A. Repeated ON 480 frames into an opening ramp restarts a fresh 20 ms ramp.
        let t0 = F0 + 200;
        let total = 10_000 as Frame;
        let run_send = |repeat: bool| {
            let mut srig = Rig::new();
            srig.set_level(0.5);
            setup_send(&mut srig, t0);
            srig.send_at(t0, Command::SetInputSend(send, true));
            if repeat {
                srig.send_at(t0 + MID_RAMP, Command::SetInputSend(send, true));
            }
            srig.keep_output();
            srig.advance(total);
            srig.monitor
        };
        let want_rep = run_send(true);
        let want_once = run_send(false);
        let floor = if send == InputSend::Reverb { 1e-3f32 } else { 0.02f32 };
        assert!(
            max_abs_diff(&want_rep, &want_once) > floor,
            "{send:?} open repeat must differ from single ON (diff {})",
            max_abs_diff(&want_rep, &want_once)
        );

        let mut base_open: Option<[Vec<f32>; 2]> = None;
        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            if send == InputSend::Reverb {
                rig.set_stereo_inputs(|_| [0.5, 0.5], |_| [0.0, 0.0]);
            } else {
                rig.set_level(0.5);
            }
            assert!(rig.port(0).install_at(1, 1, 0, true, None, make_unit()).is_ok());
            setup_unit(&mut rig, t0);
            rig.send_at(t0, Command::SetUnitBypass(1, false));
            rig.send_at(t0 + MID_RAMP, Command::SetUnitBypass(1, false));
            rig.keep_output();
            rig.advance(total);
            if send == InputSend::Reverb {
                let folded: Vec<f32> = rig.monitor.iter().zip(&rig.monitor_right).map(|(&l, &r)| lf_engine::fold(l, r)).collect();
                assert!(max_abs_diff(&folded, &want_rep) < 1e-6, "reverb open repeat block {block} vs InputFx");
                assert!(max_abs_diff(&folded, &want_once) > floor, "reverb open repeat block {block} vs single ON");
            } else {
                assert_bits_eq(&rig.monitor, &want_rep, &format!("{send:?} open repeat block {block} L"));
                assert_bits_eq(&rig.monitor_right, &want_rep, &format!("{send:?} open repeat block {block} R"));
            }
            let cur = [rig.monitor.clone(), rig.monitor_right.clone()];
            let b = base_open.get_or_insert_with(|| cur.clone());
            assert_bits_eq(&cur[0], &b[0], &format!("{send:?} open repeat block {block} vs 1 L"));
            assert_bits_eq(&cur[1], &b[1], &format!("{send:?} open repeat block {block} vs 1 R"));
            drain_returns(&mut rig);
        }

        // B. Repeated OFF 480 frames into a closing ramp restarts a fresh 20 ms closing ramp.
        let settle = 2_400 as Frame;
        let t_off = F0 + settle + 200;
        let run_close_send = |repeat: bool| {
            let mut srig = Rig::new();
            setup_send(&mut srig, F0);
            srig.send_at(F0, Command::SetInputSend(send, true));
            srig.advance(settle);
            srig.set_level(0.5);
            srig.send_at(t_off, Command::SetInputSend(send, false));
            if repeat {
                srig.send_at(t_off + MID_RAMP, Command::SetInputSend(send, false));
            }
            srig.keep_output();
            srig.advance(total);
            srig.monitor
        };
        let want_close_rep = run_close_send(true);
        let want_close_once = run_close_send(false);
        assert!(
            max_abs_diff(&want_close_rep, &want_close_once) > floor,
            "{send:?} close repeat must differ from single OFF (diff {})",
            max_abs_diff(&want_close_rep, &want_close_once)
        );
        let mut base_close: Option<[Vec<f32>; 2]> = None;
        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            assert!(rig.port(0).install_at(1, 1, 0, false, None, make_unit()).is_ok());
            setup_unit(&mut rig, F0);
            rig.advance(settle);
            if send == InputSend::Reverb {
                rig.set_stereo_inputs(|_| [0.5, 0.5], |_| [0.0, 0.0]);
            } else {
                rig.set_level(0.5);
            }
            rig.send_at(t_off, Command::SetUnitBypass(1, true));
            rig.send_at(t_off + MID_RAMP, Command::SetUnitBypass(1, true));
            rig.keep_output();
            rig.advance(total);
            if send == InputSend::Reverb {
                let folded: Vec<f32> = rig.monitor.iter().zip(&rig.monitor_right).map(|(&l, &r)| lf_engine::fold(l, r)).collect();
                assert!(max_abs_diff(&folded, &want_close_rep) < 1e-6, "reverb close repeat block {block} vs InputFx");
                assert!(max_abs_diff(&folded, &want_close_once) > floor, "reverb close repeat block {block} vs single OFF");
            } else {
                assert_bits_eq(&rig.monitor, &want_close_rep, &format!("{send:?} close repeat block {block} L"));
                assert_bits_eq(&rig.monitor_right, &want_close_rep, &format!("{send:?} close repeat block {block} R"));
            }
            let cur = [rig.monitor.clone(), rig.monitor_right.clone()];
            let b = base_close.get_or_insert_with(|| cur.clone());
            assert_bits_eq(&cur[0], &b[0], &format!("{send:?} close repeat block {block} vs 1 L"));
            assert_bits_eq(&cur[1], &b[1], &format!("{send:?} close repeat block {block} vs 1 R"));
            drain_returns(&mut rig);
        }

        // C. Settled ON and settled OFF are bit-for-bit unchanged by a repeated bypass command.
        for settled_bypassed in [false, true] {
            let run_settled = |block: usize, repeat: bool| {
                let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
                assert!(rig.port(0).install_at(1, 1, 0, settled_bypassed, None, make_unit()).is_ok());
                setup_unit(&mut rig, F0);
                rig.advance(settle);
                rig.set_stereo_inputs(|_| [0.5, 0.5], |_| [0.0, 0.0]);
                if repeat {
                    rig.send_at(F0 + settle + 200, Command::SetUnitBypass(1, settled_bypassed));
                }
                rig.keep_output();
                rig.advance(4_000);
                let out = [rig.monitor.clone(), rig.monitor_right.clone()];
                drain_returns(&mut rig);
                out
            };
            let want = run_settled(128, false);
            for block in BLOCKS {
                let got = run_settled(block, true);
                assert_bits_eq(&got[0], &want[0], &format!("{send:?} settled bypassed={settled_bypassed} block {block} L"));
                assert_bits_eq(&got[1], &want[1], &format!("{send:?} settled bypassed={settled_bypassed} block {block} R"));
            }
        }
    }

    // OTT: repeated bypass mid-fade and when settled matches legacy `InputSend::Ott` bit for bit.
    {
        let t0 = F0 + 200;
        let mut srig = Rig::new();
        srig.set_level(0.35);
        srig.send_at(t0, Command::SetInputSend(InputSend::Ott, true));
        srig.send_at(t0 + 240, Command::SetInputSend(InputSend::Ott, true));
        srig.keep_output();
        srig.advance(3_000);
        let want_ott = srig.monitor.clone();
        for block in BLOCKS {
            let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
            rig.set_level(0.35);
            assert!(rig.port(0).install_at(1, 1, 0, true, None, Box::new(OttUnit::new(SR))).is_ok());
            rig.send_at(t0, Command::SetUnitBypass(1, false));
            rig.send_at(t0 + 240, Command::SetUnitBypass(1, false));
            rig.keep_output();
            rig.advance(3_000);
            assert_bits_eq(&rig.monitor, &want_ott, &format!("ott mid-fade repeat block {block}"));
            drain_returns(&mut rig);
        }

        for settled_bypassed in [false, true] {
            let run_ott_settled = |block: usize, repeat: bool| {
                let mut rig = Rig::with(Opts { sr: SR, start: F0, block, ..Opts::default() });
                assert!(rig.port(0).install_at(1, 1, 0, settled_bypassed, None, Box::new(OttUnit::new(SR))).is_ok());
                rig.set_level(0.35);
                if repeat {
                    rig.send_at(F0 + 200, Command::SetUnitBypass(1, settled_bypassed));
                }
                rig.keep_output();
                rig.advance(2_000);
                let mon = rig.monitor.clone();
                drain_returns(&mut rig);
                mon
            };
            let want = run_ott_settled(128, false);
            for block in BLOCKS {
                let got = run_ott_settled(block, true);
                assert_bits_eq(&got, &want, &format!("ott settled bypassed={settled_bypassed} block {block}"));
            }
        }
    }
}

/// Review fix item 4: `tests/ott.rs`'s static gain curve points (including the 6 dB downward soft
/// knee and no-inversion monotonicity) and soft-knee ceiling shape checks run directly on `OttUnit`
/// under `assert_no_alloc`, bit-identical across all 7 `BLOCKS` sizes. A hard clip at `CEILING` or
/// `CEILING_KNEE`, or a hard knee in the gain curve, fails this test.
#[test]
fn ott_unit_static_gain_points_and_soft_knee_shape() {
    // At DC, LP88² · AP2500 has exact unity gain in band 0 and 0 in bands 1 & 2, so the linked
    // peak follower settles with zero ripple to |x|, letting us measure both the static gain curve
    // (DEPTH = 1) and the wet soft ceiling (DEPTH = 0) directly through `OttUnit::process`.
    const RAMP: usize = 4_800; // 100 ms raised-cosine ramp avoids LP88² step overshoot on peak follower
    const SETTLE: usize = 24_000; // 500 ms hold settles any residual to < 1e-5 dB at TIME = 0

    let settled_dc = |depth: f64, amp: f32, block: usize| -> (f32, f32, [f64; 3]) {
        let mut unit = OttUnit::new(SR);
        let cx0 = ott_cx(0);
        assert!(assert_no_alloc(|| unit.set_bypass(&cx0, true)));
        assert_eq!(assert_no_alloc(|| unit.set_param(&cx0, UnitParam::Send(InputSendParam::OttDepth), depth)), Some(depth));
        assert_eq!(assert_no_alloc(|| unit.set_param(&cx0, UnitParam::Send(InputSendParam::OttTime), 0.0)), Some(0.0));
        assert!(assert_no_alloc(|| unit.set_bypass(&cx0, false)));
        let mut inp = Vec::with_capacity(RAMP + SETTLE);
        for k in 0..RAMP {
            let t = (k + 1) as f64 / RAMP as f64;
            let w = 0.5 * (1.0 - (PI * t).cos());
            inp.push((amp as f64 * w) as f32);
        }
        inp.resize(RAMP + SETTLE, amp);
        let (ol, or) = ott_unit_run(&mut unit, &inp, &inp, block);
        (*ol.last().unwrap(), *or.last().unwrap(), unit.gains())
    };

    // 1. Static gain curve points from `tests/ott.rs::the_gain_curve_holds_its_static_points`.
    let points = [
        (-90.0, 0.0),
        (-60.0, 0.0),
        (-57.0, 12.0 * 0.25),
        (-54.0, 12.0 * 0.5),
        (-51.0, 10.0 * 0.75),
        (-48.0, 8.0),
        (-45.0, 6.0),
        (-36.0, 0.0),
        (-30.0, 0.0),
        (-27.0, 0.0),
        (-24.0, -0.75 * 9.0 / 12.0), // center of the 6 dB downward soft knee (-0.5625 dB; hard knee = 0)
        (-21.0, -2.25),
        (-12.0, -9.0),
        (0.0, -18.0),
    ];
    for &(level, want_db) in &points {
        let amp = 10f64.powf(level / 20.0) as f32;
        let mut base_y: Option<f32> = None;
        for block in BLOCKS {
            let (yl, yr, gains) = settled_dc(1.0, amp, block);
            assert_eq!(yl.to_bits(), yr.to_bits(), "level {level} block {block}: L == R");
            let by = *base_y.get_or_insert(yl);
            assert_eq!(yl.to_bits(), by.to_bits(), "level {level} block {block} vs 1");
            let gain_from_unit = ott_db(gains[0]) - MAKEUP_DB;
            let gain_from_audio = ott_db((yl.abs() as f64) / (amp as f64)) - MAKEUP_DB;
            assert!(
                (gain_from_unit - want_db).abs() < 1e-4,
                "level {level} dBFS block {block}: unit gain {gain_from_unit:.6} dB vs want {want_db:.6}"
            );
            assert!(
                (gain_from_audio - want_db).abs() < 1e-3,
                "level {level} dBFS block {block}: audio gain {gain_from_audio:.6} dB vs want {want_db:.6}"
            );
        }
    }

    // Monotonicity (no inversion): louder DC levels always leave `OttUnit` strictly louder.
    for block in BLOCKS {
        let mut unit = OttUnit::new(SR);
        let cx0 = ott_cx(0);
        assert!(assert_no_alloc(|| unit.set_bypass(&cx0, true)));
        assert_eq!(assert_no_alloc(|| unit.set_param(&cx0, UnitParam::Send(InputSendParam::OttDepth), 1.0)), Some(1.0));
        assert_eq!(assert_no_alloc(|| unit.set_param(&cx0, UnitParam::Send(InputSendParam::OttTime), 0.0)), Some(0.0));
        assert!(assert_no_alloc(|| unit.set_bypass(&cx0, false)));
        let mut last = 0.0f32;
        for step in -90..=0 {
            let amp = 10f64.powf(step as f64 / 20.0) as f32;
            let inp = vec![amp; 4_800];
            let (ol, _) = ott_unit_run(&mut unit, &inp, &inp, block);
            let y = *ol.last().unwrap();
            assert!(y > last, "block {block}: curve folded back at {step} dBFS ({y} <= {last})");
            last = y;
        }
    }

    // 2. Soft-knee ceiling shape on `OttUnit` at DEPTH = 0 (unity bands -> direct `ceiling(x)`):
    //    identity below `CEILING_KNEE`, slope 1 at the knee, `tanh(1)` one room past, slope
    //    `1/cosh²(0.5)` half a room past, odd, monotonic, and `<= CEILING` out to +40 dBFS.
    let room = CEILING - CEILING_KNEE;
    let h = 2e-3;
    let eval_ceiling = |x: f64, block: usize| -> f64 {
        let (yl, yr, _) = settled_dc(0.0, x as f32, block);
        assert_eq!(yl.to_bits(), yr.to_bits(), "ceiling x={x} block {block}: L == R");
        yl as f64
    };
    let mut base_shape: Option<Vec<u32>> = None;
    for block in BLOCKS {
        let mut shape_bits = Vec::new();
        for x in [0.0, 1e-6, 0.25, CEILING_KNEE] {
            let yp = eval_ceiling(x, block);
            let yn = eval_ceiling(-x, block);
            assert!((yp - x).abs() < 1e-6, "block {block} identity +{x}: got {yp}");
            assert!((yn + x).abs() < 1e-6, "block {block} identity -{x}: got {yn}");
            shape_bits.push((yp as f32).to_bits());
            shape_bits.push((yn as f32).to_bits());
        }
        // Slope 1 entering the soft ceiling above `CEILING_KNEE`.
        let y_knee = eval_ceiling(CEILING_KNEE, block);
        let y_knee_h = eval_ceiling(CEILING_KNEE + h, block);
        let knee_slope = (y_knee_h - y_knee) / h;
        assert!((knee_slope - 1.0).abs() < 2e-3, "block {block}: slope into ceiling knee {knee_slope}");

        // One room past the knee: `CEILING_KNEE + room * tanh(1)` (hard clip at CEILING is off by ~0.032).
        let y_room = eval_ceiling(CEILING_KNEE + room, block);
        let want_room = CEILING_KNEE + room * 1f64.tanh();
        assert!((y_room - want_room).abs() < 1e-5, "block {block}: 1-room bend {y_room} vs {want_room}");
        shape_bits.push((y_room as f32).to_bits());

        // Half a room past the knee: slope is `1 / cosh²(0.5)` (hard clip has slope 1 or 0).
        let mid = CEILING_KNEE + room / 2.0;
        let y_lo = eval_ceiling(mid - h, block);
        let y_hi = eval_ceiling(mid + h, block);
        let mid_slope = (y_hi - y_lo) / (2.0 * h);
        let want_slope = 1.0 / 0.5f64.cosh().powi(2);
        assert!((mid_slope - want_slope).abs() < 2e-3, "block {block}: mid-knee slope {mid_slope} vs {want_slope}");
        shape_bits.push((y_lo as f32).to_bits());
        shape_bits.push((y_hi as f32).to_bits());

        // Monotonic, odd, and <= CEILING out to +40 dBFS (x = 100.0).
        let mut last = y_knee;
        for &x in &[
            CEILING_KNEE + 0.25 * room,
            CEILING_KNEE + 0.5 * room,
            CEILING_KNEE + room,
            CEILING_KNEE + 2.0 * room,
            1.5,
            3.0,
            10.0,
            30.0,
            100.0,
        ] {
            let yp = eval_ceiling(x, block);
            let yn = eval_ceiling(-x, block);
            assert!(yp >= last, "block {block}: ceiling folded back at {x} ({yp} < {last})");
            assert!(yp <= CEILING, "block {block}: ceiling exceeded CEILING at {x}: {yp}");
            assert_eq!((yn as f32).to_bits(), (-(yp as f32)).to_bits(), "block {block}: ceiling odd at {x}");
            shape_bits.push((yp as f32).to_bits());
            last = yp;
        }
        let bs = base_shape.get_or_insert_with(|| shape_bits.clone());
        assert_eq!(&shape_bits, bs, "block {block}: soft-knee shape bits match block 1");
    }
}

/// Review fix round 2 items 4 & 5: the narrow `Engine` stereo reverb tap hooks must never return
/// a gapped or truncated trace silently when a render exceeds `max_block` (including a chunked
/// overflow `60 + 60 + 30` into a `100`-frame tap where the third chunk would otherwise fit after
/// a skipped middle chunk), must keep the RT path free of allocations/panics and leave audio bits
/// unchanged, and must recover cleanly on `clear_reverb_stereo_tap`.
#[test]
fn reverb_stereo_tap_overflow_fails_loudly_off_rt_path() {
    let input = pin_input();

    // Baseline without the stereo tap enabled.
    let mut base_rig = Rig::with(Opts { sr: SR, start: F0, block: 128, ..Opts::default() });
    let sig = input.clone();
    base_rig.set_input(move |f| sig.get((f - F0) as usize).copied().unwrap_or(0.0));
    base_rig.send_at(F0, Command::SetInputSend(InputSend::Reverb, true));
    base_rig.keep_output();
    base_rig.advance(60 + 60 + 30 + 128 + 64);
    let want_record = base_rig.record.clone();

    // Same render with a 100-frame stereo tap enabled via the narrow `Engine` hooks.
    let mut rig = Rig::with(Opts { sr: SR, start: F0, block: 128, ..Opts::default() });
    rig.engine.enable_reverb_stereo_tap(100);
    let sig = input.clone();
    rig.set_input(move |f| sig.get((f - F0) as usize).copied().unwrap_or(0.0));
    rig.send_at(F0, Command::SetInputSend(InputSend::Reverb, true));
    rig.keep_output();

    // 1. Multi-chunk gapped-trace case: 60 + 60 (overflows 100) + 30 (would fit after gap without latch).
    rig.block = 60;
    rig.advance(60);
    rig.advance(60);
    rig.block = 30;
    rig.advance(30);
    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.engine.reverb_stereo_tap()));
    assert!(err.is_err(), "multi-chunk overflow (60+60+30 into 100) must panic off the RT path");

    // 2. Single oversized block (128 > 100) after clear also fails loudly off the RT path.
    rig.engine.clear_reverb_stereo_tap();
    rig.block = 128;
    rig.advance(128);
    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.engine.reverb_stereo_tap()));
    assert!(err.is_err(), "single-block overflow (128 into 100) must panic off the RT path");

    // 3. Clearing resets the latch; a subsequent within-capacity render (64 <= 100) reads cleanly.
    rig.engine.clear_reverb_stereo_tap();
    rig.block = 64;
    rig.advance(64);
    let [wl, wr] = rig.engine.reverb_stereo_tap();
    assert_eq!(wl.len(), 64);
    assert_eq!(wr.len(), 64);

    // 4. Audio output bits are untouched by the tap and its overflow state.
    assert_bits_eq(&rig.record, &want_record, "reverb stereo tap overflow leaves audio bits unchanged");

    // 5. Zero-capacity enabled tap fails loudly on any rendered frame.
    rig.engine.enable_reverb_stereo_tap(0);
    rig.block = 1;
    rig.advance(1);
    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.engine.reverb_stereo_tap()));
    assert!(err.is_err(), "zero-capacity enabled tap must panic on read after rendering 1 frame");
}
