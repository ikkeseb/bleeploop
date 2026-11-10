//! OWNS: the tuner's audio-thread side: whether the tuner is on, and the tap that hands the raw input of
//! both slots, frame by frame, to a pitch detector off the audio thread. The engine owns the rest of its
//! rules (`engine.rs`): the monitor's mute while it is on, its refusal while a take exists and its
//! closing when one comes to exist, its `Event::Tuner`, and a stopped device closing it.
//!
//! While the tuner is on, the engine copies each chunk it renders onto the tap's own rtrb ring
//! ([`TunerTap::chunk`]), one [`TunerFrame`] per device frame, after that frame's commands, so the tap
//! starts and stops on the exact frame of the switch. Its own ring, as the scope's, so a reading never
//! pushes out a `Lane` or a `Beat` (invariant 2). The input is the slot's raw capture channel, before
//! the slot's live gate and its plugin: a slot that is not live is tapped too, and the amp-sim's gain
//! and noise never reach the detector.
//!
//! The reader resets its analysis when the epoch changes: the engine starts a new one wherever the
//! frames that follow do not continue the ones before (the tuner turned on, a frame the full ring
//! refused, a jump in the device frame, an xrun or a damaged block). Everything is allocated and its
//! pages touched in [`TunerTap::new`]; the copy allocates, logs, locks and waits for nothing
//! (invariant 5), and a full ring drops the frames it cannot take and counts them
//! ([`TunerTap::dropped`]). Off, nothing is copied.

use rtrb::{Consumer, Producer, RingBuffer};

use crate::api::SLOT_COUNT;
use crate::grid::Frame;

/// Seconds of input the ring holds: the reader drains it about 60 times a second, so it fills only
/// when the reader stops.
const TUNER_SECONDS: f64 = 0.35;

/// Frames the ring holds at `sample_rate`: [`TUNER_SECONDS`] of input, at least one frame.
pub fn tuner_capacity(sample_rate: u32) -> usize {
    ((sample_rate as f64 * TUNER_SECONDS).ceil() as usize).max(1)
}

/// One device frame of the raw input: each slot's capture channel at `frame`. Frames of one `epoch`
/// follow each other with no frame missing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TunerFrame {
    pub frame: Frame,
    pub epoch: u32,
    pub x: [f32; SLOT_COUNT],
}

/// The audio thread's end of the tuner: the on flag, the ring's producer, the epoch and the drop count.
pub struct TunerTap {
    tx: Producer<TunerFrame>,
    on: bool,
    epoch: u32,
    /// The next frame pushed starts a new epoch, whatever its frame.
    broken: bool,
    /// The device frame that continues the last one pushed: anything else is a jump.
    next: Frame,
    dropped: u64,
}

impl TunerTap {
    /// Allocates the ring and touches every slot (a push into an untouched page would fault inside the
    /// callback): build it off the audio thread, with the engine. Off, at epoch 0.
    pub(crate) fn new(sample_rate: u32) -> (TunerTap, Consumer<TunerFrame>) {
        let (mut tx, mut rx) = RingBuffer::new(tuner_capacity(sample_rate));
        let empty = TunerFrame { frame: 0, epoch: 0, x: [0.0; SLOT_COUNT] };
        while tx.push(empty).is_ok() {}
        while rx.pop().is_ok() {}
        (TunerTap { tx, on: false, epoch: 0, broken: true, next: 0, dropped: 0 }, rx)
    }

    /// Whether the tuner is on.
    pub fn on(&self) -> bool {
        self.on
    }

    /// The epoch the last frame pushed carried (0 before the first).
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Frames the full ring refused: the diagnostic beside the epoch the reader resets by.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Switch the tuner; whether it changed. Turning on starts a new epoch.
    pub(crate) fn set_on(&mut self, on: bool) -> bool {
        if on == self.on {
            return false;
        }
        self.on = on;
        self.broken |= on;
        true
    }

    /// The device lost input before or within this block (an xrun, a damaged block): the next frame
    /// pushed starts a new epoch.
    pub(crate) fn gap(&mut self) {
        self.broken = true;
    }

    /// Copy one chunk while on: `inputs[s]` is slot `s`'s raw input over the chunk's frames, the first
    /// at device frame `frame`. What the ring cannot take is dropped and counted, and the frames after
    /// it start a new epoch. On the audio thread: no allocation, no lock, no wait (invariant 5).
    pub(crate) fn chunk(&mut self, frame: Frame, inputs: [&[f32]; SLOT_COUNT]) {
        let len = inputs[0].len();
        if !self.on || len == 0 {
            return;
        }
        let take = len.min(self.tx.slots());
        if take > 0 && (self.broken || frame != self.next) {
            // One new epoch per break, taken by the first frame pushed after it.
            self.epoch = self.epoch.wrapping_add(1);
            self.broken = false;
        }
        self.next = frame + len as Frame;
        if take < len {
            self.dropped += (len - take) as u64;
            self.broken = true;
        }
        if take == 0 {
            return;
        }
        let epoch = self.epoch;
        if let Ok(chunk) = self.tx.write_chunk_uninit(take) {
            chunk.fill_from_iter((0..take).map(|k| TunerFrame { frame: frame + k as Frame, epoch, x: std::array::from_fn(|s| inputs[s][k]) }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut Consumer<TunerFrame>) -> Vec<TunerFrame> {
        let mut out = Vec::new();
        while let Ok(f) = rx.pop() {
            out.push(f);
        }
        out
    }

    #[test]
    fn the_ring_holds_a_third_of_a_second_and_more() {
        assert_eq!(tuner_capacity(48_000), 16_800);
        assert_eq!(tuner_capacity(44_100), 15_435);
        assert_eq!(tuner_capacity(1), 1);
    }

    #[test]
    fn off_copies_nothing_and_on_copies_each_frame_of_both_inputs() {
        let (mut tap, mut rx) = TunerTap::new(48_000);
        let (a, b) = ([0.1f32, 0.2, 0.3], [-0.1f32, -0.2, -0.3]);
        tap.chunk(100, [&a, &b]);
        assert!(drain(&mut rx).is_empty(), "off: nothing");
        assert!(tap.set_on(true) && !tap.set_on(true), "one switch");
        tap.chunk(100, [&a, &b]);
        tap.chunk(103, [&a[..1], &b[..1]]);
        let frames = drain(&mut rx);
        assert_eq!(frames.len(), 4);
        assert!(frames.iter().all(|f| f.epoch == 1));
        assert_eq!(frames.iter().map(|f| f.frame).collect::<Vec<_>>(), vec![100, 101, 102, 103]);
        assert_eq!(frames[1].x, [0.2, -0.2]);
    }

    #[test]
    fn a_jump_a_gap_a_switch_and_a_full_ring_each_start_an_epoch() {
        let (mut tap, mut rx) = TunerTap::new(200);
        let x = [0.5f32; 10];
        tap.set_on(true);
        tap.chunk(0, [&x, &x]);
        tap.chunk(20, [&x, &x]);
        tap.gap();
        tap.chunk(30, [&x, &x]);
        tap.set_on(false);
        tap.set_on(true);
        tap.chunk(40, [&x, &x]);
        let epochs: Vec<u32> = drain(&mut rx).iter().step_by(10).map(|f| f.epoch).collect();
        assert_eq!(epochs, vec![1, 2, 3, 4]);
        // The ring holds 70 frames: 90 offered, 20 dropped (the last chunk wholly), one new epoch after.
        for k in 0..9 {
            tap.chunk(50 + k * 10, [&x, &x]);
        }
        assert_eq!(tap.dropped(), 20);
        let kept = drain(&mut rx);
        assert_eq!(kept.len(), 70);
        assert!(kept.iter().all(|f| f.epoch == 4) && kept.windows(2).all(|w| w[1].frame == w[0].frame + 1));
        tap.chunk(140, [&x, &x]);
        assert_eq!(drain(&mut rx)[0].epoch, 5, "after the drop, one new epoch");
    }
}
