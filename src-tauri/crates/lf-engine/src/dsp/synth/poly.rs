//! The app's poly synths: `src/audio/synths/index.ts` (the POLY_VOICES specs, `makePolySynth`) and
//! `src/audio/synths/modulation.ts` (pitch bend and the mod-wheel vibrato), on the Tone voices of
//! [`super::voice`].
//!
//! A poly synth is a fixed pool of voices. A note-on takes the voice already holding that note, else
//! the released voice that attacked longest ago, else the held one that did (a steal); its attack is
//! nudged one sample past the voice's previous attack, so Tone never restarts a source at its last
//! start time. A note-off releases the voice holding the note, never before its attack; a stale
//! note-off (the voice was stolen) does nothing. Pitch bend sets every voice's detune (cents =
//! semitones × 100). The mod wheel sets the vibrato depth (× 0.35) and crossfades between the dry path
//! and the vibrato over 10 ms, on a 0 ↔ >0 change only.
//!
//! Every call takes `frame`, the frame being rendered when it is made; Blink's context time (and
//! Tone's `currentTime`) is then [`param::context_frame`]`(frame)`, and a note or ramp scheduled
//! before it starts there. A live note at frame `f` passed as `time = f / rate` therefore starts on
//! the next quantum boundary at or after `f`, as it would in a live AudioContext with no look-ahead;
//! an engine that wants the exact frame schedules it at least one quantum ahead (`time = (f + lead) /
//! rate`, `lead >= 127`) and keeps passing the frame being rendered. Either way the same calls at the
//! same frames render the same bits at any block size: [`PolySynth::render`] computes whole quanta on
//! absolute 128-frame boundaries and hands out slices of them.

use std::sync::Arc;

use super::vibrato::{Lfo, Vibrato};
use super::voice::{Modulation, ModulationOptions, ModulationSynth, Synth, Voice};
use crate::dsp::envelope::Adsr;
use crate::dsp::fdlibm;
use crate::dsp::gain::GainNode;
use crate::dsp::oscillator::{OscillatorType, PeriodicWave};
use crate::dsp::param::{self, Units, QUANTUM};

const Q: usize = QUANTUM;

/// Full mod wheel: vibrato depth (`MOD_WHEEL_MAX_DEPTH`).
const MOD_WHEEL_MAX_DEPTH: f64 = 0.35;
/// `VIBRATO_RATE_HZ`.
const VIBRATO_RATE_HZ: f64 = 5.5;
/// The dry ↔ vibrato crossfade (`BYPASS_CROSSFADE_S`).
const BYPASS_CROSSFADE_S: f64 = 0.01;
/// Tone's Vibrato default `maxDelay`.
const VIBRATO_MAX_DELAY: f64 = 0.005;

/// The four built-in poly voices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolyKind {
    Lead,
    Pad,
    Piano,
    Organ,
}

impl PolyKind {
    /// The registry id (`SYNTHS` in `src/audio/synths/index.ts`).
    pub fn from_id(id: &str) -> Option<PolyKind> {
        match id {
            "lead" => Some(PolyKind::Lead),
            "pad" => Some(PolyKind::Pad),
            "piano" => Some(PolyKind::Piano),
            "organ" => Some(PolyKind::Organ),
            _ => None,
        }
    }

    pub fn spec(self) -> &'static PolyVoiceSpec {
        match self {
            PolyKind::Lead => &POLY_VOICES[0],
            PolyKind::Pad => &POLY_VOICES[1],
            PolyKind::Piano => &POLY_VOICES[2],
            PolyKind::Organ => &POLY_VOICES[3],
        }
    }
}

/// What a voice is built from.
#[derive(Clone, Copy, Debug)]
pub enum VoiceSpec {
    /// `new Synth({ oscillator: { type }, envelope })`.
    Synth { oscillator: OscillatorType, envelope: Adsr },
    /// `new AMSynth(...)` / `new FMSynth(...)`.
    Modulation { oscillator: OscillatorType, modulation_type: OscillatorType, options: ModulationOptions },
}

/// One row of POLY_VOICES.
#[derive(Clone, Copy, Debug)]
pub struct PolyVoiceSpec {
    pub id: &'static str,
    pub max_polyphony: usize,
    /// The gain-staging trim; `None` is Tone's 0 dB default.
    pub volume_db: Option<f64>,
    pub voice: VoiceSpec,
}

/// `POLY_VOICES` in `src/audio/synths/index.ts` (its comments give the trims' reasons).
pub const POLY_VOICES: [PolyVoiceSpec; 4] = [
    PolyVoiceSpec {
        id: "lead",
        max_polyphony: 8,
        volume_db: Some(-9.0),
        voice: VoiceSpec::Synth { oscillator: OscillatorType::Sawtooth, envelope: Adsr::new(0.008, 0.18, 0.55, 0.35) },
    },
    PolyVoiceSpec {
        id: "pad",
        max_polyphony: 12,
        volume_db: Some(-4.0),
        voice: VoiceSpec::Modulation {
            oscillator: OscillatorType::Sine,
            modulation_type: OscillatorType::Sine,
            options: ModulationOptions {
                modulation: Modulation::Fm { modulation_index: 2.0 },
                harmonicity: 1.5,
                envelope: Adsr::new(0.8, 0.4, 0.9, 2.0),
                modulation_envelope: Adsr::new(0.6, 0.3, 0.7, 1.8),
            },
        },
    },
    PolyVoiceSpec {
        id: "piano",
        max_polyphony: 12,
        volume_db: Some(-10.0),
        voice: VoiceSpec::Synth { oscillator: OscillatorType::Triangle, envelope: Adsr::new(0.004, 0.6, 0.12, 0.8) },
    },
    PolyVoiceSpec {
        id: "organ",
        max_polyphony: 8,
        volume_db: None,
        voice: VoiceSpec::Modulation {
            oscillator: OscillatorType::Sine,
            modulation_type: OscillatorType::Square,
            options: ModulationOptions {
                modulation: Modulation::Am,
                harmonicity: 1.0,
                envelope: Adsr::new(0.01, 0.0, 1.0, 0.06),
                modulation_envelope: Adsr::new(0.01, 0.0, 1.0, 0.06),
            },
        },
    },
];

/// `midiToFreq` (`src/audio/types.ts`).
pub fn midi_to_freq(note: u8) -> f64 {
    440.0 * fdlibm::pow(2.0, (note as f64 - 69.0) / 12.0)
}

struct Entry {
    voice: Voice,
    note: Option<u8>,
    order: u64,
    attack_time: f64,
}

/// `createModulation`: the voices' sum splits into a dry gain and the vibrato's wet gain.
struct ModulationBus {
    dry: GainNode,
    wet: GainNode,
    vibrato: Vibrato,
    /// Which path is live.
    is_wet: bool,
    dry_out: [[f32; Q]; 1],
    wet_out: [[f32; Q]; 1],
}

impl ModulationBus {
    fn new(sample_rate: f32, lfo_wave_rate: f32, now: f64, frame: u64) -> Self {
        let rate = sample_rate as f64;
        let wave = Arc::new(Lfo::sine_wave(-90.0, lfo_wave_rate));
        ModulationBus {
            dry: GainNode::new(rate, 1.0, Units::Gain, frame),
            wet: GainNode::new(rate, 0.0, Units::Gain, frame),
            vibrato: Vibrato::new(wave, VIBRATO_RATE_HZ, 0.0, VIBRATO_MAX_DELAY, now, sample_rate, frame),
            is_wet: false,
            dry_out: [[0.0; Q]],
            wet_out: [[0.0; Q]],
        }
    }

    fn set_modulation(&mut self, depth: f64, now: f64, frame: u64) {
        let d = depth.clamp(0.0, 1.0);
        self.vibrato.depth().gain.set_value(d * MOD_WHEEL_MAX_DEPTH, now, frame);
        let want_wet = d > 0.0;
        if want_wet != self.is_wet {
            self.is_wet = want_wet;
            self.wet.gain.ramp_to(if want_wet { 1.0 } else { 0.0 }, BYPASS_CROSSFADE_S, now, frame);
            self.dry.gain.ramp_to(if want_wet { 0.0 } else { 1.0 }, BYPASS_CROSSFADE_S, now, frame);
        }
    }

    fn process(&mut self, q: u64, input: Option<&[f32; Q]>, out: &mut [f32; Q]) {
        let dry_silent = self.dry.process(q, input.map(std::slice::from_ref), &mut self.dry_out);
        let vibrato = self.vibrato.process(q, input);
        let wet_silent = self.wet.process(q, Some(std::slice::from_ref(vibrato)), &mut self.wet_out);
        match (dry_silent, wet_silent) {
            (true, true) => out.fill(0.0),
            (false, true) => out.copy_from_slice(&self.dry_out[0]),
            (true, false) => out.copy_from_slice(&self.wet_out[0]),
            (false, false) => {
                for ((o, &d), &w) in out.iter_mut().zip(&self.dry_out[0]).zip(&self.wet_out[0]) {
                    *o = d + w;
                }
            }
        }
    }
}

/// `makePolySynth(spec)`: the voice pool, note dispatch, bend and mod wheel. Mono output.
pub struct PolySynth {
    kind: PolyKind,
    sample_rate: f32,
    voices: Vec<Entry>,
    serial: u64,
    modulation: ModulationBus,
    sum: [f32; Q],
    out: [f32; Q],
    /// The quantum `out` holds.
    rendered: Option<u64>,
}

impl PolySynth {
    /// Build the synth (allocates: wave tables and every voice). `now` is Tone's time and `frame`
    /// the frame being rendered when it is built.
    pub fn new(kind: PolyKind, sample_rate: f32, now: f64, frame: u64) -> Self {
        PolySynth::with_lfo_wave_rate(kind, sample_rate, sample_rate, now, frame)
    }

    /// [`PolySynth::new`] with the vibrato's LFO wave built at `lfo_wave_rate`: Tone reuses the wave
    /// the first context of the page built ([`Lfo::sine_wave`]), so a context at another rate runs its
    /// vibrato at `5.5 Hz * rate / lfo_wave_rate`. The references' 44.1 k scenarios ran after the
    /// 48 k ones.
    pub fn with_lfo_wave_rate(kind: PolyKind, sample_rate: f32, lfo_wave_rate: f32, now: f64, frame: u64) -> Self {
        let spec = kind.spec();
        let wave = |t: OscillatorType| Arc::new(PeriodicWave::basic(t, sample_rate));
        let voices = match spec.voice {
            VoiceSpec::Synth { oscillator, envelope } => {
                let w = wave(oscillator);
                (0..spec.max_polyphony).map(|_| Voice::Synth(Box::new(Synth::new(Arc::clone(&w), envelope, 0.0, sample_rate, frame)))).collect::<Vec<_>>()
            }
            VoiceSpec::Modulation { oscillator, modulation_type, options } => {
                let carrier = wave(oscillator);
                let modulator = if modulation_type == oscillator { Arc::clone(&carrier) } else { wave(modulation_type) };
                (0..spec.max_polyphony)
                    .map(|_| Voice::Modulation(Box::new(ModulationSynth::new(Arc::clone(&carrier), Arc::clone(&modulator), options, 0.0, sample_rate, frame))))
                    .collect()
            }
        };
        let voices = voices
            .into_iter()
            .map(|mut voice| {
                if let Some(db) = spec.volume_db {
                    voice.volume().gain.set_value(db, now, frame);
                }
                Entry { voice, note: None, order: 0, attack_time: f64::NEG_INFINITY }
            })
            .collect();
        PolySynth {
            kind,
            sample_rate,
            voices,
            serial: 0,
            modulation: ModulationBus::new(sample_rate, lfo_wave_rate, now, frame),
            sum: [0.0; Q],
            out: [0.0; Q],
            rendered: None,
        }
    }

    pub fn kind(&self) -> PolyKind {
        self.kind
    }

    /// `noteOn(note, velocity, time)`; velocity 0..1.
    pub fn note_on(&mut self, note: u8, velocity: f64, time: f64, frame: u64) {
        let i = match self.voices.iter().position(|e| e.note == Some(note)) {
            Some(i) => i,
            None => {
                let any_free = self.voices.iter().any(|e| e.note.is_none());
                // `reduce((a, b) => a.order <= b.order ? a : b)`: the first of the lowest order.
                let mut best: Option<usize> = None;
                for (i, e) in self.voices.iter().enumerate() {
                    if any_free && e.note.is_some() {
                        continue;
                    }
                    if best.is_none_or(|b| self.voices[b].order > e.order) {
                        best = Some(i);
                    }
                }
                best.expect("a voice")
            }
        };
        self.serial += 1;
        let sample_rate = self.sample_rate as f64;
        let entry = &mut self.voices[i];
        entry.note = Some(note);
        entry.order = self.serial;
        // Tone forbids restarting a source at its last start time: a reused voice moves one sample on.
        let when = time.max(entry.attack_time + 1.0 / sample_rate);
        entry.attack_time = when;
        entry.voice.trigger_attack(midi_to_freq(note), when, velocity, frame);
    }

    /// `noteOff(note, time)`.
    pub fn note_off(&mut self, note: u8, time: f64, frame: u64) {
        // A stale release must not stop the note that stole this voice.
        let Some(entry) = self.voices.iter_mut().find(|e| e.note == Some(note)) else { return };
        entry.note = None;
        entry.voice.trigger_release(time.max(entry.attack_time), frame);
    }

    /// `allNotesOff()`: every voice releases 5 ms after the context time.
    pub fn all_notes_off(&mut self, frame: u64) {
        let immediate = param::context_frame(frame) as f64 / self.sample_rate as f64;
        for entry in self.voices.iter_mut() {
            entry.note = None;
            entry.voice.trigger_release((immediate + 0.005).max(entry.attack_time), frame);
        }
    }

    /// `setPitchBend(semitones)` at Tone time `now`: every voice's detune.
    pub fn set_pitch_bend(&mut self, semitones: f64, now: f64, frame: u64) {
        let cents = semitones * 100.0;
        for entry in self.voices.iter_mut() {
            entry.voice.detune().param.set_value(cents, now, frame);
        }
    }

    /// `setModulation(depth)` at Tone time `now`; depth 0..1.
    pub fn set_modulation(&mut self, depth: f64, now: f64, frame: u64) {
        self.modulation.set_modulation(depth, now, frame);
    }

    fn process(&mut self, q: u64) {
        let mut silent = true;
        for entry in self.voices.iter_mut() {
            if let Some(v) = entry.voice.process(q) {
                if silent {
                    self.sum.copy_from_slice(v);
                    silent = false;
                } else {
                    for (s, &x) in self.sum.iter_mut().zip(v) {
                        *s += x;
                    }
                }
            }
        }
        let input = (!silent).then_some(&self.sum);
        self.modulation.process(q, input, &mut self.out);
    }

    /// Render frames `frame..frame + out.len()`. Blocks must follow each other.
    pub fn render(&mut self, frame: u64, out: &mut [f32]) {
        let mut done = 0;
        while done < out.len() {
            let f = frame + done as u64;
            let q = f - f % Q as u64;
            if self.rendered != Some(q) {
                self.process(q);
                self.rendered = Some(q);
            }
            let at = (f - q) as usize;
            let n = (Q - at).min(out.len() - done);
            out[done..done + n].copy_from_slice(&self.out[at..at + n]);
            done += n;
        }
    }
}
