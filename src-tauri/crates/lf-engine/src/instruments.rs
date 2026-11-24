//! OWNS: the six built-in instruments as one bank (`Bank`: which one plays, or none, each one's
//! level, the performance wheels), and today's [`Instruments`] around it: the bank the
//! `NoteTarget::Builtin` path plays, the input OTT on its sum and its record path. Ported from
//! `synths/index.ts` and the synth side of `input-router.ts`; sustain and per-source note ownership stay
//! with the sender, native MIDI's one note router (`engine_io::midi::router` in the app crate). The SYNTH
//! chain unit ([`crate::units::SynthUnit`], `docs/unit-contract.md` §4) holds a bank of its own: two
//! synth slots are two banks, and [`Instruments`] keeps serving `NoteTarget::Builtin` until a later
//! slice retires it.
//!
//! All six are built in `Bank::new` and render every block, so a switch never allocates and a
//! released note rings out after one, as each slot's synth does in the web app when the player
//! switches slots. (Picking another synth for the same slot disposed the web synth and cut its tail;
//! here it rings out.) Notes go to the selected one, and nowhere while none is; a switch releases the
//! held notes and hands the instrument the wheels, also when it picks the instrument already selected.
//! A note outside 0..127 does nothing, as the web router drops it. Each instrument has its own level,
//! smoothed as a plugin slot's gain, on what is heard and what is recorded, selected or not.
//!
//! They run on the DSP clock of `effects` (the device frame less the frames the device skipped): the
//! bank's methods take DSP frames; [`Instruments`]' take device frames and [`Instruments::set_offset`]
//! keeps the difference (a unit gets its DSP frame as [`crate::UnitCx::dsp_frame`]).
//!
//! The instruments are heard on the master bus and recorded, as on the web's `looperInputBus`. A note
//! sounds when it is played, and the player played it when the click reached their ears: the output
//! latency (and the limiter's pre-delay) after the click's frame. The guitar reaches the input
//! `input_frames` + the plugin's latency later still, and a take's window starts where the guitar's
//! downbeat lands. So the record path delays the instruments by that difference, and a note played on
//! the heard click lands on the grid like a guitar note (up to its entry at the next block start).
//!
//! A note on or off is scheduled [`LEAD`] frames after the frame it is applied on and sounds exactly
//! there; the record path takes the lead off its delay (a SYNTH unit reports it as its latency, and its slot's
//! source record line takes it off the same way: `slots`). The web router scheduled 5 ms ahead
//! (`SCHEDULE_AHEAD`), and the ported synths rely on a look-ahead: a note at the current context time
//! would start on the next quantum boundary, and two starts of one voice inside a quantum would collide
//! (`dsp::synth::poly`). One quantum is the shortest lead that holds. When the input side plus the
//! plugin's latency is under it, a recorded note lands the difference late.
//!
//! The capture is stereo: the record path carries the instruments' two sides as the bus hears them (the
//! mono capture summed them at half level, which a centred instrument still reads bit for bit; the drum
//! kit's stereo is recorded now, a deliberate change of E1, `docs/unit-contract.md` §7).
//!
//! The input OTT (`dsp::ott`, one stereo instance, its detectors linked) sits on the instruments' sum
//! after every level and before the record path takes it, so what is heard on the bus and what is
//! recorded are the same processed signal. It always runs (off and settled, bit-transparent) and takes
//! the engine's one OTT setting on the frame the plugin slots' instances do ([`Instruments::set_ott`]).

use std::sync::Arc;

use crate::api::{max_chain_latency, Instrument, Pair};
use crate::dsp::buffer_source::AudioBuffer;
use crate::dsp::ott::{Setting, StereoOtt};
use crate::dsp::param::QUANTUM;
use crate::dsp::rng::Mulberry32;
use crate::dsp::synth::{Bass, DrumKit, PolyKind, PolySynth};
use crate::grid::Frame;

/// The seed of the drum kit's `Math.random` draws. Fixed, so a render repeats; the reference fixtures
/// pin their own.
const DRUM_SEED: u32 = 1;
/// Frames between a note on or off being applied and sounding: one render quantum.
pub const LEAD: Frame = QUANTUM as Frame;
/// Level smoothing, as a plugin slot's gain.
const GAIN_TAU_SECONDS: f64 = 0.012;

/// An instrument's level, smoothed toward its target frame by frame.
#[derive(Clone, Copy)]
struct Level {
    target: f64,
    gain: f64,
}

impl Level {
    /// Untouched at a steady unity level, so a default render stays bit for bit what it was.
    fn unity(&self) -> bool {
        self.gain == 1.0 && self.target == 1.0
    }

    fn step(&mut self, coef: f64) -> f64 {
        self.gain = crate::glide(self.gain, self.target, coef);
        self.gain
    }

    fn apply(&mut self, coef: f64, x: &mut [f32]) {
        if self.unity() {
            return;
        }
        for s in x {
            *s = (self.step(coef) * *s as f64) as f32;
        }
    }

    fn apply_stereo(&mut self, coef: f64, left: &mut [f32], right: &mut [f32]) {
        if self.unity() {
            return;
        }
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let g = self.step(coef);
            *l = (g * *l as f64) as f32;
            *r = (g * *r as f64) as f32;
        }
    }
}

/// The six instruments, the picked one (or none), each one's level and the wheels: today's bank, which
/// [`Instruments`] and the SYNTH unit ([`crate::units::SynthUnit`]) both hold. Its methods take frames on
/// the DSP clock (the device frame less the frames the device skipped); a note on or off is scheduled
/// [`LEAD`] after its frame, a pick, a release of every note and a wheel apply on it.
pub(crate) struct Bank {
    sample_rate: f32,
    poly: Vec<PolySynth>,
    /// Boxed, as the chains are: its render buffers are large.
    bass: Box<Bass>,
    drums: DrumKit,
    selected: Option<Instrument>,
    /// Each instrument's level, by `Instrument as usize`.
    levels: [Level; 6],
    gain_coef: f64,
    bend: f64,
    modulation: f64,
    /// One instrument's mono render over one quantum at most ([`Bank::render`] splits there).
    mono: [f32; QUANTUM],
}

impl Bank {
    /// Allocates: build it off the audio thread. `white` and `pink` are Tone's noise tables
    /// (`dsp::noise`).
    pub(crate) fn new(sample_rate: u32, white: &Arc<AudioBuffer>, pink: &Arc<AudioBuffer>) -> Self {
        let sr = sample_rate as f32;
        Bank {
            sample_rate: sr,
            poly: [PolyKind::Lead, PolyKind::Pad, PolyKind::Piano, PolyKind::Organ].into_iter().map(|kind| PolySynth::new(kind, sr, 0.0, 0)).collect(),
            bass: Box::new(Bass::new(sr, 0.0, 0)),
            drums: DrumKit::new(sr, white, pink, Mulberry32::new(DRUM_SEED), 0.0, 0),
            selected: None,
            levels: [Level { target: 1.0, gain: 1.0 }; 6],
            gain_coef: (-1.0 / (GAIN_TAU_SECONDS * sample_rate as f64)).exp(),
            bend: 0.0,
            modulation: 0.0,
            mono: [0.0; QUANTUM],
        }
    }

    pub(crate) fn selected(&self) -> Option<Instrument> {
        self.selected
    }

    /// `instrument`'s level (linear, 0.., already checked): it glides there from the next frame rendered.
    pub(crate) fn set_gain(&mut self, instrument: Instrument, gain: f64) {
        self.levels[instrument as usize].target = gain;
    }

    /// DSP frame `f`'s time in seconds.
    fn time(&self, f: u64) -> f64 {
        f as f64 / self.sample_rate as f64
    }

    /// The pick: the held notes released on `f`, then `instrument` takes the notes and the wheels, also
    /// when it is the one picked already.
    pub(crate) fn select(&mut self, instrument: Option<Instrument>, f: u64) {
        self.all_notes_off(f);
        self.selected = instrument;
        let (bend, modulation) = (self.bend, self.modulation);
        self.wheels(bend, modulation, f);
    }

    pub(crate) fn note_on(&mut self, note: u8, velocity: f32, f: u64) {
        if note > 127 {
            return;
        }
        let t = self.time(f + LEAD as u64);
        let v = if velocity.is_finite() { velocity.clamp(0.0, 1.0) as f64 } else { 0.0 };
        match self.selected {
            Some(Instrument::Bass) => self.bass.note_on(note, v, t, f),
            Some(Instrument::Drums) => self.drums.note_on(note, v, t, f),
            Some(i) => self.poly[i as usize].note_on(note, v, t, f),
            None => {}
        }
    }

    pub(crate) fn note_off(&mut self, note: u8, f: u64) {
        if note > 127 {
            return;
        }
        let t = self.time(f + LEAD as u64);
        match self.selected {
            Some(Instrument::Bass) => self.bass.note_off(note, t, f),
            Some(Instrument::Drums) => self.drums.note_off(note, t, f),
            Some(i) => self.poly[i as usize].note_off(note, t, f),
            None => {}
        }
    }

    pub(crate) fn all_notes_off(&mut self, f: u64) {
        match self.selected {
            Some(Instrument::Bass) => self.bass.all_notes_off(f),
            Some(Instrument::Drums) | None => {}
            Some(i) => self.poly[i as usize].all_notes_off(f),
        }
    }

    pub(crate) fn set_pitch_bend(&mut self, semitones: f64, f: u64) {
        if semitones.is_finite() {
            self.wheels(semitones, self.modulation, f);
        }
    }

    pub(crate) fn set_modulation(&mut self, depth: f64, f: u64) {
        if depth.is_finite() {
            self.wheels(self.bend, depth.clamp(0.0, 1.0), f);
        }
    }

    /// Keep the wheels and apply them to the selected instrument (the drum kit has none).
    fn wheels(&mut self, bend: f64, modulation: f64, f: u64) {
        self.bend = bend;
        self.modulation = modulation;
        let t = self.time(f);
        match self.selected {
            Some(Instrument::Bass) => {
                self.bass.set_pitch_bend(bend, t, f);
                self.bass.set_modulation(modulation, t, f);
            }
            Some(Instrument::Drums) | None => {}
            Some(i) => {
                let synth = &mut self.poly[i as usize];
                synth.set_pitch_bend(bend, t, f);
                synth.set_modulation(modulation, t, f);
            }
        }
    }

    /// Render DSP frames `f..f + left.len()` of all six, each at its level, into `left`/`right`
    /// (overwritten), a quantum at most per pass (the drum kit's stereo, then lead, pad, piano, organ and
    /// bass on both sides, in that order, frame by frame).
    pub(crate) fn render(&mut self, f: u64, left: &mut [f32], right: &mut [f32]) {
        let (n, q) = (left.len(), QUANTUM as u64);
        let mut done = 0;
        while done < n {
            let at = f + done as u64;
            let k = ((q - at % q) as usize).min(n - done);
            self.render_quantum(at, &mut left[done..done + k], &mut right[done..done + k]);
            done += k;
        }
    }

    fn render_quantum(&mut self, f: u64, left: &mut [f32], right: &mut [f32]) {
        let n = left.len();
        let coef = self.gain_coef;
        self.drums.render(f, left, right);
        self.levels[Instrument::Drums as usize].apply_stereo(coef, left, right);
        let mono = &mut self.mono[..n];
        // `poly` holds Lead, Pad, Piano and Organ: the first four levels.
        for (synth, level) in self.poly.iter_mut().zip(self.levels.iter_mut()) {
            synth.render(f, mono);
            level.apply(coef, mono);
            for ((l, r), &x) in left.iter_mut().zip(right.iter_mut()).zip(mono.iter()) {
                *l += x;
                *r += x;
            }
        }
        self.bass.render(f, mono);
        self.levels[Instrument::Bass as usize].apply(coef, mono);
        for ((l, r), &x) in left.iter_mut().zip(right.iter_mut()).zip(mono.iter()) {
            *l += x;
            *r += x;
        }
    }
}

pub struct Instruments {
    bank: Bank,
    /// Device frames the DSP clock is behind.
    offset: Frame,
    /// The record path's delay line: the instruments' stereo sum, `delay` frames late.
    line: Vec<Pair>,
    write: usize,
    /// The input OTT on the sum, after the levels.
    ott: StereoOtt,
}

impl Instruments {
    /// Allocates: build it off the audio thread. `max_block` bounds a render, `max_input` the device's
    /// input side (the record line holds it plus [`max_chain_latency`] plus a block); `white` and `pink`
    /// are Tone's noise tables (`dsp::noise`).
    pub fn new(sample_rate: u32, max_block: usize, max_input: Frame, white: &Arc<AudioBuffer>, pink: &Arc<AudioBuffer>) -> Self {
        Instruments {
            bank: Bank::new(sample_rate, white, pink),
            offset: 0,
            line: vec![[0.0; 2]; (max_input.max(0) + max_chain_latency(sample_rate)) as usize + max_block],
            write: 0,
            ott: StereoOtt::new(sample_rate),
        }
    }

    pub fn selected(&self) -> Option<Instrument> {
        self.bank.selected()
    }

    /// `instrument`'s level (linear, 0..): it glides there from the frame it is set.
    pub fn set_gain(&mut self, instrument: Instrument, gain: f32) {
        self.bank.set_gain(instrument, if gain.is_finite() { gain.max(0.0) as f64 } else { 0.0 });
    }

    /// The input OTT's setting, from the next frame rendered.
    pub fn set_ott(&mut self, setting: Setting) {
        self.ott.set(setting);
    }

    pub fn set_offset(&mut self, offset: Frame) {
        self.offset = offset;
    }

    /// A device frame on the DSP clock.
    fn at(&self, frame: Frame) -> u64 {
        (frame - self.offset) as u64
    }

    pub fn select(&mut self, instrument: Option<Instrument>, frame: Frame) {
        self.bank.select(instrument, self.at(frame));
    }

    pub fn note_on(&mut self, note: u8, velocity: f32, frame: Frame) {
        self.bank.note_on(note, velocity, self.at(frame));
    }

    pub fn note_off(&mut self, note: u8, frame: Frame) {
        self.bank.note_off(note, self.at(frame));
    }

    pub fn all_notes_off(&mut self, frame: Frame) {
        self.bank.all_notes_off(self.at(frame));
    }

    pub fn set_pitch_bend(&mut self, semitones: f64, frame: Frame) {
        self.bank.set_pitch_bend(semitones, self.at(frame));
    }

    pub fn set_modulation(&mut self, depth: f64, frame: Frame) {
        self.bank.set_modulation(depth, self.at(frame));
    }

    /// Render frames `frame..frame + left.len()` of all six, each at its level, then the input OTT, into
    /// `left`/`right` (overwritten), and the record path's frames into `record` (added): the stereo sum
    /// `delay` less [`LEAD`] frames late.
    pub fn render(&mut self, frame: Frame, delay: Frame, left: &mut [f32], right: &mut [f32], record: &mut [Pair]) {
        self.bank.render(self.at(frame), left, right);
        self.ott.process(left, right);
        let len = self.line.len();
        let delay = (delay - LEAD).clamp(0, (len - 1) as Frame) as usize;
        for ((&l, &r), rec) in left.iter().zip(right.iter()).zip(record.iter_mut()) {
            self.line[self.write] = [l, r];
            let [a, b] = self.line[(self.write + len - delay) % len];
            rec[0] += a;
            rec[1] += b;
            self.write = (self.write + 1) % len;
        }
    }
}
