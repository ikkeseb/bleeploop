//! The reverb's impulse response: Tone 15.1.22's `Reverb.generate` (`effect/Reverb.js`), rendered the
//! way Blink renders the OfflineContext it builds. Two white [`Noise`] sources, each downmixed to one
//! channel of a ChannelMerger (Blink's stereo-to-mono rule, `platform/audio/audio_bus.cc`:
//! `0.5 * L + 0.5 * R`), through a gain that is 0 until the pre-delay, 1 at it, then
//! `exponentialApproachValueAtTime(0, preDelay, decay)`. The IR runs `decay + preDelay` seconds at
//! the context rate, truncated to whole frames as the OfflineAudioContext length is.
//!
//! Each Noise start draws one `Math.random` for its table offset, left then right. Tone's Reverb
//! generates in its constructor and `makeReverbBus` (`src/audio/fx/fx.ts`) calls `generate()` again;
//! the second generation's buffer is set last (it waits for the first's `ready`), so the IR the
//! convolver holds comes from the SECOND pair of draws. The `ir-reverb-48000` fixture confirms it
//! (`tests/reverb_ir.rs`): its draws 3 and 4 null the reference, draws 1 and 2 do not.
//!
//! The IR is built off the audio thread ([`generate`] allocates its output); [`IrRender`] is the graph
//! itself, rendering any block size without allocating.
//!
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.

use std::sync::Arc;

use super::buffer_source::{AudioBuffer, Noise};
use super::gain::GainNode;
use super::param::{Units, QUANTUM};

/// Frames in the IR: the OfflineAudioContext length `(decay + preDelay) * rate`, truncated.
pub fn ir_frames(sample_rate: f32, decay: f64, pre_delay: f64) -> usize {
    ((decay + pre_delay) * sample_rate as f64) as usize
}

/// Reverb.generate's OfflineContext graph.
pub struct IrRender {
    noise: [Noise; 2],
    gain: GainNode,
    merged: [[f32; QUANTUM]; 2],
    out: [[f32; QUANTUM]; 2],
    rendered: Option<u64>,
}

impl IrRender {
    /// `white` is Tone's white noise table; `draws` the two `Math.random` values its Noise starts
    /// take, left then right.
    pub fn new(white: &Arc<AudioBuffer>, sample_rate: f32, decay: f64, pre_delay: f64, draws: [f64; 2]) -> Self {
        let rate = sample_rate as f64;
        let mut noise = [Noise::new(sample_rate, Arc::clone(white), 0), Noise::new(sample_rate, Arc::clone(white), 0)];
        noise[0].start(0.0, draws[0], 0.0, 0);
        noise[1].start(0.0, draws[1], 0.0, 0);
        let mut gain = GainNode::new(rate, 1.0, Units::Gain, 0);
        gain.gain.set_value_at_time(0.0, 0.0, 0);
        gain.gain.set_value_at_time(1.0, pre_delay, 0);
        gain.gain.exponential_approach_value_at_time(0.0, pre_delay, decay, 0);
        IrRender { noise, gain, merged: [[0.0; QUANTUM]; 2], out: [[0.0; QUANTUM]; 2], rendered: None }
    }

    fn process(&mut self, quantum_start: u64) {
        let mut silent = true;
        for (noise, merged) in self.noise.iter_mut().zip(self.merged.iter_mut()) {
            noise.process(quantum_start);
            if noise.silent() {
                merged.fill(0.0);
                continue;
            }
            silent = false;
            let out = noise.output();
            if out.len() == 1 {
                merged.copy_from_slice(&out[0]);
            } else {
                // Blink's down-mix: the zeroed bus, plus 0.5 * L, plus 0.5 * R.
                for ((m, &l), &r) in merged.iter_mut().zip(&out[0]).zip(&out[1]) {
                    *m = (0.0 + 0.5 * l) + 0.5 * r;
                }
            }
        }
        let input = (!silent).then_some(&self.merged[..]);
        self.gain.process(quantum_start, input, &mut self.out);
    }

    /// Render frames `frame..frame + left.len()` of the IR. Blocks must follow each other.
    pub fn render(&mut self, frame: u64, left: &mut [f32], right: &mut [f32]) {
        debug_assert_eq!(left.len(), right.len());
        let mut done = 0;
        while done < left.len() {
            let f = frame + done as u64;
            let q = f - f % QUANTUM as u64;
            if self.rendered != Some(q) {
                self.process(q);
                self.rendered = Some(q);
            }
            let at = (f - q) as usize;
            let n = (QUANTUM - at).min(left.len() - done);
            left[done..done + n].copy_from_slice(&self.out[0][at..at + n]);
            right[done..done + n].copy_from_slice(&self.out[1][at..at + n]);
            done += n;
        }
    }
}

/// The reverb IR, left and right, for Tone's `new Reverb({ decay, preDelay })` at `sample_rate`.
pub fn generate(white: &Arc<AudioBuffer>, sample_rate: f32, decay: f64, pre_delay: f64, draws: [f64; 2]) -> [Vec<f32>; 2] {
    let frames = ir_frames(sample_rate, decay, pre_delay);
    let mut ir = IrRender::new(white, sample_rate, decay, pre_delay, draws);
    let (mut left, mut right) = (vec![0.0; frames], vec![0.0; frames]);
    ir.render(0, &mut left, &mut right);
    [left, right]
}
