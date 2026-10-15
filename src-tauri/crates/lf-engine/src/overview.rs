//! OWNS: what the UI draws from the looper, shared with a reader that never takes the engine lock (the
//! host's feed thread, and its device owner's check that the engine holds audio): the master grid's
//! anchor, each lane's state, buffer, orientation and frames, and each buffer's waveform peaks. The
//! looper stores into it (relaxed atomic stores and bit sets: it never allocates, locks or waits); a
//! reader polls it ([`EngineHandle::overview`](crate::EngineHandle)). Each value is current on its own:
//! a reader may see one lane's update a chunk before another's, and a bin a chunk before the lane's
//! frames that reach it.
//!
//! Peaks belong to buffers, not lanes: one min/max pair per [`PEAK_FRAMES`] frames (the web looper's
//! `PEAK_FRAMES`), recomputed wherever a buffer is written (a take's capture, an overdub's sum, AUTO's
//! onset, the block jobs) and marked dirty for the reader. An undo, a kept RETAKE pass or a reverse
//! writes nothing: the lane shows another buffer, or the same one backwards ([`LaneView`]).
//!
//! So do the dropout marks: one bit per bin, set where a capture kept a frame the damage lane marked
//! (`api::damage`), copied and cleared by the looper with the audio they describe. Every store to a
//! mark dirties the bins it changed, after the data, as a peak's does, so a cleared or replaced mark
//! reaches the reader as a changed bin. A lane's frames are stored with Release and read with Acquire:
//! a reader that sees a longer view sees the marks set before it.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering::{Acquire, Relaxed, Release}};

use crate::api::{LaneState, TRACK_COUNT};
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
    /// The lane's state, as `state_code` stores it.
    state: AtomicU8,
}

/// One buffer's bins: min and max as f32 bits, a dirty bit per bin since the reader last took it, a
/// dropout mark per bin, and how many writes the buffer has seen (a session snapshot checks it did not
/// change under the copy).
struct Peaks {
    min: Box<[AtomicU32]>,
    max: Box<[AtomicU32]>,
    dirty: Box<[AtomicU64]>,
    marks: Box<[AtomicU64]>,
    writes: AtomicU64,
}

impl Peaks {
    fn new(bins: usize) -> Peaks {
        // Built by writing every element: the pages are touched here, not in the callback.
        let words = |n: usize| (0..n).map(|_| AtomicU32::new(0)).collect::<Box<[AtomicU32]>>();
        let bits = || (0..bins.div_ceil(64)).map(|_| AtomicU64::new(0)).collect::<Box<[AtomicU64]>>();
        Peaks { min: words(bins), max: words(bins), dirty: bits(), marks: bits(), writes: AtomicU64::new(0) }
    }
}

/// The mark words covering bins `[lo, hi)`: `(word, mask)` per word they touch.
fn bin_words(lo: usize, hi: usize, mut each: impl FnMut(usize, u64)) {
    let mut bin = lo;
    while bin < hi {
        let (w, b) = (bin / 64, bin % 64);
        let n = (hi - bin).min(64 - b);
        let mask = if n == 64 { u64::MAX } else { ((1u64 << n) - 1) << b };
        each(w, mask);
        bin += n;
    }
}

/// One lane as the UI draws it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneView {
    /// Anything but EMPTY holds audio, or will: a loop, a take in flight or armed, a kept RETAKE pass
    /// (whose `frames` read 0 until the next pass captures).
    pub state: LaneState,
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
        LaneView {
            state: state_of(c.state.load(Relaxed)),
            buf: c.buf.load(Relaxed) as usize,
            frames: c.frames.load(Acquire),
            reversed: c.reversed.load(Relaxed),
        }
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
        c.state.store(state_code(view.state), Relaxed);
        c.buf.store(view.buf as u32, Relaxed);
        c.frames.store(view.frames, Release);
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

    /// `data` (buffer `buf`) was zeroed at positions `lo..hi`: the bins wholly inside are 0 with no read,
    /// the ones at its edges recomputed as [`Overview::touch`] does, and all of them marked for the reader.
    pub(crate) fn touch_zeroed(&self, buf: usize, data: &[f32], lo: usize, hi: usize, valid: usize) {
        let p = &self.buffers[buf];
        let (a, b) = (lo.div_ceil(PEAK_FRAMES), (hi / PEAK_FRAMES).min(p.min.len()));
        if a >= b {
            self.touch(buf, data, lo, hi, valid);
            return;
        }
        // A write whatever its edges: a snapshot reads the count, and whole bins touch no edge.
        p.writes.fetch_add(1, Relaxed);
        self.touch(buf, data, lo, a * PEAK_FRAMES, valid);
        for bin in a..b {
            p.min[bin].store(0f32.to_bits(), Relaxed);
            p.max[bin].store(0f32.to_bits(), Relaxed);
        }
        bin_words(a, b, |w, mask| {
            p.dirty[w].fetch_or(mask, Release);
        });
        self.touch(buf, data, b * PEAK_FRAMES, hi, valid);
    }

    /// Buffer `buf`'s bins set whole, from `bins` ((min, max) from bin 0; the rest cleared), its marks
    /// cleared (a loaded session holds none), and all dirtied for the reader: a session load, whose host
    /// computed them.
    pub(crate) fn set_bins(&self, buf: usize, bins: &[(f32, f32)]) {
        let p = &self.buffers[buf];
        p.writes.fetch_add(1, Relaxed);
        for (k, (min, max)) in p.min.iter().zip(p.max.iter()).enumerate() {
            let (lo, hi) = bins.get(k).copied().unwrap_or((0.0, 0.0));
            min.store(lo.to_bits(), Relaxed);
            max.store(hi.to_bits(), Relaxed);
        }
        for mark in p.marks.iter() {
            mark.store(0, Relaxed);
        }
        for word in p.dirty.iter() {
            word.store(u64::MAX, Release);
        }
    }

    /// Buffer `buf`'s bins `[lo, hi)` (clamped to the buffer) take a dropout mark.
    pub(crate) fn mark_bins(&self, buf: usize, lo: usize, hi: usize) {
        let p = &self.buffers[buf];
        let hi = hi.min(p.min.len());
        bin_words(lo, hi, |w, mask| {
            let old = p.marks[w].fetch_or(mask, Relaxed);
            if old & mask != mask {
                p.dirty[w].fetch_or(mask & !old, Release);
            }
        });
    }

    /// Buffer `buf`'s positions `[lo, hi)` hold a frame the damage lane marked: their bins take a mark.
    pub(crate) fn mark(&self, buf: usize, lo: usize, hi: usize) {
        if lo < hi {
            self.mark_bins(buf, lo / PEAK_FRAMES, (hi - 1) / PEAK_FRAMES + 1);
        }
    }

    /// Whether buffer `buf`'s bin `bin` holds a dropout mark.
    pub fn marked(&self, buf: usize, bin: usize) -> bool {
        self.buffers[buf].marks.get(bin / 64).is_some_and(|w| w.load(Relaxed) & (1 << (bin % 64)) != 0)
    }

    /// Whether heard bin `h` of a lane that shows buffer `buf`, `frames` long, holds a mark. Forward, the
    /// buffer's bin `h`. Reversed, the marks map through frames, not through the bin count: the OR of the
    /// buffer bins (at most two) holding the frames it plays, `[frames - (h + 1)·PEAK_FRAMES, frames -
    /// h·PEAK_FRAMES)`, so a mark sits where it is heard on a loop that is not whole bins.
    pub fn heard_marked(&self, buf: usize, frames: usize, reversed: bool, h: usize) -> bool {
        if !reversed {
            return self.marked(buf, h);
        }
        let hi = frames.saturating_sub(h * PEAK_FRAMES);
        let lo = frames.saturating_sub((h + 1) * PEAK_FRAMES);
        hi > lo && (lo / PEAK_FRAMES..=(hi - 1) / PEAK_FRAMES).any(|b| self.marked(buf, b))
    }

    /// The heard bins buffer bin `b` reaches on a lane `frames` long ([`Overview::heard_marked`]): `[b, b]`
    /// forward, the bins of the frames it holds played backwards reversed (at most two). `None` past the
    /// lane's frames.
    pub fn heard_bins(frames: usize, reversed: bool, b: usize) -> Option<(usize, usize)> {
        let lo = b * PEAK_FRAMES;
        if lo >= frames {
            return None;
        }
        if !reversed {
            return Some((b, b));
        }
        let hi = (lo + PEAK_FRAMES).min(frames);
        // Heard position of buffer position p: frames - 1 - p.
        Some(((frames - hi) / PEAK_FRAMES, (frames - 1 - lo) / PEAK_FRAMES))
    }

    /// Buffer `buf`'s mark words, one per 64 bins.
    pub(crate) fn mark_words(&self, buf: usize) -> impl Iterator<Item = u64> + '_ {
        self.buffers[buf].marks.iter().map(|w| w.load(Relaxed))
    }

    /// Buffer `buf`'s marks set whole from `words`: every bin that changes is dirtied, a cleared one too.
    pub(crate) fn set_marks(&self, buf: usize, words: impl IntoIterator<Item = u64>) {
        let p = &self.buffers[buf];
        let mut words = words.into_iter();
        for (mark, dirty) in p.marks.iter().zip(p.dirty.iter()) {
            let new = words.next().unwrap_or(0);
            let old = mark.swap(new, Relaxed);
            if old != new {
                dirty.fetch_or(old ^ new, Release);
            }
        }
    }

    /// Buffer `dst`'s marks become `src`'s (the audio copied whole).
    pub(crate) fn copy_marks(&self, src: usize, dst: usize) {
        if src != dst {
            self.set_marks(dst, self.buffers[src].marks.iter().map(|w| w.load(Relaxed)));
        }
    }

    /// Buffer `buf` takes new audio from position 0: no mark is left.
    pub(crate) fn clear_marks(&self, buf: usize) {
        self.set_marks(buf, std::iter::empty());
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

/// A lane state as an atomic holds it (`state_of` reads it back).
fn state_code(state: LaneState) -> u8 {
    match state {
        LaneState::Empty => 0,
        LaneState::Recording => 1,
        LaneState::Overdubbing => 2,
        LaneState::Playing => 3,
        LaneState::Stopped => 4,
    }
}

fn state_of(code: u8) -> LaneState {
    match code {
        1 => LaneState::Recording,
        2 => LaneState::Overdubbing,
        3 => LaneState::Playing,
        4 => LaneState::Stopped,
        _ => LaneState::Empty,
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

    #[test]
    fn a_lane_reads_back_every_state_it_was_given() {
        let o = Overview::new(1, PEAK_FRAMES);
        for state in [LaneState::Empty, LaneState::Recording, LaneState::Overdubbing, LaneState::Playing, LaneState::Stopped] {
            let view = LaneView { state, buf: 0, frames: 7, reversed: true };
            o.set_lane(1, view);
            assert_eq!(o.lane(1), view);
        }
    }
}
