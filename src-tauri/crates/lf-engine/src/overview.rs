//! OWNS: what the UI draws from the looper, shared with a reader that never takes the engine lock (the
//! host's feed thread): the master grid's anchor, and each lane's buffer, orientation and frames. The
//! looper stores into it at every publish (relaxed atomic stores: it never allocates, locks or waits);
//! a reader polls it ([`EngineHandle::overview`](crate::EngineHandle)). Each value is current on its
//! own: a reader may see one lane's update a chunk before another's.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering::Relaxed};

use crate::api::TRACK_COUNT;
use crate::grid::Frame;

pub struct Overview {
    grid: AtomicI64,
    lanes: [LaneCell; TRACK_COUNT],
}

#[derive(Default)]
struct LaneCell {
    buf: AtomicU32,
    frames: AtomicI64,
    reversed: AtomicBool,
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
    pub(crate) fn new() -> Overview {
        Overview { grid: AtomicI64::new(0), lanes: std::array::from_fn(|_| LaneCell::default()) }
    }

    /// The master grid's anchor (`Looper::anchor`): loop position 0 plays at `grid + k * master`.
    pub fn grid(&self) -> Frame {
        self.grid.load(Relaxed)
    }

    pub fn lane(&self, i: usize) -> LaneView {
        let c = &self.lanes[i];
        LaneView { buf: c.buf.load(Relaxed) as usize, frames: c.frames.load(Relaxed), reversed: c.reversed.load(Relaxed) }
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
}
