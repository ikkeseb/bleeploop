//! OWNS (DEV builds only): the applied-note record: each `NoteOn` and `NoteOff` the engine applies, with
//! the frame it applied it on, for a reader off the audio thread. The MIDI latency benchmark reads it
//! (`src-tauri/src/engine_io/midi_bench.rs`; how to run it: `docs/VERIFY.md` § MIDI latency benchmark).
//!
//! A preallocated ring (invariant 5: recording never allocates, locks or logs): the engine holds the
//! producer, [`crate::Engine::take_applied_notes`] hands the reader out once. Nobody reads it unless a
//! benchmark runs, so a full ring refuses the note and counts it, as the event ring does. A release
//! build compiles none of this (`lib.rs` declares the module under `debug_assertions`).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::grid::Frame;

/// Notes the ring holds before it refuses: a benchmark drains it every few milliseconds.
pub const CAPACITY: usize = 4096;

/// One `NoteOn` (`on`, with its velocity 0..1) or `NoteOff` (velocity 0) as the engine applied it, and
/// the device frame it applied on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AppliedNote {
    pub note: u8,
    pub velocity: f32,
    pub frame: Frame,
    pub on: bool,
}

/// The engine's end: it records, the reader is handed out once.
pub struct NoteRecord {
    tx: Producer<AppliedNote>,
    reader: Option<AppliedNotes>,
    refused: Arc<AtomicU64>,
}

/// The reader's end: pop the notes, and how many the full ring refused so far.
pub struct AppliedNotes {
    pub rx: Consumer<AppliedNote>,
    refused: Arc<AtomicU64>,
}

impl AppliedNotes {
    /// Notes the ring refused because nobody drained it (counted since the engine was built).
    pub fn refused(&self) -> u64 {
        self.refused.load(Relaxed)
    }
}

impl NoteRecord {
    /// Allocates the ring (with the engine, off the audio thread).
    pub fn new() -> NoteRecord {
        let (tx, rx) = RingBuffer::new(CAPACITY);
        let refused = Arc::new(AtomicU64::new(0));
        NoteRecord { tx, reader: Some(AppliedNotes { rx, refused: refused.clone() }), refused }
    }

    /// The audio thread: a `NoteOn` of `note` applied at `frame`. Never allocates or waits.
    #[inline]
    pub fn record(&mut self, note: u8, velocity: f32, frame: Frame) {
        self.push(AppliedNote { note, velocity, frame, on: true });
    }

    /// The audio thread: a `NoteOff` of `note` applied at `frame`.
    #[inline]
    pub fn record_off(&mut self, note: u8, frame: Frame) {
        self.push(AppliedNote { note, velocity: 0.0, frame, on: false });
    }

    #[inline]
    fn push(&mut self, note: AppliedNote) {
        if self.tx.push(note).is_err() {
            self.refused.fetch_add(1, Relaxed);
        }
    }

    /// The reader, once; `None` after.
    pub fn take_reader(&mut self) -> Option<AppliedNotes> {
        self.reader.take()
    }
}

impl Default for NoteRecord {
    fn default() -> Self {
        NoteRecord::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reader_gets_the_notes_in_order_once_and_a_full_ring_counts_what_it_refuses() {
        let mut record = NoteRecord::new();
        let mut reader = record.take_reader().expect("the first take hands the reader out");
        assert!(record.take_reader().is_none(), "the reader is handed out once");
        record.record(60, 0.5, 128);
        record.record_off(60, 200);
        record.record(61, 1.0, 256);
        assert_eq!(reader.rx.pop(), Ok(AppliedNote { note: 60, velocity: 0.5, frame: 128, on: true }));
        assert_eq!(reader.rx.pop(), Ok(AppliedNote { note: 60, velocity: 0.0, frame: 200, on: false }));
        assert_eq!(reader.rx.pop(), Ok(AppliedNote { note: 61, velocity: 1.0, frame: 256, on: true }));
        assert!(reader.rx.pop().is_err());
        for k in 0..CAPACITY + 3 {
            record.record(48, 0.25, k as Frame);
        }
        assert_eq!(reader.refused(), 3);
        assert_eq!(reader.rx.slots(), CAPACITY);
        assert_eq!(reader.rx.pop().map(|n| n.frame), Ok(0), "the oldest stays; the newest were refused");
    }
}
