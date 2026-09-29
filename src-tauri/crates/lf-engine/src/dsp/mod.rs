//! Stage 3: the synths, the FX and the limiter, ported literally from what Tone.js 15.1.22 builds on
//! Blink's Web Audio nodes (Chromium 153). Each port is tested against the Tone reference renders in
//! `tests/fixtures/tone` (`verify/probes/tone-refs.mjs`) and holds the tightest tolerance class it
//! passes, recorded in its test (the classes and the harness: `tests/common/refs.rs`). The engine plays
//! them through [`crate::effects`] and [`crate::instruments`], and the limiter on its master bus. The
//! Blink ports are BSD-3 (`THIRD-PARTY-NOTICES.md`).
//!
//! Every scenario passes class N. Bit-exact: the limiter (a literal port of Blink's
//! DynamicsCompressorKernel), the reverb IR, the filter, stutter and delay FX, the bypass crossfade and
//! the reverb bus (Blink's partitioned convolver on RustFFT 6.4.1, the FFT Chromium runs, where RustFFT
//! picks its AVX code as it did for the reference). Near exact, the residual a change is compared
//! against: the lead, piano, organ and pad −125 to −141 dB (Blink's band-limited PeriodicWave tables),
//! the bass −114 dB, the drum kit −95 dB (the metals' FM near Nyquist, Blink's biquad tail-stop), the
//! hot delay −776 dB (Blink's denormal flush), and PitchShift −69 to −72 dB, the nearest to the −60 dB
//! bar (a 1-ulp wave-table difference moves its float delay reads). FFT paths may take ≤ −120 dB
//! instead of bit-exact. Two literal Tone behaviours stay: a bypassed chain is not bit-transparent
//! (about +0.035 dB), and a closed stutter gate leaks about 0.2 % of the dry signal.
//!
//! Costs per 128-frame quantum at 48 kHz, release, on the dev PC: the pad at 12 voices 85 µs, the drum
//! kit with all 16 voices ringing 367 µs, a chain with pitch on 18 µs, the reverb bus 47 µs mean and
//! 106 µs worst (the whole engine's bars: `tests/perf.rs`).
//!
//! - Building blocks, one of each: param automation ([`param`], with Blink's time and quantum helpers),
//!   [`gain`], Tone's signal plumbing and `Scale` ([`signal`]), buffer playback and noise ([`buffer_source`],
//!   [`noise`]), [`oscillator`], [`envelope`], the delay line ([`delay`]), [`biquad`] and Tone's
//!   [`filter`], [`crossfade`], Blink's ConvolverNode ([`convolver`]), fdlibm ([`fdlibm`]) and the seeded
//!   `Math.random` ([`rng`]).
//! - Instruments and effects: the lead, piano, organ and pad synths with the app's poly layer and
//!   modulation, the bass and the drum kit ([`synth`]); the per-track FX chain with its filter, pitch shift,
//!   stutter and feedback delay, and the shared reverb bus ([`fx`]); the reverb IR ([`reverb_ir`]); the
//!   master limiter ([`compressor`]).
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
