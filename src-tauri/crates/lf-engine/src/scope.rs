//! OWNS: the live scope taps, what the UI draws of the sound coming out now: per source a min/max
//! envelope decimated into fixed 4 ms columns, folded on the audio thread and published off it. The
//! sources are the five lanes after their FX, the live monitor, and the master — the engine's whole
//! output, after the limiter and with the monitor summed in, which is what leaves for the device.
//!
//! A column is finished work: the engine folds one chunk at a time ([`Scope::chunk`]) into the open
//! column and pushes each one the chunk completes onto its own rtrb ring (invariant 2), separate from
//! the event ring so a visual never pushes out a `Lane` or a `Beat`. Everything is allocated and its
//! pages touched in [`Scope::new`]; the fold allocates, logs, locks and waits for nothing (invariant
//! 5), and a full ring drops the column it could not take and counts it ([`Scope::dropped`]) rather
//! than wait.
//!
//! The reader splices columns by their `frame`: each one starts [`scope_bin_frames`] after the one
//! before it, so a break in that sequence is every loss there is — a column the full ring refused, the
//! open column a device-frame skip discarded (its frames were never played), a batch the feed capped, a
//! new engine. The feed reads the jump as its `gap` and the UI drops the trace it holds
//! (`src-tauri/src/engine_io/feed.rs`); the drop counter is the diagnostic beside it, not its trigger.
//!
//! Two limits, both accepted rather than paid for:
//!
//! - **A chunk that crosses a column boundary gives its whole envelope to BOTH columns**, so a
//!   transient can be drawn up to one chunk (at most a 128-frame quantum, 2.7 ms at 48 kHz) outside the
//!   column it belongs to. Splitting chunks on column boundaries would change the engine's chunking —
//!   and with it every block-size-identical render — for about three pixels of the stage.
//! - **A driver period the driver drops without its overload report leaves `ProcessContext::frame`
//!   contiguous** (`src-tauri/AGENTS.md` § Open threads), so no skip fires and the trace draws straight
//!   through a break the player heard. The scope can only see the losses the device side reports.
//!
//! The master lags the lanes by the limiter's pre-delay: a lane's column covers the frames it was mixed
//! at, the master's the frames that left the device, and the limiter delays everything it carries
//! ([`crate::Engine::limiter_latency`], about 6 ms). The look places both by frame, so a transient
//! shows in its lane about a column and a half before the master.
//!
//! While the scope is off (the default: the stage view is closed) nothing is folded and nothing is
//! pushed, so the taps cost the jam nothing.

use rtrb::{Consumer, Producer, RingBuffer};

use crate::api::TRACK_COUNT;
use crate::grid::Frame;

/// Sources a batch carries, in order: the five lanes after their FX, the monitor, the master.
pub const SCOPE_SOURCES: usize = TRACK_COUNT + 2;
/// The monitor's place in a batch: the live wet signal plus the input sends, under the master volume.
pub const SCOPE_MONITOR: usize = TRACK_COUNT;
/// The master's place: the engine's output as the device takes it — the master bus under the master
/// volume, through the limiter, with the monitor summed in (`engine.rs`: the monitor joins after the
/// limiter, so a tap before it would read neither the limiter's gain reduction nor the live guitar).
pub const SCOPE_MASTER: usize = TRACK_COUNT + 1;
/// A scope column is 4 ms of sound: at 120 BPM a bar is 500 columns, which a 1400 px stage draws
/// about three pixels wide. Fixed, not grid-derived: no integer column count divides a bar at every
/// tempo and rate (96000 / 360 = 266.67 at 120 BPM and 48 kHz), and the UI places each column by its
/// own frame instead.
pub const SCOPE_BIN_MS: f64 = 4.0;
/// Columns the ring holds (about two seconds): a full ring keeps what it has and drops the column it
/// cannot take — the newest — counting it ([`Scope::dropped`]); it never waits. So a reader that comes
/// back finds the oldest work still there and sees the loss as a jump in `frame`.
pub const SCOPE_CAPACITY: usize = 512;

/// Frames one column covers at `sample_rate`: [`SCOPE_BIN_MS`] rounded, at least one frame.
pub fn scope_bin_frames(sample_rate: u32) -> usize {
    ((sample_rate as f64 * SCOPE_BIN_MS / 1000.0).round() as usize).max(1)
}

/// One finished column: each source's min and max over the `bin` frames from `frame` on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScopeBin {
    pub frame: Frame,
    pub lo: [f32; SCOPE_SOURCES],
    pub hi: [f32; SCOPE_SOURCES],
}

impl ScopeBin {
    /// An empty column at `frame`: every envelope at zero, which is where a fold starts.
    fn empty(frame: Frame) -> ScopeBin {
        ScopeBin { frame, lo: [0.0; SCOPE_SOURCES], hi: [0.0; SCOPE_SOURCES] }
    }
}

/// The audio thread's end of the scope taps: the open column, the ring's producer, the drop count and
/// the on flag.
pub struct Scope {
    tx: Producer<ScopeBin>,
    /// Frames a column covers ([`scope_bin_frames`]).
    bin: usize,
    on: bool,
    /// The open column. Its envelope starts at zero on both sides and folds from there: a column is
    /// drawn from its min to its max through the baseline anyway, so spanning zero by construction
    /// costs the drawing nothing and removes the "no frames yet" case the reader would have to carry.
    open: ScopeBin,
    /// Frames folded into the open column so far (0: it is empty and starts wherever the next chunk
    /// does).
    filled: usize,
    /// The device frame the next chunk is expected at: anything else is a skip.
    next: Frame,
    dropped: u64,
}

impl Scope {
    /// Allocates the ring and touches every slot (a push into an untouched page would fault inside the
    /// callback): build it off the audio thread, with the engine.
    pub(crate) fn new(sample_rate: u32) -> (Scope, Consumer<ScopeBin>) {
        let (mut tx, mut rx) = RingBuffer::new(SCOPE_CAPACITY);
        while tx.push(ScopeBin::empty(0)).is_ok() {}
        while rx.pop().is_ok() {}
        let scope = Scope { tx, bin: scope_bin_frames(sample_rate), on: false, open: ScopeBin::empty(0), filled: 0, next: 0, dropped: 0 };
        (scope, rx)
    }

    /// Whether the UI asked for columns (`Command::SetScope`).
    pub fn on(&self) -> bool {
        self.on
    }

    /// `Command::SetScope`: a view that closes stops the fold at once, and one that opens starts its
    /// trace at the next column (the open column is dropped either way, so no column ever covers
    /// frames from both sides of the switch). A command that changes nothing does nothing: the value
    /// is sent again by a settings replay and by a UI that need not track it, and clearing the open
    /// column on every one of those would mean no column ever finished.
    pub(crate) fn set_on(&mut self, on: bool) {
        if on == self.on {
            return;
        }
        self.on = on;
        self.filled = 0;
    }

    /// Columns the full ring refused, plus the open columns a device-frame skip discarded, plus the
    /// chunks the engine could not stage ([`Scope::lost`]): the diagnostic beside the `frame` sequence
    /// the reader splices by.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Frames the engine folded nothing of, so no column covers them (its staging list was full). The
    /// next chunk then reads as a skip, which is what tells the reader its trace broke.
    pub(crate) fn lost(&mut self) {
        self.dropped += 1;
    }

    /// Fold one chunk: `len` frames from device frame `frame`, with each source's min in `lo` and max
    /// in `hi` over them, and push every column it completes. One chunk can complete several columns or
    /// none. On the audio thread: no allocation, no lock, no wait (invariant 5).
    pub(crate) fn chunk(&mut self, frame: Frame, len: usize, lo: &[f32; SCOPE_SOURCES], hi: &[f32; SCOPE_SOURCES]) {
        if !self.on || len == 0 {
            return;
        }
        if self.filled > 0 && frame != self.next {
            // A device-frame skip: the open column would span frames the device never played, so it
            // goes rather than being filled with silence, and the jump shows in the next column's frame.
            self.filled = 0;
            self.dropped += 1;
        }
        self.next = frame + len as Frame;
        if self.filled == 0 {
            self.open = ScopeBin::empty(frame);
        }
        let mut left = len;
        while left > 0 {
            // The chunk's whole envelope goes into every column it reaches, the smear the module doc
            // accepts: a chunk is at most one quantum and a column at least 176 frames (4 ms at
            // 44.1 kHz), so it reaches two only where it crosses a boundary.
            for ((open_lo, open_hi), (&x, &y)) in self.open.lo.iter_mut().zip(self.open.hi.iter_mut()).zip(lo.iter().zip(hi)) {
                *open_lo = open_lo.min(x);
                *open_hi = open_hi.max(y);
            }
            let take = (self.bin - self.filled).min(left);
            self.filled += take;
            left -= take;
            if self.filled == self.bin {
                let done = self.open;
                if self.tx.push(done).is_err() {
                    self.dropped += 1;
                }
                self.open = ScopeBin::empty(done.frame + self.bin as Frame);
                self.filled = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_column_is_four_milliseconds_rounded_and_never_zero() {
        assert_eq!(scope_bin_frames(48_000), 192);
        assert_eq!(scope_bin_frames(44_100), 176);
        assert_eq!(scope_bin_frames(1), 1, "a rate under a column's length still gives frames to fold");
    }

    /// Fold `chunks` of (frame, len, value on source 0) and return the columns pushed.
    fn fold(bin_rate: u32, chunks: &[(Frame, usize, f32)]) -> (Scope, Vec<ScopeBin>) {
        let (mut scope, mut rx) = Scope::new(bin_rate);
        scope.set_on(true);
        for &(frame, len, x) in chunks {
            let (mut lo, mut hi) = ([0.0; SCOPE_SOURCES], [0.0; SCOPE_SOURCES]);
            (lo[0], hi[0]) = (-x, x);
            scope.chunk(frame, len, &lo, &hi);
        }
        let mut out = Vec::new();
        while let Ok(bin) = rx.pop() {
            out.push(bin);
        }
        (scope, out)
    }

    #[test]
    fn chunks_of_any_length_make_columns_a_bin_apart_from_the_first_frame_folded() {
        let bin = scope_bin_frames(48_000) as Frame;
        let chunks: Vec<(Frame, usize, f32)> = (0..40).map(|k| (1_000 + k * 97, 97, 0.5)).collect();
        let (_, columns) = fold(48_000, &chunks);
        assert!(columns.len() >= 19, "40 chunks of 97 frames fill about 20 columns, not {}", columns.len());
        for (k, column) in columns.iter().enumerate() {
            assert_eq!(column.frame, 1_000 + k as Frame * bin, "column {k}");
            assert_eq!((column.lo[0], column.hi[0]), (-0.5, 0.5));
            assert_eq!((column.lo[1], column.hi[1]), (0.0, 0.0), "a silent source spans zero");
        }
    }

    #[test]
    fn a_skip_discards_the_open_column_and_the_next_one_breaks_the_frame_sequence() {
        // One whole column, then an open one the skip discards, then two more.
        let mut chunks: Vec<(Frame, usize, f32)> = (0..3).map(|k| (k * 100, 100, 0.25)).collect();
        chunks.push((10_300, 2 * scope_bin_frames(48_000), 0.25));
        let (scope, columns) = fold(48_000, &chunks);
        assert_eq!(columns.len(), 3, "the discarded open column is not drawn");
        assert_eq!(columns[0].frame, 0);
        assert_eq!(columns[1].frame, 10_300, "the trace starts over where the device went on");
        assert_eq!(scope.dropped(), 1, "the skip counted");
    }

    #[test]
    fn a_full_ring_drops_and_counts_rather_than_waiting() {
        let bin = scope_bin_frames(48_000);
        let chunks: Vec<(Frame, usize, f32)> = (0..SCOPE_CAPACITY + 8).map(|k| ((k * bin) as Frame, bin, 0.5)).collect();
        let (scope, columns) = fold(48_000, &chunks);
        assert_eq!(columns.len(), SCOPE_CAPACITY, "the ring holds what it holds");
        assert_eq!(scope.dropped(), 8, "every refused column counted");
        assert!(columns.windows(2).all(|w| w[1].frame == w[0].frame + bin as Frame), "what it holds is the oldest work, contiguous");
    }

    #[test]
    fn off_folds_nothing_and_a_switch_leaves_no_column_spanning_it() {
        let bin = scope_bin_frames(48_000);
        let (mut scope, mut rx) = Scope::new(48_000);
        let (lo, hi) = ([-1.0; SCOPE_SOURCES], [1.0; SCOPE_SOURCES]);
        for k in 0..100 {
            scope.chunk((k * bin) as Frame, bin, &lo, &hi);
        }
        assert!(rx.pop().is_err(), "off: nothing is pushed");
        scope.set_on(true);
        scope.chunk((100 * bin) as Frame, bin / 2, &lo, &hi);
        scope.set_on(false);
        scope.set_on(true);
        scope.chunk((100 * bin + bin / 2) as Frame, bin, &lo, &hi);
        let first = rx.pop().expect("a column after the switch");
        assert_eq!(first.frame, (100 * bin + bin / 2) as Frame, "the column open at the switch went with it");
    }

    /// The same value sent again changes nothing: a replay or a UI that re-sends it must not keep the
    /// open column from ever finishing.
    #[test]
    fn setting_the_flag_to_the_value_it_has_leaves_the_open_column_alone() {
        let bin = scope_bin_frames(48_000);
        let (mut scope, mut rx) = Scope::new(48_000);
        let (lo, hi) = ([-0.5; SCOPE_SOURCES], [0.5; SCOPE_SOURCES]);
        scope.set_on(true);
        for k in 0..bin {
            // One frame at a time, the flag re-sent before each: the column still completes.
            scope.set_on(true);
            scope.chunk(k as Frame, 1, &lo, &hi);
        }
        assert_eq!(rx.pop().map(|c| c.frame), Ok(0), "a column finished through the re-sent commands");
        scope.set_on(false);
        scope.set_on(false);
        assert!(!scope.on());
    }
}
