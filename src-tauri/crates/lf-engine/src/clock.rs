//! OWNS: tempo (the BPM, the meter and their lock) and the pulse: ONE grid that serves the free-run,
//! count-in and master anchors, the metronome click it triggers (accent, beat and the softer pulses
//! between a compound meter's beats), the click's anti-flam guard and the bar line a FADE ends on. Ported
//! from `clock.ts`; tap tempo stays in the UI and arrives as a tempo command.
//!
//! No lookahead scheduler: a beat fires on the exact frame its grid puts it, inside `process`. Only a
//! jump in the device frame counter (a callback the device never delivered) can leave beats behind:
//! those are dropped, except count-in beats, which fire late on the first frame after the jump so the
//! count stays complete (the Web Audio pulse's stalled-waker rule).

use crate::grid::{clamp_bpm, Frame, Grid, Meter};

/// Minimum spacing between two clicks on a beat of one pulse: only a true coincidence of two grids at a
/// re-anchor is closer (beats are at least 0.2 s apart at 300 bpm). A beat of `p` pulses divides it by
/// `p`: a dotted 300 in 6/8 puts its eighths 67 ms apart.
const MIN_CLICK_SPACING_CENTIS: i64 = 12;

/// Default click level (0..1), as the UI persists it.
pub const DEFAULT_CLICK_VOLUME: f32 = 0.7;

#[derive(Clone, Copy, Debug)]
struct Pulse {
    grid: Grid,
    /// The next beat index to fire.
    next: u64,
    /// Pulses below this index are the count-in's: they click even with the metronome off.
    forced_until: u64,
    /// Only a free-running pulse follows a tempo change.
    free_run: bool,
}

/// One counted beat the pulse fired (the pulses between a compound meter's beats click without one):
/// what the beat LED shows, and whether it clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Beat {
    pub frame: Frame,
    /// The counted beat's position in the bar, 0 = the accented downbeat.
    pub beat_in_bar: u8,
    /// Count-in beats still to come including this one (4, 3, 2, 1 in 4/4; 2, 1 in 6/8), 0 outside a
    /// count.
    pub count_left: u8,
    pub clicked: bool,
}

/// How loud a pulse clicks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickLevel {
    /// The bar's first pulse.
    Accent,
    /// The first pulse of an accent group (every pulse of an x/4 meter but the first).
    Beat,
    /// A compound meter's other pulses.
    Sub,
}

impl ClickLevel {
    /// What pulse `pulse_in_bar` of `meter` clicks as.
    pub fn of(meter: Meter, pulse_in_bar: u64) -> ClickLevel {
        if pulse_in_bar == 0 {
            ClickLevel::Accent
        } else if pulse_in_bar % meter.accent_group() == 0 {
            ClickLevel::Beat
        } else {
            ClickLevel::Sub
        }
    }

    /// The blip's peak at full click volume. Accent and beat are twice the old 0.5 / 0.28 (by ear,
    /// 2026-09-24); a sub-pulse's level is not heard yet. The master limiter catches the sum.
    fn peak(self) -> f64 {
        match self {
            ClickLevel::Accent => 1.0,
            ClickLevel::Beat => 0.56,
            ClickLevel::Sub => 0.32,
        }
    }
}

pub struct Clock {
    sample_rate: u32,
    bpm: u32,
    meter: Meter,
    locked: bool,
    metronome: bool,
    click_volume: f32,
    pulse: Option<Pulse>,
    last_click: Option<Frame>,
    /// The last click was a late count beat fired after a jump: it may not silence a beat on time.
    last_click_late: bool,
    click: ClickVoice,
}

impl Clock {
    pub fn new(sample_rate: u32) -> Self {
        Clock {
            sample_rate,
            bpm: 120,
            meter: Meter::FOUR_FOUR,
            locked: false,
            metronome: false,
            click_volume: DEFAULT_CLICK_VOLUME,
            pulse: None,
            last_click: None,
            last_click_late: false,
            click: ClickVoice::default(),
        }
    }

    pub fn bpm(&self) -> u32 {
        self.bpm
    }

    /// The meter every bar counts in: the master's, locked with the tempo.
    pub fn meter(&self) -> Meter {
        self.meter
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    pub fn metronome(&self) -> bool {
        self.metronome
    }

    /// The frame the pulse fires next on; `None` before the transport runs.
    pub fn next_beat_frame(&self) -> Option<Frame> {
        self.pulse.map(|p| p.grid.beat_frame(p.next))
    }

    /// The downbeat `bars` bars after the first one at or after `frame`, on the grid the click plays
    /// (where a FADE ends); `None` before the transport runs.
    pub fn downbeat_after(&self, frame: Frame, bars: u64) -> Option<Frame> {
        let grid = self.pulse?.grid;
        let bar = self.meter.pulses_per_bar();
        let first = grid.first_beat_at_or_after(frame).next_multiple_of(bar);
        Some(grid.beat_frame(first + bars * bar))
    }

    /// Start the free-running pulse at `now` if nothing runs yet (the engine's first frame).
    pub fn ensure_running(&mut self, now: Frame) {
        if self.pulse.is_none() {
            self.start_free_run(now);
        }
    }

    /// Set the tempo, clamped to [40, 300] and rounded. A no-op while locked or when unchanged. A live
    /// free-run pulse bends to the new period from its next beat; count and master grids keep theirs.
    pub fn set_bpm(&mut self, bpm: f64, now: Frame) {
        if self.locked {
            return;
        }
        let bpm = clamp_bpm(bpm);
        if bpm == self.bpm {
            return;
        }
        self.bpm = bpm;
        if self.pulse.is_some_and(|p| p.free_run) {
            self.start_free_run(now);
        }
    }

    /// Set the meter. A no-op while locked (it locks with the tempo) or when unchanged. A free-running
    /// pulse keeps its next pulse's frame, which becomes a downbeat: carrying the old bar position would
    /// put the accent mid-bar.
    pub fn set_meter(&mut self, meter: Meter) {
        if self.locked || meter == self.meter {
            return;
        }
        self.meter = meter;
        if let Some(p) = self.pulse.filter(|p| p.free_run) {
            let grid = Grid::tempo(p.grid.beat_frame(p.next), 0, self.bpm, self.sample_rate, meter);
            self.pulse = Some(Pulse { grid, next: 0, forced_until: 0, free_run: true });
        }
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    pub fn set_metronome(&mut self, on: bool) {
        self.metronome = on;
    }

    pub fn set_click_volume(&mut self, volume: f32) {
        self.click_volume = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 0.0 };
    }

    /// Free-run at the current tempo. A running grid is CARRIED: its next beat keeps its frame and bar
    /// index, later beats follow the new period, so the LED never hops (count abort, master reset, a
    /// tempo change mid-free-run).
    pub fn start_free_run(&mut self, now: Frame) {
        let (base_frame, base_index) = match self.pulse {
            Some(p) => (p.grid.beat_frame(p.next), p.next),
            None => (now, 0),
        };
        self.pulse = Some(Pulse {
            grid: Grid::tempo(base_frame, base_index, self.bpm, self.sample_rate, self.meter),
            next: base_index,
            forced_until: 0,
            free_run: true,
        });
    }

    /// A count-in: one bar of the meter's pulses from `anchor`, forced, at the current tempo. Returns
    /// the downbeat after it, where the take's musical time starts. Clears the anti-flam reference so a
    /// fast abort and re-record can never swallow the new count "1".
    pub fn start_count_in(&mut self, anchor: Frame, now: Frame) -> Frame {
        self.last_click = None;
        let grid = Grid::tempo(anchor, 0, self.bpm, self.sample_rate, self.meter);
        let bar = self.meter.pulses_per_bar();
        self.pulse = Some(Pulse { grid, next: grid.first_beat_at_or_after(now), forced_until: bar, free_run: false });
        grid.beat_frame(bar)
    }

    /// Lock the pulse to a committed master: `bars` bars of the meter's pulses per `master` frames from
    /// `anchor`, which may lie in the past (the counted downbeat); the next pulse is the first at or after
    /// `now`. Keeps the anti-flam reference, so a count beat that just sounded cannot double with the
    /// master's.
    pub fn start_master(&mut self, anchor: Frame, master: Frame, bars: Frame, now: Frame) {
        let grid = Grid::master(anchor, master, bars, self.meter);
        self.pulse = Some(Pulse { grid, next: grid.first_beat_at_or_after(now), forced_until: 0, free_run: false });
    }

    /// AUTO REC: the current tempo from the detected onset, with no count beat to carry.
    pub fn start_auto_record(&mut self, anchor: Frame, now: Frame) {
        self.last_click = None;
        let grid = Grid::tempo(anchor, 0, self.bpm, self.sample_rate, self.meter);
        self.pulse = Some(Pulse { grid, next: grid.first_beat_at_or_after(now), forced_until: 0, free_run: false });
    }

    /// Master reset: carry the loop grid into free-run.
    pub fn stop_master(&mut self, now: Frame) {
        self.start_free_run(now);
    }

    /// True while a count-in still has forced pulses to fire.
    pub fn counting(&self) -> bool {
        self.pulse.is_some_and(|p| p.next < p.forced_until)
    }

    /// Fire the pulses due at or before `frame` up to the next counted beat, and return that beat, or
    /// `None` when the next pulse lies ahead. A pulse between beats clicks with no beat to return. A
    /// pulse the frame counter jumped over is dropped, unless it is a count pulse: that one fires late,
    /// on `frame`. The click sounds on a count pulse, or with the metronome on while the transport is
    /// active (a transport active `until` a loop-end stop clicks strictly before it).
    pub fn fire_due(&mut self, frame: Frame, transport_until: Option<Frame>) -> Option<Beat> {
        self.fire(frame, transport_until, false)
    }

    /// [`Clock::fire_due`] for the count-in's pulses alone: what a count still owes when its downbeat
    /// arrives late, before the master's grid takes over there.
    pub fn fire_count(&mut self, frame: Frame, transport_until: Option<Frame>) -> Option<Beat> {
        self.fire(frame, transport_until, true)
    }

    fn fire(&mut self, frame: Frame, transport_until: Option<Frame>, count_only: bool) -> Option<Beat> {
        let meter = self.meter;
        loop {
            let pulse = self.pulse.as_mut()?;
            let at = pulse.grid.beat_frame(pulse.next);
            if at > frame || (count_only && pulse.next >= pulse.forced_until) {
                return None;
            }
            let n = pulse.next;
            pulse.next += 1;
            let forced = n < pulse.forced_until;
            if at < frame && !forced {
                continue;
            }
            let per_beat = meter.pulses_per_beat();
            let count_left = if forced { (pulse.forced_until - n).div_ceil(per_beat) as u8 } else { 0 };
            let in_bar = n % meter.pulses_per_bar();
            let gate = forced || (self.metronome && transport_until.is_some_and(|until| frame < until));
            let clicked = gate && self.trigger_click(frame, ClickLevel::of(meter, in_bar), at < frame);
            if in_bar % per_beat == 0 {
                return Some(Beat { frame, beat_in_bar: (in_bar / per_beat) as u8, count_left, clicked });
            }
        }
    }

    /// Anti-flam: a click closer than 0.12 s (a beat of one pulse; less in a compound beat) to the
    /// previous one is dropped, except a pulse on time after a late count pulse (the come-in "1" must
    /// sound).
    fn trigger_click(&mut self, frame: Frame, level: ClickLevel, late: bool) -> bool {
        if self.click_volume <= 0.0 {
            return false;
        }
        let sr = self.sample_rate as i64;
        let per_beat = self.meter.pulses_per_beat() as i64;
        let near = self.last_click.is_some_and(|last| (frame - last).abs() * 100 * per_beat < MIN_CLICK_SPACING_CENTIS * sr);
        let on_time_after_late = self.last_click_late && !late;
        if near && !on_time_after_late {
            return false;
        }
        self.last_click = Some(frame);
        self.last_click_late = late;
        self.click.start(frame, level, self.click_volume);
        true
    }

    /// Add the click for frames `[frame, frame + out.len())` into `out`.
    pub fn render_click(&mut self, frame: Frame, out: &mut [f32]) {
        self.click.render(frame, self.sample_rate, out);
    }
}

/// The metronome blip: a band-limited triangle at 1 kHz (1.5 kHz accented), a 2 ms linear attack to
/// its peak ([`ClickLevel`]), an exponential decay to 0.0001 at 60 ms, silence from 70 ms (clock.ts
/// `triggerClick`).
#[derive(Clone, Copy, Debug, Default)]
struct ClickVoice {
    start: Option<Frame>,
    accent: bool,
    peak: f64,
}

const CLICK_ATTACK: f64 = 0.002;
const CLICK_DECAY_END: f64 = 0.06;
const CLICK_STOP: f64 = 0.07;
const CLICK_FLOOR: f64 = 0.0001;

impl ClickVoice {
    fn start(&mut self, frame: Frame, level: ClickLevel, volume: f32) {
        self.start = Some(frame);
        self.accent = level == ClickLevel::Accent;
        self.peak = level.peak() * volume as f64;
    }

    fn render(&mut self, frame: Frame, sample_rate: u32, out: &mut [f32]) {
        let Some(start) = self.start else { return };
        let sr = sample_rate as f64;
        let stop = (CLICK_STOP * sr).round() as Frame;
        let freq = if self.accent { 1500.0 } else { 1000.0 };
        for (k, sample) in out.iter_mut().enumerate() {
            let age = frame + k as Frame - start;
            if age < 0 {
                continue;
            }
            if age >= stop {
                self.start = None;
                return;
            }
            let t = age as f64 / sr;
            let gain = if t < CLICK_ATTACK {
                self.peak * t / CLICK_ATTACK
            } else if t < CLICK_DECAY_END {
                self.peak * (CLICK_FLOOR / self.peak).powf((t - CLICK_ATTACK) / (CLICK_DECAY_END - CLICK_ATTACK))
            } else {
                CLICK_FLOOR
            };
            *sample += (gain * triangle(freq, t, sr)) as f32;
        }
    }
}

/// The triangle's Fourier series up to Nyquist: what a Web Audio 'triangle' oscillator plays.
fn triangle(freq: f64, t: f64, sample_rate: f64) -> f64 {
    let mut sum = 0.0;
    let mut k = 1u32;
    let mut sign = 1.0;
    while k as f64 * freq < sample_rate / 2.0 {
        let kf = k as f64;
        sum += sign * (std::f64::consts::TAU * kf * freq * t).sin() / (kf * kf);
        sign = -sign;
        k += 2;
    }
    sum * 8.0 / (std::f64::consts::PI * std::f64::consts::PI)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48000;

    fn meter(num: u8, den: u8) -> Meter {
        Meter::new(num, den).unwrap()
    }

    /// Every pulse due before `until`: its frame, and the click it started there (its peak at full click
    /// volume), or `None` when it stayed silent; and the counted beats returned.
    fn pulses(clock: &mut Clock, until: Frame, transport: Option<Frame>) -> (Vec<(Frame, Option<f64>)>, Vec<Beat>) {
        let (mut pulses, mut beats) = (Vec::new(), Vec::new());
        while let Some(at) = clock.next_beat_frame().filter(|&at| at < until) {
            let before = clock.last_click;
            beats.extend(clock.fire_due(at, transport));
            let clicked = clock.last_click == Some(at) && before != Some(at);
            pulses.push((at, clicked.then_some(clock.click.peak)));
        }
        (pulses, beats)
    }

    #[test]
    fn each_meter_accents_its_bar_and_softens_only_a_compound_meters_inner_pulses() {
        use ClickLevel::{Accent as A, Beat as B, Sub as S};
        let bar = |m: Meter| (0..m.pulses_per_bar()).map(|p| ClickLevel::of(m, p)).collect::<Vec<_>>();
        assert_eq!(bar(Meter::FOUR_FOUR), [A, B, B, B]);
        assert_eq!(bar(meter(3, 4)), [A, B, B]);
        assert_eq!(bar(meter(5, 4)), [A, B, B, B, B]);
        assert_eq!(bar(meter(6, 8)), [A, S, S, B, S, S]);
        assert_eq!(bar(meter(12, 8)), [A, S, S, B, S, S, B, S, S, B, S, S]);
        assert!([1.0, 0.56, 0.32].windows(2).all(|w| w[0] > w[1]));
        assert_eq!((A.peak(), B.peak(), S.peak()), (1.0, 0.56, 0.32), "accent and beat as 4/4 always had them");
    }

    #[test]
    fn a_count_in_is_one_bar_of_pulses_counted_down_in_beats() {
        for (m, counts) in [(Meter::FOUR_FOUR, vec![4, 3, 2, 1]), (meter(3, 4), vec![3, 2, 1]), (meter(6, 8), vec![2, 1]), (meter(12, 8), vec![4, 3, 2, 1]), (meter(5, 4), vec![5, 4, 3, 2, 1])] {
            let mut clock = Clock::new(SR);
            clock.set_meter(m);
            clock.set_bpm(100.0, 0);
            let downbeat = clock.start_count_in(0, 0);
            let (fired, beats) = pulses(&mut clock, downbeat, None);
            assert_eq!(fired.len() as u64, m.pulses_per_bar(), "{m:?}: one bar of pulses");
            assert!(fired.iter().all(|p| p.1.is_some()), "{m:?}: every count pulse clicks, the metronome off");
            assert_eq!(beats.iter().map(|b| b.count_left).collect::<Vec<_>>(), counts, "{m:?}");
            assert_eq!(beats.iter().map(|b| b.beat_in_bar as u64).collect::<Vec<_>>(), (0..m.beats_per_bar()).collect::<Vec<_>>());
            let grid = Grid::tempo(0, 0, 100, SR, m);
            assert_eq!(downbeat, grid.beat_frame(m.pulses_per_bar()));
            assert_eq!(beats.iter().map(|b| b.frame).collect::<Vec<_>>(), (0..m.beats_per_bar()).map(|k| grid.beat_frame(k * m.pulses_per_beat())).collect::<Vec<_>>());
            assert!(!clock.counting());
        }
    }

    #[test]
    fn a_dotted_300_in_six_eight_keeps_every_eighth_and_four_four_keeps_its_spacing() {
        // Eighths 67 ms apart: a fixed 0.12 s anti-flam would drop every other one.
        let mut clock = Clock::new(SR);
        clock.set_meter(meter(6, 8));
        clock.set_bpm(300.0, 0);
        let downbeat = clock.start_count_in(0, 0);
        assert_eq!(downbeat, 6 * 3200);
        let (fired, _) = pulses(&mut clock, downbeat, None);
        assert_eq!(fired.iter().map(|p| p.0).collect::<Vec<_>>(), (0..6).map(|k| k * 3200).collect::<Vec<_>>());
        let v = DEFAULT_CLICK_VOLUME as f64;
        assert_eq!(fired.iter().map(|p| p.1).collect::<Vec<_>>(), [1.0, 0.32, 0.32, 0.56, 0.32, 0.32].map(|peak| Some(peak * v)));
        // In 4/4 the guard is the old 0.12 s: two clicks 0.1 s apart (a re-anchor) keep one.
        let mut clock = Clock::new(SR);
        assert!(clock.trigger_click(0, ClickLevel::Accent, false));
        assert!(!clock.trigger_click(4800, ClickLevel::Beat, false));
        assert!(clock.trigger_click(5760, ClickLevel::Beat, false));
    }

    #[test]
    fn a_fade_ends_on_the_meters_bar_line() {
        for m in Meter::OFFERED {
            let mut clock = Clock::new(SR);
            clock.set_meter(m);
            let fpb = crate::grid::frames_per_bar(120.0, SR, m);
            clock.start_master(1000, 2 * fpb, 2, 1000);
            for (at, bars, want) in [(1000, 1, 1000 + fpb), (1001, 1, 1000 + 2 * fpb), (1000 + fpb / 2, 2, 1000 + 3 * fpb), (1000 + fpb, 0, 1000 + fpb)] {
                assert_eq!(clock.downbeat_after(at, bars), Some(want), "{m:?} at {at} + {bars} bars");
            }
        }
    }

    #[test]
    fn the_meter_locks_with_the_tempo() {
        let mut clock = Clock::new(SR);
        clock.set_locked(true);
        clock.set_meter(meter(3, 4));
        assert_eq!(clock.meter(), Meter::FOUR_FOUR, "ignored while locked");
        clock.set_locked(false);
        clock.set_meter(meter(3, 4));
        assert_eq!(clock.meter(), meter(3, 4));
    }

    #[test]
    fn a_free_run_meter_change_makes_the_next_pulse_a_downbeat() {
        let mut clock = Clock::new(SR);
        clock.ensure_running(0);
        let (_, beats) = pulses(&mut clock, 30_000, None);
        assert_eq!(beats.iter().map(|b| b.beat_in_bar).collect::<Vec<_>>(), [0, 1], "120 BPM in 4/4: beats at 0 and 24000");
        let next = clock.next_beat_frame().unwrap();
        clock.set_meter(meter(3, 4));
        assert_eq!(clock.next_beat_frame(), Some(next), "the next pulse keeps its frame");
        let (_, beats) = pulses(&mut clock, next + 3 * 24_000 + 1, None);
        assert_eq!(beats.iter().map(|b| (b.frame, b.beat_in_bar)).collect::<Vec<_>>(), [(next, 0), (next + 24_000, 1), (next + 48_000, 2), (next + 72_000, 0)]);
        // A compound meter's pulse is a third of its beat.
        clock.set_meter(meter(6, 8));
        let next = clock.next_beat_frame().unwrap();
        let (fired, beats) = pulses(&mut clock, next + 48_000, None);
        assert_eq!(fired.iter().map(|p| p.0 - next).collect::<Vec<_>>(), (0..6).map(|k| k * 8000).collect::<Vec<_>>());
        assert_eq!(beats.iter().map(|b| (b.frame - next, b.beat_in_bar)).collect::<Vec<_>>(), [(0, 0), (24_000, 1)]);
    }

    #[test]
    fn the_triangle_peaks_near_one_and_starts_at_zero() {
        assert_eq!(triangle(1000.0, 0.0, 48000.0), 0.0);
        let peak = (0..48).map(|k| triangle(1000.0, k as f64 / 48000.0, 48000.0).abs()).fold(0.0, f64::max);
        assert!((0.95..=1.01).contains(&peak), "peak={peak}");
    }

    #[test]
    fn the_click_envelope_rises_decays_and_stops() {
        let mut voice = ClickVoice::default();
        voice.start(100, ClickLevel::Accent, 1.0);
        let mut out = vec![0.0f32; 4000];
        voice.render(0, 48000, &mut out);
        assert!(out[..100].iter().all(|&s| s == 0.0));
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.5 && peak <= 1.0, "peak={peak}");
        let stop = 100 + (0.07f64 * 48000.0).round() as usize;
        assert!(out[stop..].iter().all(|&s| s == 0.0));
        assert!(out[stop - 10..stop].iter().all(|&s| s.abs() <= 1.1e-4));
        assert!(voice.start.is_none());
    }
}
