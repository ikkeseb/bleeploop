//! lf-engine: BleepLoop's native audio engine, a pure crate (docs/plans/native-engine.md § Stage 2).
//!
//! Dormant until the plan's Stage 6 flip: nothing in the app calls it yet, and the live line keeps the
//! Web Audio looper. No host, device or plugin crate may enter its dependency tree
//! (`scripts/engine-deny.mjs`); `cargo test -p lf-engine` runs on Linux and Windows CI.

#![forbid(unsafe_code)]

pub mod grid;
