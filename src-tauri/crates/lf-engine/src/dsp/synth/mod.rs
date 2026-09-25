//! The built-in synths: Tone's voices ([`voice`]), its LFO and Vibrato ([`vibrato`]), the app's poly
//! layer over them ([`poly`]: lead, pad, piano and organ), the bass ([`bass`]: MonoSynth with the
//! held-note stack) and the drum kit ([`drum`]: MembraneSynth, NoiseSynth and MetalSynth). Built on
//! [`crate::dsp::oscillator`], [`crate::dsp::envelope`], [`crate::dsp::filter`],
//! [`crate::dsp::buffer_source`], [`crate::dsp::delay`] and [`crate::dsp::signal`]; tested against the
//! Tone references in `tests/synth.rs` and `tests/voices.rs`.
//!
//! Construction allocates (wave tables, voice pools); scheduling and rendering do not.

pub mod bass;
pub mod drum;
pub mod poly;
pub mod vibrato;
pub mod voice;

pub use bass::Bass;
pub use drum::DrumKit;
pub use poly::{PolyKind, PolySynth};
