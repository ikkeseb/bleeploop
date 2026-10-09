//! OWNS: the callback's stamp of where the device clock is, and the frame a press lands on.
//!
//! Each output callback publishes the instant it entered and the device frame of its first sample
//! (a seqlock over atomics: the callback never waits, a reader retries). A press (a MIDI pedal) maps
//! its arrival instant to the frame the engine was rendering then, plus one block: always the next
//! block or later, so the engine applies it on that exact frame, jitter-free, instead of at whichever
//! block start comes first. The UI's gestures land at the next block start instead (Stage 2); both are
//! judged on the render clock, so they sit the same output latency ahead of what the player heard.
//!
//! DEV builds also keep the last [`HISTORY`] stamps (each slot a seqlock of its own; the writer claims
//! a slot with one atomic add and never waits): a frame the engine applied a note on converts to an
//! instant through the stamp of the block that rendered it ([`frame_instant`]), on the same clock as
//! the instants the MIDI benchmark stamps (`midi_bench`), never the feed's wall-clock anchor.

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
    #[cfg(debug_assertions)]
    history: History,
}

/// DEV: the stamps the history keeps: some 22 s of callbacks at 128 frames and 48 kHz, 5.5 s at 32.
/// A reader that falls further behind loses the oldest (counted).
#[cfg(debug_assertions)]
pub const HISTORY: usize = 8192;

/// DEV: one callback's stamp as the history keeps it: it entered `entry_ns` after the process's time
/// origin ([`stamp`]) and rendered `block` frames from `frame` at `rate`. A `rate` of 0 is
/// [`FrameClock::clear`]: no callback ran from there.
#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub entry_ns: u64,
    pub frame: Frame,
    pub block: u32,
    pub rate: u32,
}

#[cfg(debug_assertions)]
#[derive(Default)]
struct Slot {
    /// `2k + 1` while stamp `k` is written into it, `2k + 2` once it is whole.
    seq: AtomicU64,
    entry_ns: AtomicU64,
    frame: AtomicI64,
    block: AtomicU32,
    rate: AtomicU32,
}

#[cfg(debug_assertions)]
struct History {
    /// Stamps claimed so far (the next one's index).
    written: AtomicU64,
    slots: Box<[Slot]>,
}

#[cfg(debug_assertions)]
impl Default for History {
    fn default() -> Self {
        History { written: AtomicU64::new(0), slots: (0..HISTORY).map(|_| Slot::default()).collect() }
    }
}

/// Why a stamp could not be read.
#[cfg(debug_assertions)]
enum Miss {
    /// Its writer has not finished it: read it again later.
    NotYet,
    /// A later stamp took its slot.
    Overwritten,
}

#[cfg(debug_assertions)]
impl History {
    /// Wait-free: one atomic add claims the slot (`clear` on the owner may race a callback).
    fn push(&self, s: Stamp) {
        let k = self.written.fetch_add(1, Relaxed);
        let slot = &self.slots[(k % HISTORY as u64) as usize];
        slot.seq.store(2 * k + 1, Relaxed);
        std::sync::atomic::fence(Release);
        slot.entry_ns.store(s.entry_ns, Relaxed);
        slot.frame.store(s.frame, Relaxed);
        slot.block.store(s.block, Relaxed);
        slot.rate.store(s.rate, Relaxed);
        slot.seq.store(2 * k + 2, Release);
    }

    fn read(&self, k: u64) -> Result<Stamp, Miss> {
        let slot = &self.slots[(k % HISTORY as u64) as usize];
        let before = slot.seq.load(Acquire);
        if before < 2 * k + 2 {
            return Err(Miss::NotYet);
        }
        if before > 2 * k + 2 {
            return Err(Miss::Overwritten);
        }
        let s = Stamp { entry_ns: slot.entry_ns.load(Relaxed), frame: slot.frame.load(Relaxed), block: slot.block.load(Relaxed), rate: slot.rate.load(Relaxed) };
        std::sync::atomic::fence(Acquire);
        if slot.seq.load(Relaxed) == before { Ok(s) } else { Err(Miss::Overwritten) }
    }
}

/// DEV: the instant (nanoseconds after the time origin, as [`stamp`]) the engine rendered device frame
/// `frame`: the entry of the stamp whose block holds it, plus its offset in the block at the stamp's
/// rate. `None` when no block holds the frame (one the device skipped, or outside the stamps).
/// `stamps`: the running callbacks' stamps in the order they were published, the `clear` markers
/// (rate 0) left out; the frame counter only grows from one to the next.
#[cfg(debug_assertions)]
pub fn frame_instant(stamps: &[Stamp], frame: Frame) -> Option<u64> {
    let s = stamps[block_of(stamps, frame)?];
    Some(s.entry_ns + ((frame - s.frame) as f64 * 1e9 / s.rate as f64).round() as u64)
}

/// DEV: the index of the stamp whose block holds `frame` (`stamps` as [`frame_instant`]'s).
#[cfg(debug_assertions)]
pub fn block_of(stamps: &[Stamp], frame: Frame) -> Option<usize> {
    let k = stamps.partition_point(|s| s.frame <= frame).checked_sub(1)?;
    (frame < stamps[k].frame + stamps[k].block as Frame).then_some(k)
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
        let entry_ns = stamp(entry);
        let seq = c.seq.load(Relaxed);
        c.seq.store(seq.wrapping_add(1), Relaxed);
        std::sync::atomic::fence(Release);
        c.entry_ns.store(entry_ns, Relaxed);
        c.frame.store(frame, Relaxed);
        c.block.store(block, Relaxed);
        c.rate.store(rate, Relaxed);
        c.seq.store(seq.wrapping_add(2), Release);
        #[cfg(debug_assertions)]
        c.history.push(Stamp { entry_ns, frame, block, rate });
    }

    /// DEV: the stamps published so far (where a reader that wants only later ones starts).
    #[cfg(debug_assertions)]
    pub fn stamps_written(&self) -> u64 {
        self.0.history.written.load(Acquire)
    }

    /// DEV: append the stamps from `*next` on to `out`, oldest first, and move `next` past them; stops at
    /// one still being written. Returns how many were lost: overwritten before this read reached them.
    #[cfg(debug_assertions)]
    pub fn stamps_since(&self, next: &mut u64, out: &mut Vec<Stamp>) -> u64 {
        let history = &self.0.history;
        let written = history.written.load(Acquire);
        let mut lost = 0;
        if written.saturating_sub(*next) > HISTORY as u64 {
            lost = written - HISTORY as u64 - *next;
            *next = written - HISTORY as u64;
        }
        while *next < written {
            match history.read(*next) {
                Ok(s) => out.push(s),
                Err(Miss::NotYet) => break,
                Err(Miss::Overwritten) => lost += 1,
            }
            *next += 1;
        }
        lost
    }

    /// No callback runs (the owner, before it drops the streams): presses find no stamp.
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

    /// The last callback's stamp: device frame `frame` began rendering at `at` (Unix time, ms), at
    /// `rate` frames a second. `None` while no callback runs.
    pub fn anchor(&self) -> Option<(Frame, f64, u32)> {
        let (entry_ns, frame, _, rate) = self.read()?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?;
        let ago_ns = stamp(Instant::now()).saturating_sub(entry_ns);
        Some((frame, now.as_secs_f64() * 1e3 - ago_ns as f64 / 1e6, rate))
    }

    /// The frame a press that arrived at `at` lands on: the render position then, plus one block.
    /// `None` while no callback runs (native MIDI then drops the press: `super::midi`'s rules).
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

    /// The benchmark's frame-to-time conversion: through the recorded (entry, frame) pairs, never a
    /// wall clock; a block's frames land at its entry plus their offset at its rate.
    #[cfg(debug_assertions)]
    #[test]
    fn a_frame_converts_to_an_instant_through_the_stamp_of_the_block_that_rendered_it() {
        let clock = FrameClock::new();
        let mut next = clock.stamps_written();
        let t0 = Instant::now();
        // Three 128-frame blocks at 48 kHz, 2.667 ms apart, then a gap the device skipped (a WASAPI
        // dry jump: frames 512..640 never rendered), then the device stops.
        for (k, frame) in [1000, 1128, 1256, 1512].into_iter().enumerate() {
            clock.publish(t0 + Duration::from_micros(2667 * k as u64), frame, 128, 48_000);
        }
        clock.clear();
        let mut stamps = Vec::new();
        assert_eq!(clock.stamps_since(&mut next, &mut stamps), 0);
        assert_eq!(stamps.len(), 5);
        assert_eq!(stamps[4].rate, 0, "the clear is kept as a marker");
        stamps.retain(|s| s.rate != 0);
        let t0 = stamp(t0);
        assert_eq!(frame_instant(&stamps, 1000), Some(t0));
        // 48 frames into the second block: its entry plus 1 ms.
        assert_eq!(frame_instant(&stamps, 1176), Some(t0 + 2_667_000 + 1_000_000));
        assert_eq!(frame_instant(&stamps, 1383), Some(t0 + 2 * 2_667_000 + 2_645_833));
        assert_eq!(frame_instant(&stamps, 1400), None, "skipped frames have no instant");
        assert_eq!(frame_instant(&stamps, 1512), Some(t0 + 3 * 2_667_000));
        assert_eq!(frame_instant(&stamps, 999), None);
        assert_eq!(frame_instant(&stamps, 1640), None);
        assert_eq!(block_of(&stamps, 1300), Some(2));
        // Read again: nothing new.
        assert_eq!(clock.stamps_since(&mut next, &mut stamps), 0);
        assert_eq!(stamps.len(), 4);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn a_reader_that_falls_a_whole_history_behind_counts_what_it_lost() {
        let clock = FrameClock::new();
        let mut next = 0;
        let t0 = Instant::now();
        for k in 0..HISTORY as i64 + 10 {
            clock.publish(t0, 128 * k, 128, 48_000);
        }
        let mut stamps = Vec::new();
        assert_eq!(clock.stamps_since(&mut next, &mut stamps), 10);
        assert_eq!(stamps.len(), HISTORY);
        assert_eq!(stamps[0].frame, 1280, "the oldest kept is the eleventh");
        assert_eq!(next, HISTORY as u64 + 10);
    }
}
