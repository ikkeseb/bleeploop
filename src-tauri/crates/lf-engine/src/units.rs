//! OWNS: the built-in chain units implementing [`UnitProcessor`] (`docs/unit-contract.md` §5).
//!
//! Holds ECHO ([`EchoUnit`]), RING MOD ([`RingUnit`]), REVERB ([`ReverbUnit`]) and OTT ([`OttUnit`]),
//! each reusing [`crate::input_fx`]'s or [`crate::dsp::ott`]'s DSP without rewriting it so that
//! alone, with equal channels, each matches today's path.
//!
//! - [`EchoUnit`] and [`RingUnit`] are zero-latency stereo inserts (`input + wet` per channel):
//!   ECHO runs one independent feedback delay line per channel (two ECHOs are two lines),
//!   tempo-synced from [`UnitCx::bpm`] and [`UnitCx::meter`], with 20 ms ramps from the next
//!   128-frame DSP quantum boundary; RING MOD runs one carrier-modulated wet path per channel with
//!   the same 20 ms ramps.
//! - [`ReverbUnit`] is a zero-latency stereo insert (`input + wet`): feeds the pair's mean
//!   ([`crate::fold`]) to its convolver and adds the convolver's left and right wet tails to their
//!   own channels, with 20 ms ramps from the next quantum boundary.
//! - [`OttUnit`] wraps [`StereoOtt`] (`Ott<2>`, detectors linked across channels) and folds its
//!   per-band applied gain (dB) and linked follower (dBFS, floor −120) per sample for
//!   [`UnitProcessor::take_meter`].
//! - [`SynthUnit`] is SYNTH (`docs/unit-contract.md` §4), a source: today's bank of six instruments
//!   (`instruments`' `Bank`, the DSP moved, not rewritten), all six rendering every frame, the picked one
//!   taking the notes and the wheels ([`SlotEventKind`]), each at its level. It reports
//!   [`instruments::LEAD`] as its latency, since a note sounds that long after its frame, so its slot's
//!   source record line lags it exactly as [`crate::instruments::Instruments`]' does. It starts with no
//!   instrument picked: silent until [`UnitParam::Instrument`] picks one (the host sends the pick with the
//!   unit's params). The seat does its bypass (a bypassed source is silent and still gets its notes).
//! - The four effects bypass themselves (`set_bypass` returns `true`): ECHO, REVERB and RING MOD close
//!   their input gate over 20 ms from the command's frame (dry passes untouched, ECHO and REVERB
//!   tails ring out, and wherever the wet sample is exact zero the output is the input bit for bit);
//!   OTT crossfades linearly between dry and processed over 10 ms (settled off is exact input bit
//!   for bit). Only an install's own starting bypassed state — or a bypass arriving before the
//!   unit's first rendered frame (nothing has sounded, so nothing clicks or leaks) — settles
//!   immediately; every later un-bypass fades in from the command's frame.
//!
//! Everything is allocated in `new` / `with_ir` / `with_noise`; `process`, `set_param`, `set_bypass`, `take_meter`
//! and `stop` never allocate, lock or block (invariant 5).

use std::any::Any;
use std::sync::Arc;

use crate::api::{
    BandMeter, InputSendParam, Instrument, SlotEvent, SlotEventKind, UnitCx, UnitInfo, UnitKind, UnitParam,
    UnitProcessor, BAND_LEVEL_FLOOR_DB,
};
use crate::dsp::buffer_source::AudioBuffer;
use crate::dsp::fx::Ctl;
use crate::dsp::noise::NoiseTables;
use crate::dsp::ott::{Setting, StereoOtt};
use crate::dsp::param::QUANTUM;
use crate::dsp::rng::Mulberry32;
use crate::effects;
use crate::engine::NOISE_SEED;
use crate::grid::frames_per_bar;
use crate::input_fx::{Echo, Reverb, Ring};
use crate::instruments::{self, Bank};

/// Stereo ECHO insert unit (`UnitKind::Echo`): one [`Echo`] feedback delay line per channel.
pub struct EchoUnit {
    sample_rate: u32,
    /// Quarter-note period (as f64 bits) last applied to both channels; `None` before the first render
    /// (matching `InputFx`).
    beat_period: Option<u64>,
    channels: [Echo; 2],
    /// The DSP quantum whose control values are computed.
    prepared: Option<u64>,
    /// True until the first `set_bypass` or rendered frame: only a cold install arriving bypassed
    /// settles immediately (no 20 ms wet leak); every later un-bypass fades in over 20 ms.
    starting: bool,
    wet: [f32; QUANTUM],
}

impl EchoUnit {
    /// Allocates both channels' 2 s delay lines: build off the audio thread. Starts active (not
    /// bypassed) at the default ECHO parameters (`1/8`, feedback `0.4`, level `0.5`).
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate as f32;
        let start = Ctl::at(0, sr);
        EchoUnit {
            sample_rate,
            beat_period: None,
            channels: [Echo::new_on(sr, start), Echo::new_on(sr, start)],
            prepared: None,
            starting: true,
            wet: [0.0; QUANTUM],
        }
    }

    fn ctl(&self, cx: &UnitCx) -> Ctl {
        Ctl::at(cx.dsp_frame.max(0) as u64, self.sample_rate as f32)
    }

    fn follow_tempo(&mut self, cx: &UnitCx) {
        if !(cx.bpm > 0.0 && cx.bpm.is_finite()) {
            return;
        }
        let sr = self.sample_rate as f64;
        let beat_period = frames_per_bar(cx.bpm, self.sample_rate, cx.meter) as f64 / sr / cx.meter.quarters_per_bar();
        if self.beat_period == Some(beat_period.to_bits()) {
            return;
        }
        self.beat_period = Some(beat_period.to_bits());
        let ctl = self.ctl(cx);
        for ch in &mut self.channels {
            ch.set_beat_period(beat_period, ctl);
        }
    }
}

impl UnitProcessor for EchoUnit {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Echo, source: false, latency: 0 }
    }

    fn process(&mut self, cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        self.follow_tempo(cx);
        let len = out[0].len();
        if len == 0 {
            return;
        }
        self.starting = false;
        let [ol, or] = out;
        let q = QUANTUM as u64;
        let start = cx.dsp_frame.max(0) as u64;
        let mut done = 0;
        while done < len {
            let f = start + done as u64;
            let quantum_start = f - f % q;
            if self.prepared != Some(quantum_start) {
                for ch in &mut self.channels {
                    ch.begin_quantum(quantum_start);
                }
                self.prepared = Some(quantum_start);
            }
            let at = (f - quantum_start) as usize;
            let n = (QUANTUM - at).min(len - done);
            let span = done..done + n;
            let wet = &mut self.wet[..n];
            for (ch, (x, y)) in self.channels.iter_mut().zip([(&input[0][span.clone()], &mut ol[span.clone()]), (&input[1][span.clone()], &mut or[span])]) {
                if ch.process(at, x, wet) && ch.has_level() {
                    for (o, (&dry, &w)) in y.iter_mut().zip(x.iter().zip(wet.iter())) {
                        *o = if w == 0.0 { dry } else { dry + w };
                    }
                } else {
                    y.copy_from_slice(x);
                }
            }
            done += n;
        }
    }

    fn set_param(&mut self, cx: &UnitCx, param: UnitParam, value: f64) -> Option<f64> {
        let UnitParam::Send(p @ (InputSendParam::EchoTime | InputSendParam::EchoFeedback | InputSendParam::EchoLevel)) = param else {
            return None;
        };
        if !value.is_finite() {
            return None;
        }
        // Do not call `follow_tempo` here: `process` follows tempo on this frame after all same-frame
        // commands have applied, matching `InputFx` (where a same-frame tempo change cancels an
        // `EchoTime` ramp and snaps to the new time in either command order).
        let ctl = self.ctl(cx);
        let applied = self.channels[0].set_param(p, value, ctl);
        self.channels[1].set_param(p, value, ctl);
        Some(applied)
    }

    fn set_bypass(&mut self, cx: &UnitCx, bypassed: bool) -> bool {
        let on = !bypassed;
        let starting = std::mem::replace(&mut self.starting, false);
        if starting && bypassed {
            for ch in &mut self.channels {
                ch.snap_on(false);
            }
            return true;
        }
        let ctl = self.ctl(cx);
        if self.channels[0].is_settled(on, ctl) {
            return true;
        }
        for ch in &mut self.channels {
            ch.set_on(on, ctl);
        }
        true
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

/// Stereo RING MOD insert unit (`UnitKind::Ring`): one [`Ring`] modulator per channel.
pub struct RingUnit {
    sample_rate: u32,
    channels: [Ring; 2],
    /// The DSP quantum whose control values are computed.
    prepared: Option<u64>,
    /// True until the first `set_bypass` or rendered frame: only a cold install arriving bypassed
    /// settles immediately (no 20 ms wet leak); every later un-bypass fades in over 20 ms.
    starting: bool,
    wet: [f32; QUANTUM],
}

impl RingUnit {
    /// Allocates both channels' parameter timelines: build off the audio thread. Starts active (not
    /// bypassed) at the default RING MOD parameters (`440 Hz`, level `0.5`).
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate as f32;
        let start = Ctl::at(0, sr);
        RingUnit {
            sample_rate,
            channels: [Ring::new_on(sr, start), Ring::new_on(sr, start)],
            prepared: None,
            starting: true,
            wet: [0.0; QUANTUM],
        }
    }

    /// Each channel's carrier phase at the next frame it renders, in radians in [0, 2π).
    pub fn phase(&self) -> [f64; 2] {
        [self.channels[0].phase(), self.channels[1].phase()]
    }

    fn ctl(&self, cx: &UnitCx) -> Ctl {
        Ctl::at(cx.dsp_frame.max(0) as u64, self.sample_rate as f32)
    }
}

impl UnitProcessor for RingUnit {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Ring, source: false, latency: 0 }
    }

    fn process(&mut self, cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let len = out[0].len();
        if len == 0 {
            return;
        }
        self.starting = false;
        let [ol, or] = out;
        let q = QUANTUM as u64;
        let start = cx.dsp_frame.max(0) as u64;
        let mut done = 0;
        while done < len {
            let f = start + done as u64;
            let quantum_start = f - f % q;
            if self.prepared != Some(quantum_start) {
                for ch in &mut self.channels {
                    ch.begin_quantum(quantum_start);
                }
                self.prepared = Some(quantum_start);
            }
            let at = (f - quantum_start) as usize;
            let n = (QUANTUM - at).min(len - done);
            let span = done..done + n;
            let wet = &mut self.wet[..n];
            for (ch, (x, y)) in self.channels.iter_mut().zip([(&input[0][span.clone()], &mut ol[span.clone()]), (&input[1][span.clone()], &mut or[span])]) {
                if ch.process(at, x, wet) && ch.has_level() {
                    for (o, (&dry, &w)) in y.iter_mut().zip(x.iter().zip(wet.iter())) {
                        *o = if w == 0.0 { dry } else { dry + w };
                    }
                } else {
                    y.copy_from_slice(x);
                }
            }
            done += n;
        }
    }

    fn set_param(&mut self, cx: &UnitCx, param: UnitParam, value: f64) -> Option<f64> {
        let UnitParam::Send(p @ (InputSendParam::RingFreq | InputSendParam::RingLevel)) = param else {
            return None;
        };
        if !value.is_finite() {
            return None;
        }
        let ctl = self.ctl(cx);
        let applied = self.channels[0].set_param(p, value, ctl);
        self.channels[1].set_param(p, value, ctl);
        Some(applied)
    }

    fn set_bypass(&mut self, cx: &UnitCx, bypassed: bool) -> bool {
        let on = !bypassed;
        let starting = std::mem::replace(&mut self.starting, false);
        if starting && bypassed {
            for ch in &mut self.channels {
                ch.snap_on(false);
            }
            return true;
        }
        let ctl = self.ctl(cx);
        if self.channels[0].is_settled(on, ctl) {
            return true;
        }
        for ch in &mut self.channels {
            ch.set_on(on, ctl);
        }
        true
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

/// Generates the engine's standard stereo reverb impulse response at `sample_rate` (identical to
/// [`crate::Engine::new`]). Allocates: call off the audio thread.
pub fn default_reverb_ir(sample_rate: u32) -> [Vec<f32>; 2] {
    let [white, _] = tone_noise(sample_rate);
    effects::reverb_ir(sample_rate, &white)
}

/// Tone's white and pink noise tables at `sample_rate`, as [`crate::Engine::new`] generates them (the
/// drum kit's and the reverb IR's). Allocates: call off the audio thread.
pub fn tone_noise(sample_rate: u32) -> [Arc<AudioBuffer>; 2] {
    let NoiseTables { white, pink } = NoiseTables::generate(&mut Mulberry32::new(NOISE_SEED));
    [white, pink].map(|[l, r]| Arc::new(AudioBuffer::new(sample_rate as f32, vec![l, r])))
}

/// Stereo REVERB insert unit (`UnitKind::Reverb`): feeds the pair's mean ([`crate::fold`]) to its
/// convolver and adds its left and right wet tails to their own channels.
pub struct ReverbUnit {
    sample_rate: u32,
    reverb: Reverb,
    /// The DSP quantum whose control values are computed.
    prepared: Option<u64>,
    /// True until the first `set_bypass` or rendered frame: only a cold install arriving bypassed
    /// settles immediately (no 20 ms wet leak); every later un-bypass fades in over 20 ms.
    starting: bool,
    mono: [f32; QUANTUM],
    wet: [[f32; QUANTUM]; 2],
}

impl ReverbUnit {
    /// Allocates the impulse response and convolver: build off the audio thread. Starts active (not
    /// bypassed) at the default REVERB parameter (level `0.5`).
    pub fn new(sample_rate: u32) -> Self {
        let ir = default_reverb_ir(sample_rate);
        ReverbUnit::with_ir(sample_rate, [&ir[0], &ir[1]])
    }

    /// Build over a precomputed stereo impulse response (`[left, right]`): build off the audio thread.
    /// Starts active (not bypassed) at the default REVERB parameter (level `0.5`).
    pub fn with_ir(sample_rate: u32, ir: [&[f32]; 2]) -> Self {
        let sr = sample_rate as f32;
        let start = Ctl::at(0, sr);
        ReverbUnit {
            sample_rate,
            reverb: Reverb::new_on(sr, ir, start),
            prepared: None,
            starting: true,
            mono: [0.0; QUANTUM],
            wet: [[0.0; QUANTUM]; 2],
        }
    }

    fn ctl(&self, cx: &UnitCx) -> Ctl {
        Ctl::at(cx.dsp_frame.max(0) as u64, self.sample_rate as f32)
    }
}

impl UnitProcessor for ReverbUnit {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Reverb, source: false, latency: 0 }
    }

    fn process(&mut self, cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let len = out[0].len();
        if len == 0 {
            return;
        }
        self.starting = false;
        let [ol, or] = out;
        let q = QUANTUM as u64;
        let start = cx.dsp_frame.max(0) as u64;
        let mut done = 0;
        while done < len {
            let f = start + done as u64;
            let quantum_start = f - f % q;
            if self.prepared != Some(quantum_start) {
                self.reverb.begin_quantum(quantum_start);
                self.prepared = Some(quantum_start);
            }
            let at = (f - quantum_start) as usize;
            let n = (QUANTUM - at).min(len - done);
            let span = done..done + n;
            let mono = &mut self.mono[..n];
            for ((m, &l), &r) in mono.iter_mut().zip(&input[0][span.clone()]).zip(&input[1][span.clone()]) {
                *m = crate::fold(l, r);
            }
            let [wl, wr] = &mut self.wet;
            let (wl, wr) = (&mut wl[..n], &mut wr[..n]);
            if self.reverb.process_stereo(f, at, mono, wl, wr) && self.reverb.has_level() {
                for (o, (&dry, &w)) in ol[span.clone()].iter_mut().zip(input[0][span.clone()].iter().zip(wl.iter())) {
                    *o = if w == 0.0 { dry } else { dry + w };
                }
                for (o, (&dry, &w)) in or[span.clone()].iter_mut().zip(input[1][span].iter().zip(wr.iter())) {
                    *o = if w == 0.0 { dry } else { dry + w };
                }
            } else {
                ol[span.clone()].copy_from_slice(&input[0][span.clone()]);
                or[span.clone()].copy_from_slice(&input[1][span]);
            }
            done += n;
        }
    }

    fn set_param(&mut self, cx: &UnitCx, param: UnitParam, value: f64) -> Option<f64> {
        let UnitParam::Send(InputSendParam::ReverbLevel) = param else {
            return None;
        };
        if !value.is_finite() {
            return None;
        }
        let ctl = self.ctl(cx);
        Some(self.reverb.set_level(value, ctl))
    }

    fn set_bypass(&mut self, cx: &UnitCx, bypassed: bool) -> bool {
        let on = !bypassed;
        let starting = std::mem::replace(&mut self.starting, false);
        if starting && bypassed {
            self.reverb.snap_on(false);
            return true;
        }
        let ctl = self.ctl(cx);
        if self.reverb.is_settled(on, ctl) {
            return true;
        }
        self.reverb.set_on(on, ctl);
        true
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

/// Linear amplitude corresponding to [`BAND_LEVEL_FLOOR_DB`] (−120 dBFS = 10^(−120 / 20)).
const BAND_LEVEL_FLOOR_LIN: f64 = 1e-6;

/// Stereo OTT unit (`UnitKind::Ott`): [`StereoOtt`] (`Ott<2>`, detectors linked across channels)
/// with per-sample band meter folding for [`UnitProcessor::take_meter`].
pub struct OttUnit {
    ott: StereoOtt,
    /// True until the first `set_bypass` or rendered frame: only a cold install arriving bypassed
    /// settles immediately (no 10 ms fade from active); every later un-bypass fades in over 10 ms.
    starting: bool,
    meter_has_samples: bool,
    gain_last: [f64; 3],
    gain_min: [f64; 3],
    gain_max: [f64; 3],
    env_last: [f64; 3],
    env_peak: [f64; 3],
}

impl OttUnit {
    /// Starts active (not bypassed, settled at weight 1.0) at the default OTT parameters
    /// (`depth 0.5`, `time 0.5`, `upward 1.0`, `downward 1.0`).
    pub fn new(sample_rate: u32) -> Self {
        OttUnit {
            ott: StereoOtt::new_on(sample_rate),
            starting: true,
            meter_has_samples: false,
            gain_last: [1.0; 3],
            gain_min: [1.0; 3],
            gain_max: [1.0; 3],
            env_last: [0.0; 3],
            env_peak: [0.0; 3],
        }
    }

    /// The processed signal's share of the output, 0..1: 0 bypassed and settled, 1 active and settled.
    pub fn weight(&self) -> f64 {
        self.ott.weight()
    }

    /// DEPTH, TIME, UP and DOWN as set (a glide's target).
    pub fn controls(&self) -> [f64; 4] {
        self.ott.controls()
    }

    /// The last frame's bands, low, mid and high, each per channel, before their gains.
    pub fn bands(&self) -> [[f64; 2]; 3] {
        self.ott.bands()
    }

    /// The last frame's linear gain per band (the controls and the makeup applied).
    pub fn gains(&self) -> [f64; 3] {
        self.ott.gains()
    }

    /// The last frame's linked peak follower per band, linear amplitude.
    pub fn env(&self) -> [f64; 3] {
        self.ott.env()
    }
}

impl UnitProcessor for OttUnit {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Ott, source: false, latency: 0 }
    }

    fn process(&mut self, _cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let len = out[0].len();
        if len == 0 {
            return;
        }
        self.starting = false;
        let [ol, or] = out;
        for ((&l, &r), (yl, yr)) in input[0].iter().zip(input[1].iter()).zip(ol.iter_mut().zip(or.iter_mut())) {
            [*yl, *yr] = self.ott.frame([l, r]);
            let g = self.ott.gains();
            let e = self.ott.env();
            if !self.meter_has_samples {
                self.gain_last = g;
                self.gain_min = g;
                self.gain_max = g;
                self.env_last = e;
                self.env_peak = e;
                self.meter_has_samples = true;
            } else {
                self.gain_last = g;
                self.env_last = e;
                for b in 0..3 {
                    self.gain_min[b] = self.gain_min[b].min(g[b]);
                    self.gain_max[b] = self.gain_max[b].max(g[b]);
                    self.env_peak[b] = self.env_peak[b].max(e[b]);
                }
            }
        }
    }

    fn set_param(&mut self, _cx: &UnitCx, param: UnitParam, value: f64) -> Option<f64> {
        if !value.is_finite() {
            return None;
        }
        let setting = match param {
            UnitParam::Send(InputSendParam::OttDepth) => Setting::Depth(value),
            UnitParam::Send(InputSendParam::OttTime) => Setting::Time(value),
            UnitParam::Send(InputSendParam::OttUpward) => Setting::Upward(value),
            UnitParam::Send(InputSendParam::OttDownward) => Setting::Downward(value),
            _ => return None,
        };
        let clamped = value.clamp(0.0, 1.0);
        self.ott.set(setting);
        Some(clamped)
    }

    fn set_bypass(&mut self, _cx: &UnitCx, bypassed: bool) -> bool {
        let starting = std::mem::replace(&mut self.starting, false);
        if starting && bypassed {
            self.ott.snap_on(false);
            return true;
        }
        self.ott.set(Setting::On(!bypassed));
        true
    }

    fn take_meter(&mut self, bands: &mut [BandMeter; 3]) -> bool {
        if !self.meter_has_samples {
            return false;
        }
        let level_db = |e: f64| -> f32 {
            if e <= BAND_LEVEL_FLOOR_LIN {
                BAND_LEVEL_FLOOR_DB
            } else {
                (20.0 * e.log10()) as f32
            }
        };
        for (b, slot) in bands.iter_mut().enumerate() {
            *slot = BandMeter {
                gain: (20.0 * self.gain_last[b].log10()) as f32,
                gain_min: (20.0 * self.gain_min[b].log10()) as f32,
                gain_max: (20.0 * self.gain_max[b].log10()) as f32,
                level: level_db(self.env_last[b]),
                level_peak: level_db(self.env_peak[b]),
            };
        }
        self.meter_has_samples = false;
        true
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

/// SYNTH's instruments in the UI's `SYNTHS` order (`src/ui/state/instruments.ts`): what
/// [`UnitParam::Instrument`]'s index names, mapped through [`Instrument::from_id`] (never the enum's own
/// order, where 1 is Pad).
pub const SYNTH_IDS: [&str; 6] = ["lead", "bass", "pad", "piano", "organ", "drum"];

/// SYNTH's level ceiling: the UI's `SLOT_GAIN_MAX` (`src/ui/state/instrument.ts`).
pub const SYNTH_GAIN_MAX: f64 = 1.5;

/// SYNTH (`UnitKind::Synth`, `docs/unit-contract.md` §4): today's bank of six built-in instruments as one
/// source unit. Notes and wheels arrive as its slot's events and apply on their frames, the bank rendered
/// up to each; a note sounds [`instruments::LEAD`] after its frame.
pub struct SynthUnit {
    bank: Bank,
}

impl SynthUnit {
    /// Allocates the six instruments and Tone's noise tables: build off the audio thread. No instrument
    /// is picked, every level is 1 and the wheels are at rest.
    pub fn new(sample_rate: u32) -> Self {
        let [white, pink] = tone_noise(sample_rate);
        SynthUnit::with_noise(sample_rate, &white, &pink)
    }

    /// [`SynthUnit::new`] over noise tables already made ([`tone_noise`]): build off the audio thread.
    pub fn with_noise(sample_rate: u32, white: &Arc<AudioBuffer>, pink: &Arc<AudioBuffer>) -> Self {
        SynthUnit { bank: Bank::new(sample_rate, white, pink) }
    }

    /// The instrument that takes the notes, if one is picked.
    pub fn instrument(&self) -> Option<Instrument> {
        self.bank.selected()
    }
}

impl UnitProcessor for SynthUnit {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Synth, source: true, latency: instruments::LEAD }
    }

    fn process(&mut self, cx: &UnitCx, _input: [&[f32]; 2], events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let [ol, or] = out;
        let len = ol.len();
        let start = cx.dsp_frame.max(0) as u64;
        let mut done = 0;
        for e in events {
            let at = (e.offset as usize).min(len);
            if at > done {
                self.bank.render(start + done as u64, &mut ol[done..at], &mut or[done..at]);
                done = at;
            }
            let f = start + at as u64;
            match e.kind {
                SlotEventKind::NoteOn { key, velocity } => self.bank.note_on(key, velocity, f),
                SlotEventKind::NoteOff { key } => self.bank.note_off(key, f),
                SlotEventKind::PitchBend { semitones } => self.bank.set_pitch_bend(semitones, f),
                SlotEventKind::Modulation { depth } => self.bank.set_modulation(depth, f),
            }
        }
        if done < len {
            self.bank.render(start + done as u64, &mut ol[done..], &mut or[done..]);
        }
    }

    fn set_param(&mut self, cx: &UnitCx, param: UnitParam, value: f64) -> Option<f64> {
        if !value.is_finite() {
            return None;
        }
        match param {
            UnitParam::Instrument => {
                let k = (value + 0.5).floor();
                if !(0.0..SYNTH_IDS.len() as f64).contains(&k) {
                    return None;
                }
                let instrument = Instrument::from_id(SYNTH_IDS[k as usize])?;
                self.bank.select(Some(instrument), cx.dsp_frame.max(0) as u64);
                Some(k)
            }
            UnitParam::Gain(instrument) => {
                let gain = value.clamp(0.0, SYNTH_GAIN_MAX);
                self.bank.set_gain(instrument, gain);
                Some(gain)
            }
            UnitParam::Send(_) => None,
        }
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}
