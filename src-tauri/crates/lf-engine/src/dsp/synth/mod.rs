//! The built-in synths: Tone's voices ([`voice`]), its LFO and Vibrato ([`vibrato`]), and the app's
//! poly layer over them ([`poly`]: lead, pad, piano and organ). Built on [`crate::dsp::oscillator`],
//! [`crate::dsp::envelope`], [`crate::dsp::delay`] and [`crate::dsp::signal`]; tested against the Tone
//! references in `tests/synth.rs`.
//!
//! Construction allocates (wave tables, voice pools); scheduling and rendering do not.

pub mod poly;
pub mod vibrato;
pub mod voice;

pub use poly::{PolyKind, PolySynth};
