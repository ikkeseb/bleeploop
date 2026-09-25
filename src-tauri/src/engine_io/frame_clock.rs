//! OWNS: the callback's stamp of where the device clock is, and the frame a press lands on.
//!
//! Each output callback publishes the instant it entered and the device frame of its first sample
//! (a seqlock over atomics: the callback never waits, a reader retries). A press (a MIDI pedal) maps
//! its arrival instant to the frame the engine was rendering then, plus one block: always the next
//! block or later, so the engine applies it on that exact frame, jitter-free, instead of at whichever
//! block start comes first. The UI's gestures land at the next block start instead (Stage 2); both are
//! judged on the render clock, so they sit the same output latency ahead of what the player heard.

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use lf_engine::grid::Frame;

/// The process's time origin: every stamp is nanoseconds after it.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Nanoseconds from the process's time origin to `at` (0 for an instant before it).
pub fn stamp(at: Instant) -> u64 {
    at.saturating_duration_since(epoch()).as_nanos() as u64
}

#[derive(Default)]
struct Cell {
    /// Odd while a write is in progress.
    seq: AtomicU64,
    entry_ns: AtomicU64,
    frame: AtomicI64,
    block: AtomicU32,
    rate: AtomicU32,
}

/// Cheap to clone; the callback writes, any thread reads.
#[derive(Clone, Default)]
pub struct FrameClock(Arc<Cell>);

impl FrameClock {
    pub fn new() -> FrameClock {
        epoch();
        FrameClock::default()
    }

    /// The callback: it entered at `entry` and renders `block` frames from `frame` at `rate`.
    /// Wait-free (the callback is the only writer).
    pub fn publish(&self, entry: Instant, frame: Frame, block: u32, rate: u32) {
        let c = &self.0;
        let seq = c.seq.load(Relaxed);
        c.seq.store(seq.wrapping_add(1), Relaxed);
        std::sync::atomic::fence(Release);
        c.entry_ns.store(stamp(entry), Relaxed);
        c.frame.store(frame, Relaxed);
        c.block.store(block, Relaxed);
        c.rate.store(rate, Relaxed);
        c.seq.store(seq.wrapping_add(2), Release);
    }

    /// No callback runs (the owner, before it drops the streams): presses fall back to "now".
    pub fn clear(&self) {
        self.publish(epoch(), 0, 0, 0);
    }

    /// The last stamp: (entry, frame, block, rate), or `None` while no callback runs.
    fn read(&self) -> Option<(u64, Frame, u32, u32)> {
        let c = &self.0;
        for _ in 0..64 {
            let before = c.seq.load(Acquire);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let v = (c.entry_ns.load(Relaxed), c.frame.load(Relaxed), c.block.load(Relaxed), c.rate.load(Relaxed));
            std::sync::atomic::fence(Acquire);
            if c.seq.load(Relaxed) == before {
                return (v.3 != 0).then_some(v);
            }
        }
        None
    }

    /// The frame a press that arrived at `at` lands on: the render position then, plus one block.
    /// `None` while no callback runs (send it unstamped: it lands at the next block start).
    pub fn press_frame(&self, at: Instant) -> Option<Frame> {
        let (entry_ns, frame, block, rate) = self.read()?;
        let since = stamp(at).saturating_sub(entry_ns) as f64 / 1e9;
        // A press stamped long after the last callback (the device stalled) still lands one block on.
        let ahead = ((since * rate as f64).round() as Frame).min(block as Frame);
        Some(frame + ahead + block as Frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_press_lands_one_block_after_the_render_position() {
        let clock = FrameClock::new();
        assert_eq!(clock.press_frame(Instant::now()), None, "no callback yet");
        let entry = Instant::now();
        clock.publish(entry, 48_000, 256, 48_000);
        // 1 ms after the callback entered: 48 frames into its block, then one block on.
        assert_eq!(clock.press_frame(entry + Duration::from_millis(1)), Some(48_000 + 48 + 256));
        // Before it entered (a press stamped earlier than the callback): the block start plus a block.
        assert_eq!(clock.press_frame(entry - Duration::from_millis(1)), Some(48_000 + 256));
        // Long after (a stalled device): capped at one block into it.
        assert_eq!(clock.press_frame(entry + Duration::from_secs(1)), Some(48_000 + 256 + 256));
        clock.clear();
        assert_eq!(clock.press_frame(Instant::now()), None);
    }
}
