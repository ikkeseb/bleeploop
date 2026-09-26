//! OWNS: what the UI draws from the looper, shared with a reader that never takes the engine lock (the
//! host's feed thread): the master grid's anchor, each lane's buffer, orientation and frames, and each
//! buffer's waveform peaks. The looper stores into it (relaxed atomic stores and bit sets: it never
//! allocates, locks or waits); a reader polls it ([`EngineHandle::overview`](crate::EngineHandle)). Each
//! value is current on its own: a reader may see one lane's update a chunk before another's, and a bin
//! a chunk before the lane's frames that reach it.
//!
//! Peaks belong to buffers, not lanes: one min/max pair per [`PEAK_FRAMES`] frames (the web looper's
//! `PEAK_FRAMES`), recomputed wherever a buffer is written (a take's capture, an overdub's sum, AUTO's
//! onset, the block jobs) and marked dirty for the reader. An undo, a kept RETAKE pass or a reverse
//! writes nothing: the lane shows another buffer, or the same one backwards ([`LaneView`]).

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::{Acquire, Relaxed, Release}};

use crate::api::TRACK_COUNT;
use crate::grid::Frame;

/// Frames per waveform bin.
pub const PEAK_FRAMES: usize = 1024;

pub struct Overview {
    grid: AtomicI64,
    lanes: [LaneCell; TRACK_COUNT],
    buffers: Box<[Peaks]>,
    capacity: usize,
}

#[derive(Default)]
struct LaneCell {
    buf: AtomicU32,
    frames: AtomicI64,
    reversed: AtomicBool,
}

/// One buffer's bins: min and max as f32 bits, a dirty bit per bin since the reader last took it, and
/// how many writes the buffer has seen (a session snapshot checks it did not change under the copy).
struct Peaks {
    min: Box<[AtomicU32]>,
    max: Box<[AtomicU32]>,
    dirty: Box<[AtomicU64]>,
    writes: AtomicU64,
}

impl Peaks {
    fn new(bins: usize) -> Peaks {
        // Built by writing every element: the pages are touched here, not in the callback.
        let words = |n: usize| (0..n).map(|_| AtomicU32::new(0)).collect::<Box<[AtomicU32]>>();
        Peaks { min: words(bins), max: words(bins), dirty: (0..bins.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(), writes: AtomicU64::new(0) }
    }
}

/// One lane as the UI draws it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LaneView {
    /// The buffer the lane shows (its live one): an undo or a kept RETAKE pass changes it.
    pub buf: usize,
    /// What it holds: the frames captured so far while it records a take, its loop once committed
    /// (overdubbing included), 0 while empty or waiting for its downbeat.
    pub frames: Frame,
    /// It plays its buffer backwards (the orientation REVERSE set, before the loop boundary swaps it in).
    pub reversed: bool,
}

impl Overview {
    /// For `buffers` buffers of `capacity` frames.
    pub(crate) fn new(buffers: usize, capacity: usize) -> Overview {
        let bins = capacity.div_ceil(PEAK_FRAMES);
        Overview {
            grid: AtomicI64::new(0),
            lanes: std::array::from_fn(|_| LaneCell::default()),
            buffers: (0..buffers).map(|_| Peaks::new(bins)).collect(),
            capacity,
        }
    }

    /// The master grid's anchor (`Looper::anchor`): loop position 0 plays at `grid + k * master`.
    pub fn grid(&self) -> Frame {
        self.grid.load(Relaxed)
    }

    pub fn lane(&self, i: usize) -> LaneView {
        let c = &self.lanes[i];
        LaneView { buf: c.buf.load(Relaxed) as usize, frames: c.frames.load(Relaxed), reversed: c.reversed.load(Relaxed) }
    }

    /// Bins a buffer holds.
    pub fn bins(&self) -> usize {
        self.buffers.first().map_or(0, |p| p.min.len())
    }

    /// Buffer `buf`'s bin `bin`: its (min, max).
    pub fn bin(&self, buf: usize, bin: usize) -> (f32, f32) {
        let p = &self.buffers[buf];
        (f32::from_bits(p.min[bin].load(Relaxed)), f32::from_bits(p.max[bin].load(Relaxed)))
    }

    /// Visit, and clear, buffer `buf`'s bins that changed since the last take. A bin written after its
    /// bit is cleared is marked again, so nothing is missed; its value may be seen twice.
    pub fn take_dirty(&self, buf: usize, mut visit: impl FnMut(usize)) {
        for (w, word) in self.buffers[buf].dirty.iter().enumerate() {
            let mut bits = word.swap(0, Acquire);
            while bits != 0 {
                visit(w * 64 + bits.trailing_zeros() as usize);
                bits &= bits - 1;
            }
        }
    }

    pub(crate) fn set_grid(&self, grid: Frame) {
        self.grid.store(grid, Relaxed);
    }

    pub(crate) fn set_lane(&self, i: usize, view: LaneView) {
        let c = &self.lanes[i];
        c.buf.store(view.buf as u32, Relaxed);
        c.frames.store(view.frames, Relaxed);
        c.reversed.store(view.reversed, Relaxed);
    }

    /// `data` (buffer `buf`) changed at positions `lo..hi`: recompute the bins they fall in from what the
    /// buffer holds up to `valid` (past it is an older take), and mark them for the reader.
    pub(crate) fn touch(&self, buf: usize, data: &[f32], lo: usize, hi: usize, valid: usize) {
        let p = &self.buffers[buf];
        if lo >= hi || p.min.is_empty() {
            return;
        }
        p.writes.fetch_add(1, Relaxed);
        let valid = valid.min(data.len());
        for bin in lo / PEAK_FRAMES..=((hi - 1) / PEAK_FRAMES).min(p.min.len() - 1) {
            let from = bin * PEAK_FRAMES;
            let to = (from + PEAK_FRAMES).min(valid);
            let (min, max) = data.get(from..to).unwrap_or(&[]).iter().fold((0.0f32, 0.0f32), |(lo, hi), &x| (lo.min(x), hi.max(x)));
            p.min[bin].store(min.to_bits(), Relaxed);
            p.max[bin].store(max.to_bits(), Relaxed);
            p.dirty[bin / 64].fetch_or(1 << (bin % 64), Release);
        }
    }

    /// Buffer `buf`'s bins set whole, from `bins` ((min, max) from bin 0; the rest cleared), and
    /// marked for the reader: a session load, whose host computed them.
    pub(crate) fn set_bins(&self, buf: usize, bins: &[(f32, f32)]) {
        let p = &self.buffers[buf];
        p.writes.fetch_add(1, Relaxed);
        for (k, (min, max)) in p.min.iter().zip(p.max.iter()).enumerate() {
            let (lo, hi) = bins.get(k).copied().unwrap_or((0.0, 0.0));
            min.store(lo.to_bits(), Relaxed);
            max.store(hi.to_bits(), Relaxed);
        }
        for word in p.dirty.iter() {
            word.store(u64::MAX, Release);
        }
    }

    /// How many writes buffer `buf` has seen.
    pub(crate) fn writes(&self, buf: usize) -> u64 {
        self.buffers[buf].writes.load(Relaxed)
    }

    /// The frames a buffer holds.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Mark every bin of buffer `buf` taken (the reader sends the whole buffer instead).
    pub fn clear_dirty(&self, buf: usize) {
        for word in self.buffers[buf].dirty.iter() {
            word.swap(0, Acquire);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_touch_recomputes_whole_bins_up_to_the_valid_end_and_marks_them_once() {
        let o = Overview::new(1, 4 * PEAK_FRAMES);
        let mut data = vec![0.0f32; 4 * PEAK_FRAMES];
        data[10] = -0.5;
        data[PEAK_FRAMES + 3] = 0.75;
        data[PEAK_FRAMES + 600] = 0.9; // past the valid end: an older take
        o.touch(0, &data, 5, PEAK_FRAMES + 4, PEAK_FRAMES + 500);
        assert_eq!(o.bin(0, 0), (-0.5, 0.0));
        assert_eq!(o.bin(0, 1), (0.0, 0.75), "the old take past the valid end is not drawn");
        let mut seen = Vec::new();
        o.take_dirty(0, |b| seen.push(b));
        assert_eq!(seen, [0, 1]);
        o.take_dirty(0, |b| seen.push(b));
        assert_eq!(seen, [0, 1], "taken once");
        o.touch(0, &data, 3 * PEAK_FRAMES, 4 * PEAK_FRAMES, 4 * PEAK_FRAMES);
        o.clear_dirty(0);
        o.take_dirty(0, |b| seen.push(b));
        assert_eq!(seen, [0, 1]);
    }
}
