//! OWNS: the pure grid arithmetic, in device frames: bar length, the whole-bar clamps, the commit and
//! stop plans that turn a raw take into a master loop, a later take's bar count (a multiply past the
//! master), the beat grid the click and the beat LED read, and the later-take tiling map. Ported from
//! `src/audio/quantize.ts` and `src/audio/looper/grid-math.ts`; the multiply is the engine's own (F14).
//!
//! Every time here is an integer device frame; the only fractional quantity is a beat period, which a
//! [`Grid`] carries as an exact ratio so a beat never drifts off the loop it belongs to.

/// An absolute device frame (or a frame count). Signed so differences need no casts.
pub type Frame = i64;

/// First-track count-in: one bar of forced-audible click before the take begins.
pub const COUNT_IN_BEATS: u64 = 4;
pub const BEATS_PER_BAR: u64 = 4;
pub const MIN_BPM: u32 = 40;
pub const MAX_BPM: u32 = 300;

/// Clamp and round a requested tempo to the integer range the transport runs at.
pub fn clamp_bpm(bpm: f64) -> u32 {
    if !bpm.is_finite() {
        return MIN_BPM;
    }
    bpm.round().clamp(MIN_BPM as f64, MAX_BPM as f64) as u32
}

/// Frames per 4/4 bar at `bpm`, rounded to an integer (quantize.ts `framesPerBar`, same float order).
pub fn frames_per_bar(bpm: f64, sample_rate: u32) -> Frame {
    (60.0 / bpm * BEATS_PER_BAR as f64 * sample_rate as f64).round() as Frame
}

/// Largest number of whole bars that fit `buffer` frames, never below 1.
pub fn max_whole_bars(buffer: Frame, fpb: Frame) -> Frame {
    (buffer / fpb).max(1)
}

/// Clamp a bar count to `[1, max_bars]`.
pub fn clamp_bars(bars: Frame, max_bars: Frame) -> Frame {
    bars.max(1).min(max_bars)
}

/// `a / b` rounded toward +infinity, for any sign of `a` and `b > 0`.
pub fn ceil_div(a: Frame, b: Frame) -> Frame {
    -((-a).div_euclid(b))
}

/// A beat grid. Beat `n` (for `n >= base_index`) sounds at
/// `base_frame + round((n - base_index) * num / den)`, rounded half up, so the beat period `num / den`
/// is exact and beat `base_index + k * den` lands on `base_frame + k * num` with no accumulated error.
/// `base_index` carries the bar position across a re-anchor: an accent is `n % 4 == 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub base_frame: Frame,
    pub base_index: u64,
    pub num: u64,
    pub den: u64,
}

impl Grid {
    /// The tempo grid at `bpm`: one beat every `60 * sr / bpm` frames.
    pub fn tempo(base_frame: Frame, base_index: u64, bpm: u32, sample_rate: u32) -> Self {
        Grid { base_frame, base_index, num: 60 * sample_rate as u64, den: bpm as u64 }
    }

    /// The loop grid of a committed master: `4 * bars` beats span exactly `master` frames.
    pub fn master(anchor: Frame, master: Frame, bars: Frame) -> Self {
        Grid { base_frame: anchor, base_index: 0, num: master as u64, den: BEATS_PER_BAR * bars as u64 }
    }

    /// The frame beat `n` sounds on. `n` below `base_index` is clamped to it.
    pub fn beat_frame(&self, n: u64) -> Frame {
        let k = n.saturating_sub(self.base_index) as u128;
        let offset = (2 * k * self.num as u128 + self.den as u128) / (2 * self.den as u128);
        self.base_frame + offset as Frame
    }

    /// The first beat index whose frame is at or after `frame`.
    pub fn first_beat_at_or_after(&self, frame: Frame) -> u64 {
        let d = frame - self.base_frame;
        if d <= 0 {
            return self.base_index;
        }
        // round(k*num/den) >= d  <=>  2k*num + den >= 2d*den  <=>  k >= (2d - 1)*den / (2*num)
        let top = (2 * d as u128 - 1) * self.den as u128;
        let bottom = 2 * self.num as u128;
        self.base_index + top.div_ceil(bottom) as u64
    }
}

/// The first-track commit (grid-math `planCommit`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CommitPlan {
    pub fpb: Frame,
    pub bars: Frame,
    pub master: Frame,
    /// The tempo the integer frame count implies; the transport shows it rounded.
    pub derived_bpm: f64,
}

/// FLOOR a raw take to its completed whole bars (at least one, at most what fits `record_len`): a free
/// stop lands a hair after the targeted downbeat, so the overshoot is dropped, never rounded up.
pub fn plan_commit(raw: Frame, bpm: f64, sample_rate: u32, record_len: Frame) -> CommitPlan {
    let fpb = frames_per_bar(bpm, sample_rate);
    let bars = clamp_bars(raw / fpb, max_whole_bars(record_len, fpb));
    let master = bars * fpb;
    CommitPlan {
        fpb,
        bars,
        master,
        derived_bpm: (bars as f64 * 4.0 * 60.0 * sample_rate as f64) / master as f64,
    }
}

/// The master grid anchor at a first-take commit: the counted downbeat, a whole number of loops back
/// from `at`, so the loop, its click and the count-in share one grid (grid-math `commitAnchor`).
pub fn commit_anchor(downbeat: Option<Frame>, master: Frame, at: Frame) -> Frame {
    match downbeat {
        Some(d) if master > 0 => d + (at - d).div_euclid(master) * master,
        _ => at,
    }
}

/// Whole bars chosen by a stop pressed `elapsed` frames after the take's musical start, with a quarter
/// beat of grace for a slightly early press (grid-math `planFreeStop` / `planLaterStop`).
pub fn bars_at(elapsed: Frame, fpb: Frame) -> Frame {
    if fpb <= 0 {
        return 0;
    }
    // floor((elapsed + fpb/16) / fpb), exactly.
    (16 * elapsed + fpb).div_euclid(16 * fpb)
}

/// The free first-take stop: whole bars from musical time, clamped to what fits. `None` when no bar
/// has completed yet: the take then ends at the press and pads to one bar.
pub fn plan_free_stop(elapsed: Frame, fpb: Frame, record_len: Frame) -> Option<Frame> {
    let bars = bars_at(elapsed, fpb);
    (bars >= 1).then(|| clamp_bars(bars, max_whole_bars(record_len, fpb)) * fpb)
}

/// The bars a later take records over a master of `master_bars` whole bars when `requested` are asked
/// for (the FIXED count, or the bars an early stop completed). Up to the master: the request, at least
/// one (a shorter take tiles across the loop). Past it, a multiply (F14): the loop grows to the take in
/// whole loops, so the request floors to a multiple of the master, at most the largest multiple within
/// `max_bars` (the lane buffer and the FIXED bound). A request below two loops therefore gives the master.
pub fn later_take_bars(requested: Frame, master_bars: Frame, max_bars: Frame) -> Frame {
    let master_bars = master_bars.max(1);
    if requested <= master_bars {
        return clamp_bars(requested, master_bars);
    }
    (requested / master_bars).min((max_bars / master_bars).max(1)) * master_bars
}

/// The later-take stop: completed bars since the take's musical start (its window start minus the
/// alignment), as [`later_take_bars`] counts them: up to the master as they are (at least one), past it
/// floored to whole loops (a multiply window stopped early).
pub fn plan_later_stop(press: Frame, window_start: Frame, align: Frame, fpb: Frame, master_bars: Frame, max_bars: Frame) -> Frame {
    let elapsed = press - (window_start - align);
    later_take_bars(bars_at(elapsed, fpb), master_bars, max_bars) * fpb
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetakeStop {
    /// Inside the quarter-beat grace before the pass edge: let the pass in flight finish and keep it.
    FinishPass,
    /// Keep the last complete pass; the one in flight is dropped.
    KeepLast,
    /// Nothing complete kept yet: an ordinary stop.
    StopNow,
}

/// The stop gesture on a rolling RETAKE (grid-math `planRetakeStop`). The grace equals the one
/// [`bars_at`] gives a free stop.
pub fn plan_retake_stop(press: Frame, pass_end: Frame, fpb: Frame, has_kept_pass: bool) -> RetakeStop {
    if 16 * (pass_end - press) <= fpb {
        RetakeStop::FinishPass
    } else if has_kept_pass {
        RetakeStop::KeepLast
    } else {
        RetakeStop::StopNow
    }
}

/// The master boundary at or after `now` (`now` itself with no master).
pub fn next_boundary(anchor: Frame, master: Frame, now: Frame) -> Frame {
    if master <= 0 {
        return now;
    }
    anchor + ceil_div(now - anchor, master) * master
}

/// Position of `frame` in a loop of `len` frames anchored at `anchor`, in `[0, len)`.
pub fn loop_pos(frame: Frame, anchor: Frame, len: Frame) -> Frame {
    (frame - anchor).rem_euclid(len)
}

/// How a committed take fills its master region: loop position `p` holds take frame `p % period`, or
/// silence where the take never reached (`p % period >= raw`). One map for both commits (grid-math
/// `commitLaterTake`): a first take pads its short tail (`period` = master); a later take floors to whole
/// bars and tiles them across the master, blanking whatever a window cut short of its first bar left.
/// A multiply extends every other loop with it ([`TakeFill::extend`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TakeFill {
    pub raw: Frame,
    pub period: Frame,
    pub master: Frame,
}

impl TakeFill {
    pub fn first(raw: Frame, master: Frame) -> Self {
        TakeFill { raw: raw.min(master), period: master, master }
    }

    pub fn later(raw: Frame, fpb: Frame, master: Frame) -> Self {
        if fpb <= 0 || master % fpb != 0 {
            // Not a whole-bar master (a foreign import): no tiling, only the padding.
            return TakeFill::first(raw, master);
        }
        let bars = clamp_bars(raw / fpb, max_whole_bars(master, fpb));
        TakeFill { raw: raw.min(master), period: bars * fpb, master }
    }

    /// A committed loop of `old` frames tiled out to a new master of `master` frames, a whole multiple of
    /// it (a multiply): the same loop, repeating. Being whole loops, it reads the same backwards.
    pub fn extend(old: Frame, master: Frame) -> Self {
        TakeFill { raw: old, period: old, master }
    }

    /// The take frame loop position `p` plays, or `None` for silence.
    pub fn source(&self, p: Frame) -> Option<Frame> {
        let q = p % self.period;
        (q < self.raw).then_some(q)
    }

    /// Positions below this already hold their final content; the fill rewrites `[untouched, master)`.
    pub fn untouched(&self) -> Frame {
        self.raw.min(self.period)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ports verify/guards/quantize.mjs (framesPerBar, maxWholeBars, clampBars; averageInterval stays in
    // the UI with tap tempo) and the pure halves of grid.mjs A, count-grid.mjs A/B and short-take.mjs.

    #[test]
    fn frames_per_bar_matches_the_ts_rounding() {
        assert_eq!(frames_per_bar(120.0, 48000), 96000);
        assert_eq!(frames_per_bar(120.0, 44100), 88200);
        assert_eq!(frames_per_bar(137.0, 44100), 77255);
        assert_eq!(frames_per_bar(137.0, 48000), 84088);
    }

    #[test]
    fn whole_bar_clamps() {
        assert_eq!(max_whole_bars(96000 * 4, 96000), 4);
        assert_eq!(max_whole_bars(96000 * 4 + 1, 96000), 4);
        assert_eq!(max_whole_bars(100, 96000), 1);
        assert_eq!(clamp_bars(0, 8), 1);
        assert_eq!(clamp_bars(3, 8), 3);
        assert_eq!(clamp_bars(99, 8), 8);
        let fpb = frames_per_bar(120.0, 48000);
        for req in [1, 7, 30, 31, 999] {
            let bars = clamp_bars(req, max_whole_bars(60 * 48000, fpb));
            assert!(bars >= 1 && bars * fpb <= 60 * 48000);
        }
    }

    #[test]
    fn bpm_clamps_and_rounds() {
        assert_eq!(clamp_bpm(120.3), 120);
        assert_eq!(clamp_bpm(10.0), MIN_BPM);
        assert_eq!(clamp_bpm(999.0), MAX_BPM);
        assert_eq!(clamp_bpm(f64::NAN), MIN_BPM);
    }

    #[test]
    fn master_grid_beats_span_the_loop_exactly() {
        for sr in [48000, 44100] {
            for bpm in [120.0, 90.0, 137.0, 100.0, 200.0, 73.5] {
                for bars in [1, 2, 3, 4, 7, 8] {
                    let master = bars * frames_per_bar(bpm, sr);
                    let grid = Grid::master(1000, master, bars);
                    for k in 0..50u64 {
                        assert_eq!(grid.beat_frame(k * 4 * bars as u64), 1000 + k as Frame * master);
                    }
                    // Every beat within half a frame of its exact time.
                    for n in 0..200u64 {
                        let exact = 1000.0 + n as f64 * master as f64 / (4 * bars) as f64;
                        assert!((grid.beat_frame(n) as f64 - exact).abs() <= 0.5);
                    }
                }
            }
        }
    }

    #[test]
    fn first_beat_at_or_after_inverts_beat_frame() {
        let grids = [
            Grid::tempo(0, 0, 137, 44100),
            Grid::tempo(-5000, 7, 90, 48000),
            Grid::master(123, 77255 * 3, 3),
            Grid { base_frame: 10, base_index: 123457, num: 22050, den: 1 },
            // Beats on a half frame (77255 / 4 * 2 = 38627.5): round half up puts them one frame later.
            Grid::master(0, 77255, 1),
            Grid { base_frame: 0, base_index: 0, num: 3, den: 2 },
        ];
        for g in grids {
            let probes = (g.base_frame - 3..g.base_frame + 200_000).step_by(997).chain((0..40).map(|n| g.beat_frame(n)));
            for f in probes {
                let n = g.first_beat_at_or_after(f);
                assert!(g.beat_frame(n) >= f);
                assert!(n == g.base_index || g.beat_frame(n - 1) < f);
            }
        }
    }

    #[test]
    fn a_large_beat_index_stays_exact() {
        let g = Grid::tempo(0, 0, 120, 48000);
        assert_eq!(g.beat_frame(123_457), 123_457 * 24000);
    }

    #[test]
    fn plan_commit_floors_to_completed_bars() {
        for sr in [48000, 44100] {
            for bpm in [120.0, 90.0, 137.0, 100.0, 73.0, 200.0] {
                let fpb = frames_per_bar(bpm, sr);
                for bars in [1, 2, 3, 4, 7, 16] {
                    let plan = plan_commit(bars * fpb, bpm, sr, bars * fpb + 16);
                    assert_eq!((plan.master, plan.bars), (bars * fpb, bars));
                    assert!((plan.derived_bpm - bpm).abs() < 0.05);
                    assert_eq!(clamp_bpm(plan.derived_bpm), clamp_bpm(bpm));
                }
                // A hair past a bar line keeps the completed bars; below one bar pads up to one.
                assert_eq!(plan_commit(3 * fpb + fpb / 3, bpm, sr, 60 * sr as Frame).master, 3 * fpb);
                assert_eq!(plan_commit(fpb / 3, bpm, sr, 60 * sr as Frame).master, fpb);
            }
        }
        // 42 bpm at 44.1 kHz fits 10.5 bars in 60 s: the clamp floors to 10.
        let fpb = frames_per_bar(42.0, 44100);
        assert_eq!(plan_commit(32 * fpb, 42.0, 44100, 60 * 44100).master, 10 * fpb);
    }

    #[test]
    fn commit_anchor_lands_on_the_count_grid() {
        for master in [96000, 77255, 3 * 84088] {
            let downbeat = 1_234_567;
            for at in [downbeat, downbeat + 1, downbeat + master - 1, downbeat + master, downbeat + 5 * master + 17] {
                let anchor = commit_anchor(Some(downbeat), master, at);
                assert_eq!((anchor - downbeat) % master, 0);
                assert!(anchor <= at && at < anchor + master);
            }
        }
        assert_eq!(commit_anchor(None, 96000, 500), 500);
        assert_eq!(commit_anchor(Some(100), 0, 500), 500, "no master: anchor at the commit");
    }

    #[test]
    fn stop_plans_use_a_quarter_beat_grace() {
        let fpb = frames_per_bar(120.0, 48000);
        let plan = |bars: f64| plan_later_stop((bars * fpb as f64) as Frame + 480_000, 480_000, 0, fpb, 8, 30) / fpb;
        assert_eq!(plan(0.4), 1);
        assert_eq!(plan(1.5), 1);
        assert_eq!(plan(2.0 - 1.0 / 32.0), 2);
        assert_eq!(plan(2.0 + 1e-6), 2);
        assert_eq!(plan(8.0), 8);
        assert_eq!(plan(11.0), 8);
        // Past the master (a multiply window) the completed bars floor to whole loops, grace included.
        assert_eq!(plan(16.0 - 1.0 / 32.0), 16);
        assert_eq!(plan(23.9), 16);
        // The alignment shifts the window, not the musical time: a press just outside the grace stays out.
        let outside = 2 * fpb - fpb / 16 - 240;
        assert_eq!(plan_later_stop(480_000 + outside, 480_000, 0, fpb, 8, 30), fpb);
        assert_eq!(plan_later_stop(480_000 + outside, 480_000 + 960, 960, fpb, 8, 30), fpb);
        // Musical time starts `align` before the window: a press on the grace edge keeps the bar.
        let edge = 480_000 + 2 * fpb - fpb / 16;
        assert_eq!(plan_later_stop(edge, 480_000 + 960, 960, fpb, 8, 30), 2 * fpb);
        assert_eq!(plan_later_stop(edge - 1, 480_000 + 960, 960, fpb, 8, 30), fpb);
        // Free stop: no completed bar yet is None; the grace edge is inclusive.
        assert_eq!(plan_free_stop(fpb / 2, fpb, 60 * 48000), None);
        assert_eq!(plan_free_stop(fpb - fpb / 16, fpb, 60 * 48000), Some(fpb));
        assert_eq!(plan_free_stop(fpb - fpb / 16 - 1, fpb, 60 * 48000), None);
    }

    #[test]
    fn a_later_take_is_its_bars_up_to_the_master_and_whole_loops_past_it() {
        // Up to the master (and a request below 1): as today, at least one bar.
        for (req, m, want) in [(0, 2, 1), (1, 2, 1), (2, 2, 2), (3, 4, 3), (4, 4, 4), (32, 75, 32)] {
            assert_eq!(later_take_bars(req, m, 30), want, "req={req} m={m}");
        }
        // Past it: floored to whole loops, so below two loops is the master.
        for (req, m, want) in [(3, 2, 2), (4, 2, 4), (5, 2, 4), (8, 2, 8), (4, 1, 4), (7, 3, 6), (8, 3, 6), (9, 3, 9), (5, 4, 4)] {
            assert_eq!(later_take_bars(req, m, 30), want, "req={req} m={m}");
        }
        // Capped at the largest multiple within the bound, never below the master.
        for (req, m, max, want) in [(32, 1, 30, 30), (32, 2, 30, 30), (32, 3, 30, 30), (32, 4, 30, 28), (32, 7, 30, 28), (32, 3, 10, 9), (32, 4, 10, 8), (32, 6, 10, 6), (12, 6, 6, 6)] {
            assert_eq!(later_take_bars(req, m, max), want, "req={req} m={m} max={max}");
        }
        // Always a whole number of loops once past the master, and never past the bound.
        for m in 1..=12 {
            for max in m..=32 {
                for req in m + 1..=32 {
                    let bars = later_take_bars(req, m, max);
                    assert!(bars % m == 0 && bars >= m && bars <= req.max(m) && bars <= max, "req={req} m={m} max={max} -> {bars}");
                }
            }
        }
    }

    #[test]
    fn the_retake_grace_equals_the_free_stop_grace() {
        for sr in [44100, 48000] {
            for bpm in [60.0, 97.0, 120.0, 174.0] {
                let fpb = frames_per_bar(bpm, sr);
                let grace = fpb / 16;
                let end = 1_000_000;
                assert_eq!(plan_retake_stop(end - fpb * 2, end, fpb, true), RetakeStop::KeepLast);
                assert_eq!(plan_retake_stop(end - fpb * 2, end, fpb, false), RetakeStop::StopNow);
                assert_eq!(plan_retake_stop(end - grace, end, fpb, false), RetakeStop::FinishPass);
                assert_eq!(plan_retake_stop(end - grace - 1, end, fpb, true), RetakeStop::KeepLast);
                assert_eq!(plan_retake_stop(end, end, fpb, true), RetakeStop::FinishPass);
                for bars in [1, 2, 4] {
                    let len = bars * fpb;
                    for early in [0, 1, grace - 1, grace + 2, fpb] {
                        let free = plan_free_stop(len - early, fpb, 60 * sr as Frame).is_some_and(|t| t >= len);
                        let retake = plan_retake_stop(len - early, len, fpb, true) == RetakeStop::FinishPass;
                        assert_eq!(free, retake, "bars={bars} early={early}");
                    }
                }
            }
        }
    }

    #[test]
    fn next_boundary_and_loop_pos() {
        assert_eq!(next_boundary(100, 50, 100), 100);
        assert_eq!(next_boundary(100, 50, 101), 150);
        assert_eq!(next_boundary(100, 50, 60), 100);
        assert_eq!(next_boundary(100, 0, 77), 77);
        assert_eq!(loop_pos(99, 100, 50), 49);
        assert_eq!(loop_pos(150, 100, 50), 0);
    }

    fn fill(buf: &mut [f32], f: TakeFill) {
        let src = buf.to_vec();
        for p in f.untouched()..f.master {
            buf[p as usize] = f.source(p).map_or(0.0, |q| src[q as usize]);
        }
    }

    #[test]
    fn later_takes_tile_whole_bars_and_blank_a_short_window() {
        // 1 bar (3 frames) over a master of 8 bars.
        let mut buf = vec![99.0; 26];
        buf[..3].copy_from_slice(&[1.0, 2.0, 3.0]);
        fill(&mut buf, TakeFill::later(3, 3, 24));
        assert!((0..24).all(|i| buf[i] == [1.0, 2.0, 3.0][i % 3]));
        assert_eq!(&buf[24..], &[99.0, 99.0]);
        // 3 bars over 8: the last copy is cut at the master edge.
        let mut buf: Vec<f32> = (1..=9).map(|v| v as f32).chain([77.0; 17]).collect();
        fill(&mut buf, TakeFill::later(9, 3, 24));
        assert!((0..24).all(|i| buf[i] == ((i % 9) + 1) as f32));
        assert_eq!(buf[24], 77.0);
        // A take as long as the master is untouched.
        let before = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 88.0, 89.0];
        let mut buf = before.to_vec();
        fill(&mut buf, TakeFill::later(6, 3, 6));
        assert_eq!(buf, before);
        // 2.5 captured bars floor to two and tile.
        let mut buf: Vec<f32> = (1..=10).map(|v| v as f32).chain([9.0; 6]).collect();
        fill(&mut buf, TakeFill::later(10, 4, 16));
        assert_eq!(buf, [1., 2., 3., 4., 5., 6., 7., 8., 1., 2., 3., 4., 5., 6., 7., 8.]);
        // A window cut short of its first bar line blanks its stale tail before tiling.
        let mut buf = vec![9.0; 16];
        buf[..2].copy_from_slice(&[1.0, 2.0]);
        fill(&mut buf, TakeFill::later(2, 4, 16));
        assert_eq!(buf, [1., 2., 0., 0., 1., 2., 0., 0., 1., 2., 0., 0., 1., 2., 0., 0.]);
        // A first take pads its short tail.
        let mut buf = vec![5.0; 8];
        fill(&mut buf, TakeFill::first(3, 8));
        assert_eq!(buf, [5., 5., 5., 0., 0., 0., 0., 0.]);
        // A master that is no whole number of bars (a foreign import) is padded, never tiled.
        assert_eq!(TakeFill::later(5, 4, 10), TakeFill::first(5, 10));
        assert_eq!(TakeFill::later(5, 0, 10), TakeFill::first(5, 10));
        // A multiply extends a committed loop to whole copies of it; played backwards it is the loop
        // backwards, repeating.
        let mut buf: Vec<f32> = (1..=4).map(|v| v as f32).chain([7.0; 9]).collect();
        let f = TakeFill::extend(4, 12);
        assert_eq!(f.untouched(), 4);
        fill(&mut buf, f);
        assert_eq!(buf, [1., 2., 3., 4., 1., 2., 3., 4., 1., 2., 3., 4., 7.]);
        let back: Vec<f32> = buf[..12].iter().rev().copied().collect();
        assert!((0..12).all(|k| back[k] == [4., 3., 2., 1.][k % 4]));
    }
}
