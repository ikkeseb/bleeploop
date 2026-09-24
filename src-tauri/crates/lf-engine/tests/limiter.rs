//! The master limiter (`dsp::compressor`, Blink's DynamicsCompressorNode as `makeMasterLimiter`
//! configures it) against the Tone references `limiter-sweep-48000` and `limiter-sweep-44100`: the
//! probe's stereo ramp-and-bursts input through the node, rendered from frame 0 as the offline context
//! does. Also: the same bits at any block size and start frame, no allocation while rendering, and the
//! pre-delay the engine will have to align for.

mod common;

use assert_no_alloc::{assert_no_alloc, violation_count};
use common::refs::{self, Class};
use lf_engine::dsp::compressor::{Compressor, QUANTUM};
use lf_engine::grid::Frame;

const SCENARIOS: [&str; 2] = ["limiter-sweep-48000", "limiter-sweep-44100"];

/// The scenario's input through a fresh master limiter, `block` frames per call from `start`.
fn render(id: &str, block: usize, start: Frame) -> Vec<Vec<f32>> {
    let s = refs::scenario(id);
    let [mut l, mut r] = refs::limiter_input(s);
    let mut limiter = Compressor::master_limiter(s.rate as f32);
    let violations = violation_count();
    let mut at = 0;
    while at < s.frames {
        let n = block.min(s.frames - at);
        let (bl, br) = (&mut l[at..at + n], &mut r[at..at + n]);
        assert_no_alloc(|| limiter.process(start + at as Frame, bl, br));
        at += n;
    }
    assert_eq!(violation_count(), violations, "{id}: the limiter allocated while rendering");
    vec![l, r]
}

#[test]
fn limiter_sweeps_match_blink_at_class_n() {
    for id in SCENARIOS {
        refs::assert_class(id, &render(id, 128, 0), Class::N);
    }
}

#[test]
fn limiter_renders_the_same_bits_at_any_block_size_and_quantum_aligned_start() {
    for id in SCENARIOS {
        let reference = render(id, 128, 0);
        for block in [1, 64, 127, 128, 480] {
            assert!(render(id, block, 0) == reference, "{id}: block {block} differs");
        }
        // Control steps follow the absolute frame: a start on a later quantum boundary is the same render.
        assert!(render(id, 127, 1000 * QUANTUM) == reference, "{id}: a later start differs");
    }
}

#[test]
fn limiter_latency_is_blinks_truncated_pre_delay() {
    // kPreDelay (6 ms) × the rate in f32, truncated: what measureMasterLimiterLatency finds.
    assert_eq!(Compressor::master_limiter(48000.0).latency(), 288);
    assert_eq!(Compressor::master_limiter(44100.0).latency(), 264);
    // An impulse comes out exactly that late.
    for rate in [44100.0f32, 48000.0] {
        let mut limiter = Compressor::master_limiter(rate);
        let frames = 512 + (rate * 0.05).ceil() as usize;
        let (mut l, mut r) = (vec![0.0f32; frames], vec![0.0f32; frames]);
        l[512] = 0.1;
        limiter.process(0, &mut l, &mut r);
        let first = l.iter().position(|v| v.abs() > 1e-6).expect("the impulse comes out");
        assert_eq!(first - 512, limiter.latency(), "at {rate}");
    }
}

/// The limiter's cost per 128 frames. Ignored by default (timing on CI is noise):
///   cargo test -p lf-engine --test limiter -- --ignored --nocapture
#[test]
#[ignore]
fn limiter_cost_per_quantum() {
    let rate = 48000.0f32;
    let quanta = 48000 * 60 / 128;
    let mut limiter = Compressor::master_limiter(rate);
    // A loud triangle keeps the curve above the threshold, the costlier branch.
    let src: Vec<f32> = (0..128).map(|k| 2.0 * (1.0 - 4.0 * ((k as f32 / 128.0) - 0.5).abs())).collect();
    let (mut l, mut r) = (src.clone(), src.clone());
    let started = std::time::Instant::now();
    for q in 0..quanta {
        l.copy_from_slice(&src);
        r.copy_from_slice(&src);
        limiter.process(q as Frame * 128, &mut l, &mut r);
    }
    let per = started.elapsed().as_secs_f64() / quanta as f64;
    println!("limiter: {:.2} µs per 128 frames ({:.2} % of the quantum at 48 k)", per * 1e6, per / (128.0 / 48000.0) * 100.0);
    std::hint::black_box((&l, &r));
}
