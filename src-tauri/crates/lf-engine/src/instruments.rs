//! OWNS: the six built-in instruments, which one plays, the performance wheels, and the instruments'
//! record path. Ported from `src/audio/synths/index.ts` and the synth side of
//! `src/audio/input-router.ts`; the router's sustain and its per-source note ownership stay with the
//! sender.
//!
//! All six are built in [`Instruments::new`] and render every block, so a switch never allocates and a
//! released note rings out after one, as each slot's synth does in the web app when the player
//! switches slots. (Picking another synth for the same slot disposed the web synth and cut its tail;
//! here it rings out.) Notes go to the selected one; a switch releases the held notes and hands the
//! instrument the wheels, also when it picks the instrument already selected. A note outside 0..127
//! does nothing, as the web router drops it.
//!
//! They run on the DSP clock of `effects` (the device frame less the frames the device skipped);
//! methods take device frames and [`Instruments::set_offset`] keeps the difference.
//!
//! The instruments are heard on the master bus and recorded, as on the web's `looperInputBus`. A note
//! sounds when it is played, and the player played it when the click reached their ears: the output
//! latency (and the limiter's pre-delay) after the click's frame. The guitar reaches the input
//! `input_frames` + the plugin's latency later still, and a take's window starts where the guitar's
//! downbeat lands. So the record path delays the instruments by that difference, and a note played on
//! the heard click lands on the grid like a guitar note (up to its entry at the next block start).
//!
//! A note on or off is scheduled [`LEAD`] frames after the frame it is applied on and sounds exactly
//! there; the record path takes the lead off its delay. The web router scheduled 5 ms ahead
//! (`SCHEDULE_AHEAD`), and the ported synths rely on a look-ahead: a note at the current context time
//! would start on the next quantum boundary, and two starts of one voice inside a quantum would collide
//! (`dsp::synth::poly`). One quantum is the shortest lead that holds. When the input side plus the
//! plugin's latency is under it, a recorded note lands the difference late.
//!
//! The capture is mono: the stereo instruments are summed at half level (a Web Audio speakers downmix
//! into the one-channel capture).

use std::sync::Arc;

use crate::api::Instrument;
use crate::dsp::buffer_source::AudioBuffer;
use crate::dsp::param::QUANTUM;
use crate::dsp::rng::Mulberry32;
use crate::dsp::synth::{Bass, DrumKit, PolyKind, PolySynth};
use crate::grid::Frame;

/// The seed of the drum kit's `Math.random` draws. Fixed, so a render repeats; the reference fixtures
/// pin their own.
const DRUM_SEED: u32 = 1;
/// Frames between a note on or off being applied and sounding: one render quantum.
pub const LEAD: Frame = QUANTUM as Frame;
/// The longest record-path delay: a second of input and plugin latency.
const MAX_RECORD_DELAY_SECONDS: usize = 1;

pub struct Instruments {
    sample_rate: f32,
    poly: Vec<PolySynth>,
    /// Boxed, as the chains are: its render buffers are large.
    bass: Box<Bass>,
    drums: DrumKit,
    selected: Option<Instrument>,
    /// Device frames the DSP clock is behind.
    offset: Frame,
    bend: f64,
    modulation: f64,
    mono: Vec<f32>,
    /// The record path's delay line: the instruments' mono sum, `delay` frames late.
    line: Vec<f32>,
    write: usize,
}

impl Instruments {
    /// Allocates: build it off the audio thread. `max_block` bounds a render; `white` and `pink` are
    /// Tone's noise tables (`dsp::noise`).
    pub fn new(sample_rate: u32, max_block: usize, white: &Arc<AudioBuffer>, pink: &Arc<AudioBuffer>) -> Self {
        let sr = sample_rate as f32;
        Instruments {
            sample_rate: sr,
            poly: [PolyKind::Lead, PolyKind::Pad, PolyKind::Piano, PolyKind::Organ].into_iter().map(|kind| PolySynth::new(kind, sr, 0.0, 0)).collect(),
            bass: Box::new(Bass::new(sr, 0.0, 0)),
            drums: DrumKit::new(sr, white, pink, Mulberry32::new(DRUM_SEED), 0.0, 0),
            selected: None,
            offset: 0,
            bend: 0.0,
            modulation: 0.0,
            mono: vec![0.0; max_block],
            line: vec![0.0; sample_rate as usize * MAX_RECORD_DELAY_SECONDS + max_block],
            write: 0,
        }
    }

    pub fn selected(&self) -> Option<Instrument> {
        self.selected
    }

    pub fn set_offset(&mut self, offset: Frame) {
        self.offset = offset;
    }

    /// A device frame on the DSP clock, and its time in seconds.
    fn at(&self, frame: Frame) -> (f64, u64) {
        let f = (frame - self.offset) as u64;
        (f as f64 / self.sample_rate as f64, f)
    }

    /// A note's time, [`LEAD`] after `frame`, and the frame being rendered.
    fn ahead(&self, frame: Frame) -> (f64, u64) {
        let (_, f) = self.at(frame);
        ((f + LEAD as u64) as f64 / self.sample_rate as f64, f)
    }

    pub fn select(&mut self, instrument: Option<Instrument>, frame: Frame) {
        self.all_notes_off(frame);
        self.selected = instrument;
        let (bend, modulation) = (self.bend, self.modulation);
        self.wheels(bend, modulation, frame);
    }

    pub fn note_on(&mut self, note: u8, velocity: f32, frame: Frame) {
        if note > 127 {
            return;
        }
        let (t, f) = self.ahead(frame);
        let v = if velocity.is_finite() { velocity.clamp(0.0, 1.0) as f64 } else { 0.0 };
        match self.selected {
            Some(Instrument::Bass) => self.bass.note_on(note, v, t, f),
            Some(Instrument::Drums) => self.drums.note_on(note, v, t, f),
            Some(i) => self.poly[i as usize].note_on(note, v, t, f),
            None => {}
        }
    }

    pub fn note_off(&mut self, note: u8, frame: Frame) {
        if note > 127 {
            return;
        }
        let (t, f) = self.ahead(frame);
        match self.selected {
            Some(Instrument::Bass) => self.bass.note_off(note, t, f),
            Some(Instrument::Drums) => self.drums.note_off(note, t, f),
            Some(i) => self.poly[i as usize].note_off(note, t, f),
            None => {}
        }
    }

    pub fn all_notes_off(&mut self, frame: Frame) {
        let f = self.at(frame).1;
        match self.selected {
            Some(Instrument::Bass) => self.bass.all_notes_off(f),
            Some(Instrument::Drums) | None => {}
            Some(i) => self.poly[i as usize].all_notes_off(f),
        }
    }

    pub fn set_pitch_bend(&mut self, semitones: f64, frame: Frame) {
        if semitones.is_finite() {
            self.wheels(semitones, self.modulation, frame);
        }
    }

    pub fn set_modulation(&mut self, depth: f64, frame: Frame) {
        if depth.is_finite() {
            self.wheels(self.bend, depth.clamp(0.0, 1.0), frame);
        }
    }

    /// Keep the wheels and apply them to the selected instrument (the drum kit has none).
    fn wheels(&mut self, bend: f64, modulation: f64, frame: Frame) {
        self.bend = bend;
        self.modulation = modulation;
        let (t, f) = self.at(frame);
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

    /// Render frames `frame..frame + left.len()` of all six into `left`/`right` (overwritten), and the
    /// record path's frames into `record` (added): the mono sum `delay` less [`LEAD`] frames late.
    pub fn render(&mut self, frame: Frame, delay: Frame, left: &mut [f32], right: &mut [f32], record: &mut [f32]) {
        let n = left.len();
        let f = self.at(frame).1;
        self.drums.render(f, left, right);
        let mono = &mut self.mono[..n];
        for synth in self.poly.iter_mut() {
            synth.render(f, mono);
            for ((l, r), &x) in left.iter_mut().zip(right.iter_mut()).zip(mono.iter()) {
                *l += x;
                *r += x;
            }
        }
        self.bass.render(f, mono);
        for ((l, r), &x) in left.iter_mut().zip(right.iter_mut()).zip(mono.iter()) {
            *l += x;
            *r += x;
        }
        let len = self.line.len();
        let delay = (delay - LEAD).clamp(0, (len - 1) as Frame) as usize;
        for ((&l, &r), rec) in left.iter().zip(right.iter()).zip(record.iter_mut()) {
            self.line[self.write] = 0.5 * (l + r);
            *rec += self.line[(self.write + len - delay) % len];
            self.write = (self.write + 1) % len;
        }
    }
}
