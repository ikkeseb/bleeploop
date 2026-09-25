//! The shared reverb bus (`dsp::fx::ReverbBus` over `dsp::convolver`) against Tone's:
//! `fx-reverb-full-48000` replayed from the manifest (a chain with only its reverb send on, at amount
//! 1, into `makeReverbBus`; the reference is the destination, the chain's output plus the bus). Also:
//! the same bits at any block size, no allocation while rendering, and the bus's cost (ignored).
//!
//! The bus's IR comes from `dsp::reverb_ir` with the scenario's own third and fourth draws
//! (makeReverbBus's `generate()`, see `tests/reverb_ir.rs`).

mod common;

use std::sync::Arc;

use assert_no_alloc::assert_no_alloc;
use common::fx::{states, timing, BLOCKS};
use common::refs::{self, Class};
use common::violation_count;
use lf_engine::dsp::buffer_source::AudioBuffer;
use lf_engine::dsp::fx::{Ctl, FxChain, ReverbBus, REVERB_DECAY, REVERB_PRE_DELAY};
use lf_engine::dsp::noise::NoiseTables;
use lf_engine::dsp::param::QUANTUM;
use lf_engine::dsp::reverb_ir;
use lf_engine::dsp::rng::Mulberry32;

const ID: &str = "fx-reverb-full-48000";

/// The bus's IR at `rate`, from the scenario's draws.
fn impulse_response(s: &refs::Scenario) -> [Vec<f32>; 2] {
    let ir = refs::scenario("ir-reverb-48000");
    assert_eq!(ir.setup["decay"].as_f64(), Some(REVERB_DECAY));
    assert_eq!(ir.setup["preDelay"].as_f64(), Some(REVERB_PRE_DELAY));
    let t = &refs::manifest().tables;
    let [l, r] = NoiseTables::generate(&mut Mulberry32::new(t.seed)).white;
    let white = Arc::new(AudioBuffer::new(t.rate as f32, vec![l, r]));
    assert_eq!(s.random.len(), 4, "the Reverb constructor's generate() and makeReverbBus's each draw two offsets");
    reverb_ir::generate(&white, s.rate as f32, REVERB_DECAY, REVERB_PRE_DELAY, [s.random[2], s.random[3]])
}

struct Rig {
    chain: FxChain,
    bus: ReverbBus,
}

impl Rig {
    fn new(s: &refs::Scenario, ir: &[Vec<f32>; 2]) -> Self {
        let rate = s.rate as f32;
        let start = Ctl { now: 0.0, frame: 0 };
        let bus = ReverbBus::new(rate, [&ir[0], &ir[1]], 0);
        let mut chain = FxChain::new(rate, Some(&states(s)), start);
        chain.set_timing(timing(s), start).expect("a valid timing");
        Rig { chain, bus }
    }

    /// Frames `frame..frame + x.len()` of the destination, split at quanta as the engine feeds the
    /// bus: the chain, then the bus on the chain's send.
    fn process(&mut self, frame: u64, x: &[f32], out: &mut [f32; 2 * QUANTUM], left: &mut [f32], right: &mut [f32]) {
        let mut done = 0;
        while done < x.len() {
            let f = frame + done as u64;
            let n = (QUANTUM - (f % QUANTUM as u64) as usize).min(x.len() - done);
            let span = done..done + n;
            let (dry, send) = out.split_at_mut(QUANTUM);
            let (dry, send) = (&mut dry[..n], &mut send[..n]);
            self.chain.process(f, &x[span.clone()], dry, send);
            let input = (!self.chain.send_silent()).then_some(&*send);
            self.bus.process(f, input, &mut left[span.clone()], &mut right[span.clone()]);
            // The destination sums the chain's mono output into both channels.
            for (o, &d) in left[span.clone()].iter_mut().chain(right[span].iter_mut()).zip(dry.iter().chain(dry.iter())) {
                *o += d;
            }
            done += n;
        }
    }
}

fn render(block: usize) -> Vec<Vec<f32>> {
    let s = refs::scenario(ID);
    let x = refs::fx_input(s);
    let ir = impulse_response(s);
    let mut rig = Rig::new(s, &ir);
    let (mut left, mut right) = (vec![0.0f32; s.frames], vec![0.0f32; s.frames]);
    let mut scratch = [0.0f32; 2 * QUANTUM];
    let violations = violation_count();
    let mut at = 0;
    while at < s.frames {
        let end = (at + block).min(s.frames);
        let (l, r) = (&mut left[at..end], &mut right[at..end]);
        assert_no_alloc(|| rig.process(at as u64, &x[at..end], &mut scratch, l, r));
        at = end;
    }
    assert_eq!(violation_count(), violations, "{ID}: the chain or the bus allocated while rendering");
    vec![left, right]
}

#[test]
fn fx_reverb_full_48000_is_null() {
    let render = render(128);
    refs::assert_class(ID, &render, Class::N);
    // Beyond N: Blink's bits, where RustFFT runs the AVX code it ran for the reference. Its planner
    // picks SSE or scalar code on other CPUs, whose transforms round differently.
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx") && is_x86_feature_detected!("fma") {
        let bits = |v: &[Vec<f32>]| v.iter().map(|c| c.iter().map(|x| x.to_bits()).collect::<Vec<_>>()).collect::<Vec<_>>();
        assert!(bits(&render) == bits(&refs::reference(ID)), "{ID}: not bit-exact");
    }
}

#[test]
fn renders_the_same_bits_at_any_block_size() {
    let bits = |v: &[Vec<f32>]| v.iter().map(|c| c.iter().map(|x| x.to_bits()).collect::<Vec<_>>()).collect::<Vec<_>>();
    let reference = bits(&render(128));
    for block in BLOCKS {
        assert!(bits(&render(block)) == reference, "block {block} differs");
    }
}

/// The bus alone per 128 frames at 48 kHz, with a send running through the whole IR: the mean and the
/// worst quantum (every 32nd, where the small FFT stages meet an 8192 one). Ignored by default
/// (timing on CI is noise):
///   cargo test -p lf-engine --test fx_reverb -- --ignored --nocapture
#[test]
#[ignore]
fn bus_cost_per_quantum() {
    let s = refs::scenario(ID);
    let ir = impulse_response(s);
    let mut bus = ReverbBus::new(48000.0, [&ir[0], &ir[1]], 0);
    let x: Vec<f32> = (0..QUANTUM).map(|k| ((k * 37 % 101) as f32 / 50.0) - 1.0).collect();
    let (mut l, mut r) = ([0.0f32; QUANTUM], [0.0f32; QUANTUM]);
    let quanta = 48000 * 60 / QUANTUM;
    let mut times = Vec::with_capacity(quanta);
    for q in 0..quanta {
        let started = std::time::Instant::now();
        bus.process((q * QUANTUM) as u64, Some(&x), &mut l, &mut r);
        times.push(started.elapsed().as_secs_f64());
        std::hint::black_box((&l, &r));
    }
    let quantum = QUANTUM as f64 / 48000.0;
    let mean = times.iter().sum::<f64>() / quanta as f64;
    // The work repeats every 32 quanta once every stage runs (past the IR): the median per phase
    // separates the schedule's worst quantum from the OS's preemptions, which the raw maximum keeps.
    let steady = &times[2 * ir[0].len() / QUANTUM..];
    let phase_median = |p: usize| {
        let mut t: Vec<f64> = steady.iter().skip(p).step_by(32).copied().collect();
        t.sort_by(f64::total_cmp);
        t[t.len() / 2]
    };
    let worst_phase = (0..32).map(phase_median).fold(0.0, f64::max);
    let raw_worst = steady.iter().copied().fold(0.0, f64::max);
    let pct = |t: f64| t / quantum * 100.0;
    println!(
        "reverb bus per 128 frames at 48 k: mean {:.1} µs ({:.2} %), worst quantum {:.1} µs ({:.2} %), raw max {:.1} µs ({:.2} %)",
        mean * 1e6,
        pct(mean),
        worst_phase * 1e6,
        pct(worst_phase),
        raw_worst * 1e6,
        pct(raw_worst)
    );
}
