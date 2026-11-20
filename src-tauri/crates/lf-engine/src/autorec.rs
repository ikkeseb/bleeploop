//! OWNS: the AUTO REC onset detector, where a first take starts on a level trigger instead of a count-in
//! (`looper/auto-record.ts`). Every buffer is allocated in `new`; `scan` allocates nothing.
//!
//! It listens to the stereo record tap on the linked energy, `(l² + r²) / 2` per frame (a mono signal
//! reads as it did, bit for bit), never on the mean: an attack of opposite polarity on the two sides
//! would cancel there. Its look-back holds pairs, so the onset it copies into the loop is stereo.

pub const DEFAULT_SENSITIVITY: f64 = 50.0;
pub const MIN_SENSITIVITY: f64 = 1.0;
pub const MAX_SENSITIVITY: f64 = 100.0;
use crate::api::Pair;

const LOOKBACK_MS: f64 = 40.0;
const ANALYSIS_MS: f64 = 4.0;
const LEAST_SENSITIVE_DBFS: f64 = -12.0;
const MOST_SENSITIVE_DBFS: f64 = -60.0;
const ONSET_THRESHOLD_RATIO: f64 = 0.2;

/// Map 1..100 onto -12..-60 dBFS: a higher sensitivity is a lower linear RMS threshold.
pub fn threshold(sensitivity: f64) -> f64 {
    let s = if sensitivity.is_finite() { sensitivity } else { DEFAULT_SENSITIVITY };
    let fraction = (s.clamp(MIN_SENSITIVITY, MAX_SENSITIVITY) - MIN_SENSITIVITY) / (MAX_SENSITIVITY - MIN_SENSITIVITY);
    10f64.powf((LEAST_SENSITIVE_DBFS + (MOST_SENSITIVE_DBFS - LEAST_SENSITIVE_DBFS) * fraction) / 20.0)
}

fn onset_floor() -> f64 {
    10f64.powf(-72.0 / 20.0)
}

/// A frame's linked energy: the mean of its two sides' squares (for equal sides, the square itself,
/// exactly).
fn energy([l, r]: Pair) -> f64 {
    (l as f64 * l as f64 + r as f64 * r as f64) / 2.0
}

pub struct Detector {
    history: Vec<Pair>,
    energy_window: Vec<f64>,
    analysis: usize,
    history_write: usize,
    history_fill: usize,
    energy_write: usize,
    energy_fill: usize,
    energy_sum: f64,
    copied: usize,
}

impl Detector {
    pub fn new(sample_rate: u32) -> Self {
        let rate = if sample_rate > 0 { sample_rate as f64 } else { 48000.0 };
        let analysis = ((rate * ANALYSIS_MS / 1000.0).round() as usize).max(1);
        let lookback = ((rate * LOOKBACK_MS / 1000.0).round() as usize).max(analysis);
        Detector {
            history: vec![[0.0; 2]; lookback],
            energy_window: vec![0.0; analysis],
            analysis,
            history_write: 0,
            history_fill: 0,
            energy_write: 0,
            energy_fill: 0,
            energy_sum: 0.0,
            copied: 0,
        }
    }

    /// A fresh arm: older audio and partial RMS state cannot cross this point. Also called at a gap in
    /// the input (an xrun), so a retained onset is always one contiguous run of input.
    pub fn reset(&mut self) {
        self.history_write = 0;
        self.history_fill = 0;
        self.energy_write = 0;
        self.energy_fill = 0;
        self.energy_sum = 0.0;
        self.copied = 0;
    }

    /// Look-back frames the last trigger copied.
    pub fn copied(&self) -> usize {
        self.copied
    }

    /// Feed samples. `None` while listening. On a trigger the retained onset is copied into `output`
    /// and the index of the first sample not yet copied comes back, so the caller appends the rest once.
    pub fn scan(&mut self, data: &[Pair], threshold: f64, output: &mut [Pair]) -> Option<usize> {
        let threshold = if threshold.is_finite() && threshold > 0.0 { threshold } else { self::threshold(DEFAULT_SENSITIVITY) };
        let required = threshold * threshold * self.analysis as f64;
        self.copied = 0;
        for (i, &sample) in data.iter().enumerate() {
            self.push_history(sample);
            self.push_energy(energy(sample));
            if self.energy_fill == self.analysis && self.energy_sum >= required {
                let start = self.onset_start(threshold);
                let frames = (self.history_fill - start).min(output.len());
                for (k, out) in output[..frames].iter_mut().enumerate() {
                    *out = self.history_at(start + k);
                }
                self.copied = frames;
                return Some(i + 1);
            }
        }
        None
    }

    fn push_history(&mut self, sample: Pair) {
        self.history[self.history_write] = sample;
        self.history_write = (self.history_write + 1) % self.history.len();
        self.history_fill = (self.history_fill + 1).min(self.history.len());
    }

    fn push_energy(&mut self, square: f64) {
        if self.energy_fill == self.analysis {
            self.energy_sum -= self.energy_window[self.energy_write];
        } else {
            self.energy_fill += 1;
        }
        self.energy_window[self.energy_write] = square;
        self.energy_sum += square;
        self.energy_write = (self.energy_write + 1) % self.analysis;
    }

    fn history_at(&self, index: usize) -> Pair {
        let len = self.history.len();
        self.history[(self.history_write + len - self.history_fill + index) % len]
    }

    /// Start one analysis block before the first soft activity, never at the full history blindly.
    fn onset_start(&self, threshold: f64) -> usize {
        match self.first_active_block(threshold) {
            Some(block) => block.saturating_sub(self.analysis),
            None => self.history_fill.saturating_sub(self.analysis),
        }
    }

    fn first_active_block(&self, threshold: f64) -> Option<usize> {
        let onset = onset_floor().max(threshold * ONSET_THRESHOLD_RATIO);
        let onset_energy = onset * onset;
        (0..self.history_fill).step_by(self.analysis).find(|&start| {
            let end = self.history_fill.min(start + self.analysis);
            let sum: f64 = (start..end).map(|i| energy(self.history_at(i))).sum();
            sum / (end - start) as f64 >= onset_energy
        })
    }
}

#[cfg(test)]
mod tests {
    // Ports verify/guards/auto-record.mjs. Its "historyIsQuiet" checks go: the engine resets the history
    // at an input gap instead of judging which losses a future onset could contain.
    use super::*;

    /// The detector fed a mono signal as a pair of equal sides (the ported guard's cases), reading back
    /// the onset's left side once both sides proved equal.
    struct Mono(Detector);

    impl Mono {
        fn new(sample_rate: u32) -> Mono {
            Mono(Detector::new(sample_rate))
        }

        fn scan(&mut self, data: &[f32], threshold: f64, output: &mut [f32]) -> Option<usize> {
            let pairs: Vec<Pair> = data.iter().map(|&x| [x, x]).collect();
            let mut out = vec![[0.0f32; 2]; output.len()];
            let at = self.0.scan(&pairs, threshold, &mut out);
            for (o, &[l, r]) in output.iter_mut().zip(&out[..self.0.copied()]) {
                assert_eq!(l.to_bits(), r.to_bits());
                *o = l;
            }
            at
        }

        fn copied(&self) -> usize {
            self.0.copied()
        }

        fn reset(&mut self) {
            self.0.reset();
        }
    }

    #[test]
    fn an_attack_of_opposite_polarity_triggers_and_is_copied_as_a_pair() {
        let mut d = Detector::new(1000);
        let mut out = [[0.0f32; 2]; 100];
        let data: Vec<Pair> = [[0.0, 0.0]; 20].iter().chain(&[[0.3, -0.3]; 8]).copied().collect();
        let offset = d.scan(&data, 0.2, &mut out).expect("l = -r carries its energy");
        assert_eq!(&out[d.copied() - 1], &[0.3, -0.3]);
        assert!(offset <= data.len());
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-7
    }

    #[test]
    fn sensitivity_maps_onto_a_useful_dbfs_range() {
        assert!(approx(threshold(1.0), 10f64.powf(-12.0 / 20.0)));
        assert!(approx(threshold(100.0), 10f64.powf(-60.0 / 20.0)));
        assert!(threshold(100.0) < threshold(DEFAULT_SENSITIVITY) && threshold(DEFAULT_SENSITIVITY) < threshold(1.0));
        assert_eq!(threshold(-20.0), threshold(1.0));
        assert_eq!(threshold(140.0), threshold(100.0));
    }

    #[test]
    fn silence_and_sub_threshold_sound_do_not_trigger() {
        let mut d = Mono::new(1000); // 4-frame RMS window, 40-frame history
        let mut out = [0.0; 100];
        assert_eq!(d.scan(&[0.01; 30], 0.2, &mut out), None);
        assert_eq!(d.scan(&[0.05; 20], 0.2, &mut out), None);
        assert_eq!(d.copied(), 0);
    }

    #[test]
    fn a_trigger_recovers_the_soft_onset_and_appends_the_batch_once() {
        let mut d = Mono::new(1000);
        let mut out = [0.0; 100];
        let data: Vec<f32> = [0.0; 20].iter().chain(&[0.05; 8]).chain(&[0.2; 8]).copied().collect();
        let offset = d.scan(&data, 0.2, &mut out).unwrap();
        assert_eq!((offset, d.copied()), (32, 16));
        let combined: Vec<f32> = out[..16].iter().chain(&data[offset..]).copied().collect();
        assert_eq!(combined, data[16..]);
    }

    #[test]
    fn a_batch_boundary_cannot_hide_or_duplicate_the_onset() {
        let mut d = Mono::new(1000);
        let mut out = [0.0; 100];
        let before: Vec<f32> = [0.0; 20].iter().chain(&[0.05; 8]).copied().collect();
        assert_eq!(d.scan(&before, 0.2, &mut out), None);
        let crossing = [0.2; 8];
        let offset = d.scan(&crossing, 0.2, &mut out).unwrap();
        assert_eq!(offset, 4);
        let combined: Vec<f32> = out[..d.copied()].iter().chain(&crossing[offset..]).copied().collect();
        let stream: Vec<f32> = before.iter().chain(&crossing).copied().collect();
        assert_eq!(combined, stream[16..]);
    }

    #[test]
    fn reset_and_history_wrap_cannot_leak_an_older_arm() {
        let mut d = Mono::new(1000);
        let mut out = [0.0; 100];
        d.scan(&[0.04; 35], 0.2, &mut out);
        d.reset();
        assert_eq!(d.scan(&[0.2; 4], 0.2, &mut out), Some(4));
        assert_eq!((d.copied(), &out[..4]), (4, &[0.2f32; 4][..]));
        d.reset();
        d.scan(&[0.0; 50], 0.2, &mut out);
        let tail: Vec<f32> = [0.05; 8].iter().chain(&[0.2; 4]).copied().collect();
        assert_eq!(d.scan(&tail, 0.2, &mut out), Some(12));
        assert_eq!(d.copied(), 16);
    }
}
