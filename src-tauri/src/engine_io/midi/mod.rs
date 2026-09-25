//! OWNS: native MIDI for the engine (`docs/plans/native-engine.md` § Stage 4): the input ports (midir,
//! one callback thread per port, hot-plug polling), message parsing, the MIDI-learn bindings mirrored
//! from settings (`src/app/midi-actions.ts`), and what a message becomes: a note for the engine
//! (next block start) or a looper action stamped with its press frame (`super::FrameClock`).
//!
//! MIDI lane: build this module.
