//! Stage 3: the synths, the FX and the limiter, ported literally from what Tone.js builds on Blink's
//! Web Audio nodes (docs/plans/native-engine.md § Stage 3). Each port is tested against the Tone
//! reference renders in `tests/fixtures/tone` (`verify/probes/tone-refs.mjs`) and holds the tightest
//! tolerance class it passes (`tests/common/refs.rs`). None is wired into [`crate::engine`] yet.
//!
//! - Building blocks, one of each: param automation ([`param`], with Blink's time and quantum helpers),
//!   [`gain`], Tone's signal plumbing ([`signal`]), buffer playback and noise ([`buffer_source`],
//!   [`noise`]), [`oscillator`], [`envelope`], the delay line ([`delay`]), [`biquad`] and Tone's
//!   [`filter`], [`crossfade`], Blink's ConvolverNode ([`convolver`]), fdlibm ([`fdlibm`]) and the seeded
//!   `Math.random` ([`rng`]).
//! - Instruments and effects: the lead, piano, organ and pad synths with the app's poly layer and
//!   modulation ([`synth`]); the per-track FX chain with its filter, pitch shift, stutter and feedback delay,
//!   and the shared reverb bus ([`fx`]); the reverb IR ([`reverb_ir`]); the master limiter ([`compressor`]).
//!
//! The crate briefing (`lib.rs`) lists what is not built yet.

pub mod biquad;
pub mod buffer_source;
pub mod compressor;
pub mod convolver;
pub mod crossfade;
pub mod delay;
pub mod envelope;
pub mod fdlibm;
pub mod filter;
pub mod fx;
pub mod gain;
pub mod noise;
pub mod oscillator;
pub mod param;
pub mod reverb_ir;
pub mod rng;
pub mod signal;
pub mod synth;
