//! OWNS: tempo (the BPM and its lock) and the beat pulse: ONE grid that serves the free-run, count-in and
//! master anchors, the metronome click it triggers and the click's anti-flam guard. Ported from
//! `src/audio/clock.ts`; tap tempo stays in the UI and arrives as a tempo command.
//!
//! No lookahead scheduler: a beat fires on the exact frame its grid puts it, inside `process`. Only a
//! jump in the device frame counter (a callback the device never delivered) can leave beats behind:
//! those are dropped, except count-in beats, which fire late on the first frame after the jump so the
//! count stays complete (the Web Audio pulse's stalled-waker rule).

use crate::grid::{clamp_bpm, Frame, Grid, BEATS_PER_BAR};

/// Minimum spacing between two clicks: only a true coincidence of two grids at a re-anchor is closer
/// (quarter notes are at least 0.2 s apart at 300 bpm).
const MIN_CLICK_SPACING_CENTIS: i64 = 12;

/// Default click level (0..1), as the UI persists it.
pub const DEFAULT_CLICK_VOLUME: f32 = 0.7;

#[derive(Clone, Copy, Debug)]
struct Pulse {
    grid: Grid,
    /// The next beat index to fire.
    next: u64,
    /// Beats below this index are count-in beats: they click even with the metronome off.
    forced_until: u64,
    /// Only a free-running pulse follows a tempo change.
    free_run: bool,
}

/// One beat the pulse fired: what the beat LED shows, and whether it clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Beat {
    pub frame: Frame,
    /// Position in the bar, 0 = the accented downbeat.
    pub beat_in_bar: u8,
    /// Count-in beats still to come including this one (4, 3, 2, 1), 0 outside a count.
    pub count_left: u8,
    pub clicked: bool,
}

pub struct Clock {
    sample_rate: u32,
    bpm: u32,
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

    pub fn locked(&self) -> bool {
        self.locked
    }

    pub fn metronome(&self) -> bool {
        self.metronome
    }

    pub fn click_volume(&self) -> f32 {
        self.click_volume
    }

    /// The frame the pulse fires its next beat on; `None` before the transport runs.
    pub fn next_beat_frame(&self) -> Option<Frame> {
        self.pulse.map(|p| p.grid.beat_frame(p.next))
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
            grid: Grid::tempo(base_frame, base_index, self.bpm, self.sample_rate),
            next: base_index,
            forced_until: 0,
            free_run: true,
        });
    }

    /// A first-track count-in: `beats` forced beats from `anchor` at the current tempo; the take's
    /// frame 0 is beat `beats`. Clears the anti-flam reference so a fast abort and re-record can never
    /// swallow the new count "1".
    pub fn start_count_in(&mut self, anchor: Frame, beats: u64, now: Frame) {
        self.last_click = None;
        let grid = Grid::tempo(anchor, 0, self.bpm, self.sample_rate);
        self.pulse = Some(Pulse { grid, next: grid.first_beat_at_or_after(now), forced_until: beats, free_run: false });
    }

    /// Abort a count-in: carry its grid into free-run.
    pub fn stop_count_in(&mut self, now: Frame) {
        self.start_free_run(now);
    }

    /// Lock the pulse to a committed master: `4 * bars` beats per `master` frames from `anchor`, which may
    /// lie in the past (the counted downbeat); the next beat is the first at or after `now`. Keeps the
    /// anti-flam reference, so a count beat that just sounded cannot double with the master's.
    pub fn start_master(&mut self, anchor: Frame, master: Frame, bars: Frame, now: Frame) {
        let grid = Grid::master(anchor, master, bars);
        self.pulse = Some(Pulse { grid, next: grid.first_beat_at_or_after(now), forced_until: 0, free_run: false });
    }

    /// AUTO REC: the current tempo from the detected onset, with no count beat to carry.
    pub fn start_auto_record(&mut self, anchor: Frame, now: Frame) {
        self.last_click = None;
        let grid = Grid::tempo(anchor, 0, self.bpm, self.sample_rate);
        self.pulse = Some(Pulse { grid, next: grid.first_beat_at_or_after(now), forced_until: 0, free_run: false });
    }

    /// Master reset: carry the loop grid into free-run.
    pub fn stop_master(&mut self, now: Frame) {
        self.start_free_run(now);
    }

    /// True while a count-in pulse still has forced beats to fire.
    pub fn counting(&self) -> bool {
        self.pulse.is_some_and(|p| p.next < p.forced_until)
    }

    /// Fire the next beat due at or before `frame`, or `None` when the next beat lies ahead. A beat
    /// the frame counter jumped over is dropped, unless it is a count beat: that one fires late, on
    /// `frame`. The click sounds on a count beat, or with the metronome on while the transport is
    /// active (a transport active `until` a loop-end stop clicks strictly before it).
    pub fn fire_due(&mut self, frame: Frame, transport_until: Option<Frame>) -> Option<Beat> {
        loop {
            let pulse = self.pulse.as_mut()?;
            let at = pulse.grid.beat_frame(pulse.next);
            if at > frame {
                return None;
            }
            let n = pulse.next;
            pulse.next += 1;
            let forced = n < pulse.forced_until;
            if at < frame && !forced {
                continue;
            }
            let count_left = if forced { (pulse.forced_until - n) as u8 } else { 0 };
            let beat_in_bar = (n % BEATS_PER_BAR) as u8;
            let gate = forced || (self.metronome && transport_until.is_some_and(|until| frame < until));
            let clicked = gate && self.trigger_click(frame, beat_in_bar == 0, at < frame);
            return Some(Beat { frame, beat_in_bar, count_left, clicked });
        }
    }

    /// Anti-flam: a click closer than 0.12 s to the previous one is dropped, except a beat on time
    /// after a late count beat (the come-in "1" must sound).
    fn trigger_click(&mut self, frame: Frame, accent: bool, late: bool) -> bool {
        if self.click_volume <= 0.0 {
            return false;
        }
        let sr = self.sample_rate as i64;
        let near = self.last_click.is_some_and(|last| (frame - last).abs() * 100 < MIN_CLICK_SPACING_CENTIS * sr);
        let on_time_after_late = self.last_click_late && !late;
        if near && !on_time_after_late {
            return false;
        }
        self.last_click = Some(frame);
        self.last_click_late = late;
        self.click.start(frame, accent, self.click_volume);
        true
    }

    /// Add the click for frames `[frame, frame + out.len())` into `out`.
    pub fn render_click(&mut self, frame: Frame, out: &mut [f32]) {
        self.click.render(frame, self.sample_rate, out);
    }
}

/// The metronome blip: a band-limited triangle at 1 kHz (1.5 kHz accented), a 2 ms linear attack to
/// its peak, an exponential decay to 0.0001 at 60 ms, silence from 70 ms (clock.ts `triggerClick`).
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
    fn start(&mut self, frame: Frame, accent: bool, volume: f32) {
        // Twice the old 0.5 / 0.28 (by ear, 2026-09-24); the master limiter catches the sum.
        self.start = Some(frame);
        self.accent = accent;
        self.peak = if accent { 1.0 } else { 0.56 } * volume as f64;
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

    #[test]
    fn the_triangle_peaks_near_one_and_starts_at_zero() {
        assert_eq!(triangle(1000.0, 0.0, 48000.0), 0.0);
        let peak = (0..48).map(|k| triangle(1000.0, k as f64 / 48000.0, 48000.0).abs()).fold(0.0, f64::max);
        assert!((0.95..=1.01).contains(&peak), "peak={peak}");
    }

    #[test]
    fn the_click_envelope_rises_decays_and_stops() {
        let mut voice = ClickVoice::default();
        voice.start(100, true, 1.0);
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
