//! Stage 3: the synths, the FX and the limiter, ported literally from what Tone.js builds on Blink's
//! Web Audio nodes (docs/plans/native-engine.md § Stage 3). Each port is tested against the Tone
//! reference renders in `tests/fixtures/tone` (`verify/probes/tone-refs.mjs`) and holds the tightest
//! tolerance class it passes (`tests/common/refs.rs`).

pub mod buffer_source;
pub mod compressor;
pub mod delay;
pub mod envelope;
pub mod fdlibm;
pub mod gain;
pub mod noise;
pub mod oscillator;
pub mod param;
pub mod reverb_ir;
pub mod rng;
pub mod synth;
