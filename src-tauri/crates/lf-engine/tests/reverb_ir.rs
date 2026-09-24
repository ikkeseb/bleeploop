//! The reverb IR (`dsp::reverb_ir`) against Tone's: `ir-reverb-48000` replayed from the manifest,
//! which of its four random draws made the stored IR, bit identity across block sizes, and the render
//! path (noise sources, buffer playback, param automation) under assert_no_alloc.

mod common;

use std::sync::Arc;

use assert_no_alloc::{assert_no_alloc, violation_count};
use common::refs::{self, Class};
use lf_engine::dsp::buffer_source::{AudioBuffer, Noise};
use lf_engine::dsp::gain::GainNode;
use lf_engine::dsp::noise::NoiseTables;
use lf_engine::dsp::param::{Units, QUANTUM};
use lf_engine::dsp::reverb_ir::{self, IrRender};
use lf_engine::dsp::rng::Mulberry32;

const ID: &str = "ir-reverb-48000";

struct Setup {
    white: Arc<AudioBuffer>,
    rate: f32,
    decay: f64,
    pre_delay: f64,
}

fn setup() -> Setup {
    let t = &refs::manifest().tables;
    let tables = NoiseTables::generate(&mut Mulberry32::new(t.seed));
    let [l, r] = tables.white;
    let s = refs::scenario(ID);
    Setup {
        white: Arc::new(AudioBuffer::new(t.rate as f32, vec![l, r])),
        rate: s.rate as f32,
        decay: s.setup["decay"].as_f64().expect("decay"),
        pre_delay: s.setup["preDelay"].as_f64().expect("preDelay"),
    }
}

/// The draws of the second `generate()` (makeReverbBus's call), which set the convolver's buffer last.
fn draws() -> [f64; 2] {
    let r = &refs::scenario(ID).random;
    assert_eq!(r.len(), 4, "the constructor's generate() and makeReverbBus's each draw two offsets");
    [r[2], r[3]]
}

fn render_in_blocks(s: &Setup, block: usize) -> Vec<Vec<f32>> {
    let frames = reverb_ir::ir_frames(s.rate, s.decay, s.pre_delay);
    let mut ir = IrRender::new(&s.white, s.rate, s.decay, s.pre_delay, draws());
    let (mut left, mut right) = (vec![0.0f32; frames], vec![0.0f32; frames]);
    let mut at = 0;
    while at < frames {
        let n = block.min(frames - at);
        let before = violation_count();
        let (l, r) = (&mut left[at..at + n], &mut right[at..at + n]);
        assert_no_alloc(|| ir.render(at as u64, l, r));
        assert_eq!(violation_count(), before, "the IR render allocated at frame {at}");
        at += n;
    }
    vec![left, right]
}

#[test]
fn ir_reverb_48000_is_null() {
    let s = setup();
    let scenario = refs::scenario(ID);
    assert_eq!(reverb_ir::ir_frames(s.rate, s.decay, s.pre_delay), scenario.frames);
    assert_eq!(scenario.setup["normalize"].as_bool(), Some(true), "the convolver normalizes; the stored buffer does not");
    let [l, r] = reverb_ir::generate(&s.white, s.rate, s.decay, s.pre_delay, draws());
    let render = [l, r];
    refs::assert_class(ID, &render, Class::N);
    // Beyond N: equal sample for sample. The bits differ only in the first quantum's pre-delay, where
    // the reference holds +0 for the port's -0: standardized-audio-context connects every source
    // through a keep-alive gain of 0 to the destination, and those sum in while their gain is still
    // automated (the first quantum only), which turns the sum's zeros positive.
    for (got, want) in render.iter().zip(&refs::reference(ID)) {
        assert!(got.iter().zip(want).all(|(a, b)| a == b), "not equal sample for sample");
        let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert!(bits(&got[QUANTUM..]) == bits(&want[QUANTUM..]), "not bit-exact after the first quantum");
    }
}

#[test]
fn the_constructors_draws_are_not_the_stored_ir() {
    let s = setup();
    let r = &refs::scenario(ID).random;
    let [l, rr] = reverb_ir::generate(&s.white, s.rate, s.decay, s.pre_delay, [r[0], r[1]]);
    let score = refs::score(&refs::reference(ID), &[l, rr], s.rate as u32);
    println!("[refs] {ID} from draws 1 and 2: residual {:.1} dB", score.residual_db);
    assert!(score.residual_db > -10.0, "the first generate()'s IR should not null the reference: {score:?}");
}

#[test]
fn renders_bit_identical_at_any_block_size() {
    let s = setup();
    let [l, r] = reverb_ir::generate(&s.white, s.rate, s.decay, s.pre_delay, draws());
    let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    for block in [1, 64, 127, 128, 480] {
        let got = render_in_blocks(&s, block);
        assert!(bits(&got[0]) == bits(&l) && bits(&got[1]) == bits(&r), "block size {block} renders differently");
    }
}

#[test]
fn scheduling_and_rendering_never_allocate() {
    let s = setup();
    let mut noise = Noise::new(s.rate, Arc::clone(&s.white), 0);
    let mut gain = GainNode::new(s.rate as f64, 1.0, Units::Gain, 0);
    let mut out = [[0.0f32; QUANTUM]; 2];
    let before = violation_count();
    assert_no_alloc(|| {
        for q in 0..400u64 {
            let frame = q * QUANTUM as u64;
            let now = frame as f64 / s.rate as f64;
            if q % 7 == 0 {
                // A drum-like hit: restart the noise, stop it later, shape the gain.
                noise.start(now, (q as f64 * 0.37) % 1.0, now, frame);
                noise.stop(now + 0.004, now, frame);
                gain.gain.target_ramp_to(0.5, 0.01, now, frame);
                gain.gain.exponential_approach_value_at_time(0.0, now + 0.01, 0.005, frame);
                gain.gain.cancel_and_hold_at_time(now + 0.012, frame);
                gain.gain.linear_ramp_to(1.0, 0.003, now + 0.013, frame);
                gain.gain.set_value(0.8, now + 0.016, frame);
            }
            noise.process(frame);
            let input = (!noise.silent()).then(|| noise.output());
            gain.process(frame, input, &mut out);
        }
    });
    assert_eq!(violation_count(), before, "scheduling or rendering allocated");
}
