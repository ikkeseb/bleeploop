//! OWNS: the pure grid arithmetic, in device frames: bar length, the whole-bar clamps, the commit and
//! stop plans that turn a raw take into a master loop, a later take's bar count (a multiply past the
//! master), the beat grid the click and the beat LED read, and the later-take tiling map. Ported from
//! `quantize.ts` and `looper/grid-math.ts`; the multiply is the engine's own (F14).
//!
//! Every time here is an integer device frame; the only fractional quantity is a pulse period, which a
//! [`Grid`] carries as an exact ratio so a pulse never drifts off the loop it belongs to.
//!
//! Every bar computation takes the [`Meter`] it counts in as an argument: the master's today (the
//! clock's, `clock::Clock::meter`), never a constant. A per-lane meter later passes the lane's here, but
//! that alone does not make polymeter: each bar is rounded to whole frames on its own
//! ([`frames_per_bar`]), so three 4/4 bars and four 3/4 bars at one tempo can differ by a few frames
//! and [`TakeFill::later`] would not tile one into the other. It needs bar lines from one shared
//! rational timeline, and a rule for the common loop length within the lane buffer: not built.

/// An absolute device frame (or a frame count). Signed so differences need no casts.
pub type Frame = i64;

pub const MIN_BPM: u32 = 40;
pub const MAX_BPM: u32 = 300;

/// A written time signature, one of [`Meter::OFFERED`] (no other can be made). Its **pulse** is the
/// denominator's note, a quarter in x/4 and an eighth in x/8: the click and the grids run on pulses. Its
/// **beat** is what BPM, TAP, the count-in numerals and the beat LED count ([`Meter::pulses_per_beat`]).
/// The tempo is the shared beat; the meter is pulses to a bar plus their grouping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Meter {
    num: u8,
    den: u8,
}

impl Meter {
    /// The default, and what a session with no meter (every one written before meters) is in.
    pub const FOUR_FOUR: Meter = Meter { num: 4, den: 4 };
    /// The meters a player can pick. Additive meters (5/8, 7/8) are not offered: their beats are
    /// uneven, which the uniform pulse grid cannot carry.
    pub const OFFERED: [Meter; 5] = [Meter::FOUR_FOUR, Meter { num: 3, den: 4 }, Meter { num: 6, den: 8 }, Meter { num: 12, den: 8 }, Meter { num: 5, den: 4 }];

    /// The offered meter `num/den`, or `None`.
    pub fn new(num: u8, den: u8) -> Option<Meter> {
        Meter::OFFERED.into_iter().find(|m| m.num == num && m.den == den)
    }

    pub fn num(self) -> u8 {
        self.num
    }

    pub fn den(self) -> u8 {
        self.den
    }

    pub fn pulses_per_bar(self) -> u64 {
        self.num as u64
    }

    /// Pulses per accent group: 3 in a compound meter (6/8, 12/8: an eighth-note numerator above 3 that
    /// 3 divides), else 1. The click sounds a group's first pulse as a beat, the others softer.
    pub fn accent_group(self) -> u64 {
        if self.den == 8 && self.num > 3 && self.num % 3 == 0 {
            3
        } else {
            1
        }
    }

    /// Pulses per counted beat: the unit of BPM and TAP. A compound meter counts the dotted quarter
    /// (6/8 has two beats, 12/8 four), so the foot taps where it goes, its tempos read as any song's and
    /// fit 40-300 BPM, and its quarter note fits the input echo's line. The one place this reading lives:
    /// a session file records the beats it was saved with (`engine_io::session`), so changing it later is
    /// a compatibility change for the compound sessions saved before, not an edit here alone.
    pub fn pulses_per_beat(self) -> u64 {
        self.accent_group()
    }

    /// What BPM counts in a bar: the count-in's numerals and the beat LED's dots.
    pub fn beats_per_bar(self) -> u64 {
        self.pulses_per_bar() / self.pulses_per_beat()
    }

    /// Quarter notes in a bar: the FX delay divisions are note values, so the FX follow the real quarter.
    pub fn quarters_per_bar(self) -> f64 {
        self.num as f64 * 4.0 / self.den as f64
    }
}

/// Clamp and round a requested tempo to the integer range the transport runs at.
pub fn clamp_bpm(bpm: f64) -> u32 {
    if !bpm.is_finite() {
        return MIN_BPM;
    }
    bpm.round().clamp(MIN_BPM as f64, MAX_BPM as f64) as u32
}

/// Frames per bar of `meter` at `bpm` counted beats a minute, rounded to an integer (quantize.ts
/// `framesPerBar` with the meter's beats, same float order).
pub fn frames_per_bar(bpm: f64, sample_rate: u32, meter: Meter) -> Frame {
    (60.0 / bpm * meter.beats_per_bar() as f64 * sample_rate as f64).round() as Frame
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

/// A pulse grid. Pulse `n` (for `n >= base_index`) sounds at
/// `base_frame + round((n - base_index) * num / den)`, rounded half up, so the pulse period `num / den`
/// is exact and pulse `base_index + k * den` lands on `base_frame + k * num` with no accumulated error.
/// `base_index` carries the bar position across a re-anchor: an accent is `n % pulses_per_bar == 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub base_frame: Frame,
    pub base_index: u64,
    pub num: u64,
    pub den: u64,
}

impl Grid {
    /// The tempo grid at `bpm` beats of `meter`: one pulse every `60 * sr / (bpm * pulses_per_beat)` frames.
    pub fn tempo(base_frame: Frame, base_index: u64, bpm: u32, sample_rate: u32, meter: Meter) -> Self {
        Grid { base_frame, base_index, num: 60 * sample_rate as u64, den: bpm as u64 * meter.pulses_per_beat() }
    }

    /// The loop grid of a committed master: `bars` bars of `meter`'s pulses span exactly `master` frames.
    pub fn master(anchor: Frame, master: Frame, bars: Frame, meter: Meter) -> Self {
        Grid { base_frame: anchor, base_index: 0, num: master as u64, den: meter.pulses_per_bar() * bars as u64 }
    }

    /// The frame pulse `n` sounds on. `n` below `base_index` is clamped to it.
    pub fn beat_frame(&self, n: u64) -> Frame {
        let k = n.saturating_sub(self.base_index) as u128;
        let offset = (2 * k * self.num as u128 + self.den as u128) / (2 * self.den as u128);
        self.base_frame + offset as Frame
    }

    /// The first pulse index whose frame is at or after `frame`.
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
pub fn plan_commit(raw: Frame, bpm: f64, sample_rate: u32, record_len: Frame, meter: Meter) -> CommitPlan {
    let fpb = frames_per_bar(bpm, sample_rate, meter);
    let bars = clamp_bars(raw / fpb, max_whole_bars(record_len, fpb));
    let master = bars * fpb;
    CommitPlan {
        fpb,
        bars,
        master,
        derived_bpm: (bars as f64 * meter.beats_per_bar() as f64 * 60.0 * sample_rate as f64) / master as f64,
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

/// A stop's grace, in quarters of a counted beat to the bar: a press within `fpb / grace_parts` before a
/// bar line keeps that bar. A quarter of the beat BPM counts, in every meter, so the grace never falls
/// under the IPC-plus-block jitter a UI gesture lands with.
fn grace_parts(meter: Meter) -> Frame {
    4 * meter.beats_per_bar() as Frame
}

/// Whole bars chosen by a stop pressed `elapsed` frames after the take's musical start, with a quarter
/// beat of grace for a slightly early press (grid-math `planFreeStop` / `planLaterStop`).
pub fn bars_at(elapsed: Frame, fpb: Frame, meter: Meter) -> Frame {
    if fpb <= 0 {
        return 0;
    }
    // floor((elapsed + fpb/parts) / fpb), exactly.
    let parts = grace_parts(meter);
    (parts * elapsed + fpb).div_euclid(parts * fpb)
}

/// The free first-take stop: whole bars from musical time, clamped to what fits. `None` when no bar
/// has completed yet: the take then ends at the press and pads to one bar.
pub fn plan_free_stop(elapsed: Frame, fpb: Frame, record_len: Frame, meter: Meter) -> Option<Frame> {
    let bars = bars_at(elapsed, fpb, meter);
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

/// The later-take stop, in frames from the take's musical start (its window start minus the
/// alignment). Inside the first loop pass: the completed bars, as [`later_take_bars`] counts them (at
/// least one: a press inside the first bar records on to its bar line; the quarter-beat grace
/// included). Past it: the NEAREST whole number of loops (ties go up), at most the whole loops within
/// `max_bars`, so a player who meant one loop and pressed late keeps one, and one who meant two and
/// pressed early does not lose the second. The take records on to that boundary when it lies after the
/// press. One rule for a free later take and a FIXED multiply window stopped early (E10); the caller
/// keeps it within the window.
pub fn plan_later_stop(press: Frame, window_start: Frame, align: Frame, fpb: Frame, master_bars: Frame, max_bars: Frame, meter: Meter) -> Frame {
    let elapsed = press - (window_start - align);
    let master_bars = master_bars.max(1);
    let master = master_bars * fpb;
    if elapsed < master {
        return later_take_bars(bars_at(elapsed, fpb, meter), master_bars, max_bars) * fpb;
    }
    let loops = (2 * elapsed + master).div_euclid(2 * master);
    loops.min((max_bars / master_bars).max(1)) * master
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
pub fn plan_retake_stop(press: Frame, pass_end: Frame, fpb: Frame, has_kept_pass: bool, meter: Meter) -> RetakeStop {
    if grace_parts(meter) * (pass_end - press) <= fpb {
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

    const M44: Meter = Meter::FOUR_FOUR;

    // Ports verify/guards/quantize.mjs (framesPerBar, maxWholeBars, clampBars; averageInterval stays in
    // the UI with tap tempo) and the pure halves of grid.mjs A, count-grid.mjs A/B and short-take.mjs.

    #[test]
    fn frames_per_bar_matches_the_ts_rounding() {
        assert_eq!(frames_per_bar(120.0, 48000, M44), 96000);
        assert_eq!(frames_per_bar(120.0, 44100, M44), 88200);
        assert_eq!(frames_per_bar(137.0, 44100, M44), 77255);
        assert_eq!(frames_per_bar(137.0, 48000, M44), 84088);
        // A bar is its counted beats at the tempo: a compound meter's at the dotted quarter.
        let meter = |n, d| Meter::new(n, d).unwrap();
        assert_eq!(frames_per_bar(120.0, 48000, meter(3, 4)), 72000);
        assert_eq!(frames_per_bar(60.0, 48000, meter(6, 8)), 96000);
        assert_eq!(frames_per_bar(50.0, 48000, meter(12, 8)), 230400);
        assert_eq!(frames_per_bar(100.0, 48000, meter(5, 4)), 144000);
    }

    #[test]
    fn the_offered_meters_and_how_each_counts() {
        let table: Vec<_> = Meter::OFFERED.iter().map(|m| (m.num(), m.den(), m.pulses_per_bar(), m.accent_group(), m.beats_per_bar(), m.quarters_per_bar())).collect();
        assert_eq!(table, [(4, 4, 4, 1, 4, 4.0), (3, 4, 3, 1, 3, 3.0), (6, 8, 6, 3, 2, 3.0), (12, 8, 12, 3, 4, 6.0), (5, 4, 5, 1, 5, 5.0)]);
        assert_eq!(Meter::OFFERED[0], Meter::FOUR_FOUR);
        for m in Meter::OFFERED {
            assert_eq!(Meter::new(m.num(), m.den()), Some(m));
            assert_eq!(m.pulses_per_bar() % m.pulses_per_beat(), 0, "{m:?}: whole beats");
        }
        for (n, d) in [(7, 8), (5, 8), (2, 4), (9, 8), (4, 8), (0, 4), (4, 0), (3, 8)] {
            assert_eq!(Meter::new(n, d), None, "{n}/{d} is not offered");
        }
    }

    #[test]
    fn whole_bar_clamps() {
        assert_eq!(max_whole_bars(96000 * 4, 96000), 4);
        assert_eq!(max_whole_bars(96000 * 4 + 1, 96000), 4);
        assert_eq!(max_whole_bars(100, 96000), 1);
        assert_eq!(clamp_bars(0, 8), 1);
        assert_eq!(clamp_bars(3, 8), 3);
        assert_eq!(clamp_bars(99, 8), 8);
        let fpb = frames_per_bar(120.0, 48000, M44);
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
        for meter in Meter::OFFERED {
            let pulses = meter.pulses_per_bar();
            for sr in [48000, 44100] {
                for bpm in [120.0, 90.0, 137.0, 100.0, 200.0, 73.5] {
                    for bars in [1, 2, 3, 4, 7, 8] {
                        let master = bars * frames_per_bar(bpm, sr, meter);
                        let grid = Grid::master(1000, master, bars, meter);
                        for k in 0..50u64 {
                            assert_eq!(grid.beat_frame(k * pulses * bars as u64), 1000 + k as Frame * master);
                        }
                        // Every pulse within half a frame of its exact time.
                        for n in 0..200u64 {
                            let exact = 1000.0 + n as f64 * master as f64 / (pulses * bars as u64) as f64;
                            assert!((grid.beat_frame(n) as f64 - exact).abs() <= 0.5);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_tempo_grid_bar_is_the_meters_counted_beats() {
        for meter in Meter::OFFERED {
            for (bpm, sr) in [(60, 48000), (97, 44100), (300, 48000), (40, 44100)] {
                let grid = Grid::tempo(0, 0, bpm, sr, meter);
                let exact = 60.0 * sr as f64 / bpm as f64 * meter.beats_per_bar() as f64;
                assert!((grid.beat_frame(meter.pulses_per_bar()) as f64 - exact).abs() <= 0.5, "{meter:?} at {bpm}");
                assert!((frames_per_bar(bpm as f64, sr, meter) as f64 - exact).abs() <= 0.5);
            }
        }
    }

    #[test]
    fn first_beat_at_or_after_inverts_beat_frame() {
        let grids = [
            Grid::tempo(0, 0, 137, 44100, M44),
            Grid::tempo(-5000, 7, 90, 48000, M44),
            Grid::master(123, 77255 * 3, 3, M44),
            Grid { base_frame: 10, base_index: 123457, num: 22050, den: 1 },
            // Beats on a half frame (77255 / 4 * 2 = 38627.5): round half up puts them one frame later.
            Grid::master(0, 77255, 1, M44),
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
        let g = Grid::tempo(0, 0, 120, 48000, M44);
        assert_eq!(g.beat_frame(123_457), 123_457 * 24000);
    }

    #[test]
    fn plan_commit_floors_to_completed_bars() {
        for meter in Meter::OFFERED {
            for sr in [48000, 44100] {
                for bpm in [120.0, 90.0, 137.0, 100.0, 73.0, 200.0] {
                    let fpb = frames_per_bar(bpm, sr, meter);
                    for bars in [1, 2, 3, 4, 7, 16] {
                        let plan = plan_commit(bars * fpb, bpm, sr, bars * fpb + 16, meter);
                        assert_eq!((plan.master, plan.bars), (bars * fpb, bars));
                        assert!((plan.derived_bpm - bpm).abs() < 0.05, "{meter:?}: {} at {bpm}", plan.derived_bpm);
                        assert_eq!(clamp_bpm(plan.derived_bpm), clamp_bpm(bpm));
                    }
                    // A hair past a bar line keeps the completed bars; below one bar pads up to one.
                    assert_eq!(plan_commit(3 * fpb + fpb / 3, bpm, sr, 60 * sr as Frame, meter).master, 3 * fpb);
                    assert_eq!(plan_commit(fpb / 3, bpm, sr, 60 * sr as Frame, meter).master, fpb);
                }
            }
        }
        // 42 bpm at 44.1 kHz fits 10.5 bars in 60 s: the clamp floors to 10.
        let fpb = frames_per_bar(42.0, 44100, M44);
        assert_eq!(plan_commit(32 * fpb, 42.0, 44100, 60 * 44100, M44).master, 10 * fpb);
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
        let fpb = frames_per_bar(120.0, 48000, M44);
        let plan = |bars: f64| plan_later_stop((bars * fpb as f64) as Frame + 480_000, 480_000, 0, fpb, 8, 30, M44) / fpb;
        assert_eq!(plan(0.4), 1);
        assert_eq!(plan(1.5), 1);
        assert_eq!(plan(2.0 - 1.0 / 32.0), 2);
        assert_eq!(plan(2.0 + 1e-6), 2);
        assert_eq!(plan(8.0), 8);
        assert_eq!(plan(8.0 - 1.0 / 32.0), 8, "the grace before the loop's end keeps the loop");
        // Past the first loop pass (a free take, a multiply window): the nearest whole number of loops, a
        // tie going up, at most the whole loops within the bound (30 bars: three 8-bar loops).
        assert_eq!(plan(11.0), 8);
        assert_eq!(plan(12.0 - 1e-3), 8);
        assert_eq!(plan(12.0), 16, "a tie goes up");
        assert_eq!(plan(16.0 - 1.0 / 32.0), 16);
        assert_eq!(plan(19.9), 16);
        assert_eq!(plan(23.9), 24);
        assert_eq!(plan(29.0), 24, "never past the bound");
        // Musical time starts `align` before the window: past the first pass the nearest rule counts from
        // there too.
        assert_eq!(plan_later_stop(480_000 + 12 * fpb - 1, 480_000 + 960, 960, fpb, 8, 30, M44), 8 * fpb);
        assert_eq!(plan_later_stop(480_000 + 12 * fpb, 480_000 + 960, 960, fpb, 8, 30, M44), 16 * fpb);
        // The alignment shifts the window, not the musical time: a press just outside the grace stays out.
        let outside = 2 * fpb - fpb / 16 - 240;
        assert_eq!(plan_later_stop(480_000 + outside, 480_000, 0, fpb, 8, 30, M44), fpb);
        assert_eq!(plan_later_stop(480_000 + outside, 480_000 + 960, 960, fpb, 8, 30, M44), fpb);
        // Musical time starts `align` before the window: a press on the grace edge keeps the bar.
        let edge = 480_000 + 2 * fpb - fpb / 16;
        assert_eq!(plan_later_stop(edge, 480_000 + 960, 960, fpb, 8, 30, M44), 2 * fpb);
        assert_eq!(plan_later_stop(edge - 1, 480_000 + 960, 960, fpb, 8, 30, M44), fpb);
        // Free stop: no completed bar yet is None; the grace edge is inclusive.
        assert_eq!(plan_free_stop(fpb / 2, fpb, 60 * 48000, M44), None);
        assert_eq!(plan_free_stop(fpb - fpb / 16, fpb, 60 * 48000, M44), Some(fpb));
        assert_eq!(plan_free_stop(fpb - fpb / 16 - 1, fpb, 60 * 48000, M44), None);
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
        for meter in Meter::OFFERED {
            for sr in [44100, 48000] {
                for bpm in [60.0, 97.0, 120.0, 174.0] {
                    let fpb = frames_per_bar(bpm, sr, meter);
                    // A quarter of a counted beat: 1/16 of a 4/4 bar, 1/8 of a 6/8 bar (two dotted beats).
                    let grace = fpb / (4 * meter.beats_per_bar() as Frame);
                    let end = 1_000_000;
                    assert_eq!(plan_retake_stop(end - fpb * 2, end, fpb, true, meter), RetakeStop::KeepLast);
                    assert_eq!(plan_retake_stop(end - fpb * 2, end, fpb, false, meter), RetakeStop::StopNow);
                    assert_eq!(plan_retake_stop(end - grace, end, fpb, false, meter), RetakeStop::FinishPass);
                    assert_eq!(plan_retake_stop(end - grace - 1, end, fpb, true, meter), RetakeStop::KeepLast);
                    assert_eq!(plan_retake_stop(end, end, fpb, true, meter), RetakeStop::FinishPass);
                    for bars in [1, 2, 4] {
                        let len = bars * fpb;
                        for early in [0, 1, grace - 1, grace, grace + 1, grace + 2, fpb] {
                            let free = plan_free_stop(len - early, fpb, 60 * sr as Frame, meter).is_some_and(|t| t >= len);
                            let retake = plan_retake_stop(len - early, len, fpb, true, meter) == RetakeStop::FinishPass;
                            assert_eq!(free, retake, "{meter:?} bars={bars} early={early}");
                            assert_eq!(free, early <= grace, "{meter:?} bars={bars} early={early}: the grace is a quarter beat");
                            let later = plan_later_stop(len - early, 0, 0, fpb, 8, 32, meter);
                            if bars > 1 {
                                assert_eq!(later >= len, early <= grace, "{meter:?}: a later take's stop has the same grace");
                            }
                        }
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
