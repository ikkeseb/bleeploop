//! The bass: `src/audio/synths/bass.ts` on Tone.js 15.1.22's `instrument/MonoSynth.js` ([`MonoSynth`],
//! with the note handling of `Monophonic.js` and the output `Volume` of `Instrument.js`) and the app's
//! held-note stack and modulation ([`Bass`]).
//!
//! The MonoSynth graph, per quantum and in Blink's float arithmetic: `frequency` and `detune` signals
//! into a square oscillator, through a -24 dB lowpass [`Filter`] (two biquads) whose frequency is the
//! filter envelope's output (`connectSignal`: the Filter's own 800 Hz is overridden), times the
//! amplitude envelope, times the volume. The bass's output feeds the shared dry/vibrato bus of
//! `createModulation` (the poly synths' `ModulationBus`); pitch bend sets the oscillator's detune. `portamento` is 0
//! (the default) and not ported, as in [`super::voice`].
//!
//! The held-note stack (`held`, a Map in insertion order): a note-on moves its note to the top and
//! retriggers the voice; a note-off of the sounding note retriggers the next held note below it
//! (legato), or releases when none is held; a note-off of a held but not sounding note only drops it.
//! Every attack is nudged one sample past the previous one, so Tone never restarts its source at its
//! last start time. Time and frame rules are [`super::poly`]'s.

use std::sync::Arc;

use super::poly::{midi_to_freq, ModulationBus};
use crate::dsp::biquad::FilterType;
use crate::dsp::envelope::{Adsr, AmplitudeEnvelope, FrequencyEnvelope};
use crate::dsp::filter::{Filter, Rolloff};
use crate::dsp::gain::GainNode;
use crate::dsp::oscillator::{OscillatorType, PeriodicWave, ToneOscillator};
use crate::dsp::param::{self, Units, QUANTUM};
use crate::dsp::signal::Signal;

const Q: usize = QUANTUM;

/// `createBass`'s MonoSynth options.
const VOLUME_DB: f64 = -7.0;
const ENVELOPE: Adsr = Adsr::new(0.006, 0.22, 0.65, 0.4);
const FILTER_FREQUENCY: f64 = 800.0;
/// MonoSynth's default filter Q.
const FILTER_Q: f64 = 1.0;
const FILTER_ENVELOPE: Adsr = Adsr::new(0.01, 0.18, 0.3, 0.4);
const FILTER_BASE_FREQUENCY: f64 = 120.0;
const FILTER_OCTAVES: f64 = 3.5;
/// MonoSynth's default filter envelope exponent.
const FILTER_EXPONENT: f64 = 2.0;

/// Notes the held stack holds: every MIDI note.
const MAX_HELD: usize = 128;

/// Tone's `MonoSynth`.
pub struct MonoSynth {
    /// The OmniOscillator's frequency signal (Hz).
    pub frequency: Signal,
    /// The OmniOscillator's detune signal (cents).
    pub detune: Signal,
    pub oscillator: ToneOscillator,
    pub filter: Filter,
    pub filter_envelope: FrequencyEnvelope,
    pub envelope: AmplitudeEnvelope,
    /// The Instrument's output `Volume` (dB).
    pub volume: GainNode,
    osc_out: [f32; Q],
    filtered: [f32; Q],
    env_out: [f32; Q],
    out: [[f32; Q]; 1],
}

impl MonoSynth {
    /// `new MonoSynth({...})` with `createBass`'s options (allocates the square wave's tables).
    pub fn bass(sample_rate: f32, frame: u64) -> Self {
        let wave = Arc::new(PeriodicWave::basic(OscillatorType::Square, sample_rate));
        let mut filter = Filter::new(sample_rate, FilterType::Lowpass, FILTER_FREQUENCY, FILTER_Q, Rolloff::Db24, frame);
        // `filterEnvelope.connect(filter.frequency)`.
        filter.frequency.param.connect_signal(true, frame);
        MonoSynth {
            frequency: Signal::new(sample_rate, Units::Frequency, 440.0, frame),
            detune: Signal::new(sample_rate, Units::Cents, 0.0, frame),
            oscillator: ToneOscillator::new(wave, sample_rate, frame),
            filter,
            filter_envelope: FrequencyEnvelope::new(FILTER_ENVELOPE, FILTER_BASE_FREQUENCY, FILTER_OCTAVES, FILTER_EXPONENT, sample_rate, frame),
            envelope: AmplitudeEnvelope::new(ENVELOPE, sample_rate, frame),
            volume: GainNode::new(sample_rate as f64, VOLUME_DB, Units::Decibels, frame),
            osc_out: [0.0; Q],
            filtered: [0.0; Q],
            env_out: [0.0; Q],
            out: [[0.0; Q]],
        }
    }

    /// `triggerAttack(frequency, time, velocity)`: both envelopes, the oscillator, then the note.
    pub fn trigger_attack(&mut self, frequency: f64, time: f64, velocity: f64, frame: u64) {
        self.envelope.envelope.trigger_attack(time, velocity, frame);
        self.filter_envelope.envelope.trigger_attack(time, 1.0, frame);
        self.oscillator.start(time, frame);
        let adsr = self.envelope.envelope.adsr;
        if adsr.sustain == 0.0 {
            self.oscillator.stop(time + adsr.attack + adsr.decay, frame);
        }
        self.frequency.param.set_value_at_time(frequency, time, frame);
    }

    /// `triggerRelease(time)`.
    pub fn trigger_release(&mut self, time: f64, frame: u64) {
        self.envelope.envelope.trigger_release(time, frame);
        self.filter_envelope.envelope.trigger_release(time, frame);
        self.oscillator.stop(time + self.envelope.envelope.adsr.release, frame);
    }

    /// Render the quantum at `q`; `None` when the output is silent.
    pub fn process(&mut self, q: u64) -> Option<&[f32; Q]> {
        let f = self.frequency.process(q, None);
        let d = self.detune.process(q, None);
        // A silent oscillator renders zeros: the filter keeps ringing out on them.
        self.oscillator.process(q, f, d, &mut self.osc_out);
        let cutoff = self.filter_envelope.process(q);
        self.filter.begin_quantum_driven(q, Some(cutoff));
        self.filter.process(0, &self.osc_out, &mut self.filtered);
        let silent = self.envelope.process(q, Some(&self.filtered), &mut self.env_out);
        let silent = self.volume.process(q, (!silent).then_some(std::slice::from_ref(&self.env_out)), &mut self.out);
        (!silent).then_some(&self.out[0])
    }
}

/// `createBass()`: the MonoSynth, the held-note stack, bend and mod wheel. Mono output.
pub struct Bass {
    sample_rate: f32,
    mono: MonoSynth,
    modulation: ModulationBus,
    /// `held`: (note, velocity) in insertion order, the newest last.
    held: Vec<(u8, f64)>,
    sounding: Option<u8>,
    attack_time: f64,
    out: [f32; Q],
    /// The quantum `out` holds.
    rendered: Option<u64>,
}

impl Bass {
    /// Build the bass (allocates). `now` is Tone's time and `frame` the frame being rendered.
    pub fn new(sample_rate: f32, now: f64, frame: u64) -> Self {
        Bass::with_lfo_wave_rate(sample_rate, sample_rate, now, frame)
    }

    /// [`Bass::new`] with the vibrato's LFO wave built at `lfo_wave_rate`
    /// (see [`super::PolySynth::with_lfo_wave_rate`]).
    pub fn with_lfo_wave_rate(sample_rate: f32, lfo_wave_rate: f32, now: f64, frame: u64) -> Self {
        Bass {
            sample_rate,
            mono: MonoSynth::bass(sample_rate, frame),
            modulation: ModulationBus::new(sample_rate, lfo_wave_rate, now, frame),
            held: Vec::with_capacity(MAX_HELD),
            sounding: None,
            attack_time: f64::NEG_INFINITY,
            out: [0.0; Q],
            rendered: None,
        }
    }

    fn attack(&mut self, note: u8, velocity: f64, time: f64, frame: u64) {
        self.attack_time = time.max(self.attack_time + 1.0 / self.sample_rate as f64);
        self.mono.trigger_attack(midi_to_freq(note), self.attack_time, velocity, frame);
    }

    /// `noteOn(note, velocity, time)`; velocity 0..1.
    pub fn note_on(&mut self, note: u8, velocity: f64, time: f64, frame: u64) {
        self.held.retain(|&(n, _)| n != note);
        self.held.push((note, velocity));
        self.sounding = Some(note);
        self.attack(note, velocity, time, frame);
    }

    /// `noteOff(note, time)`.
    pub fn note_off(&mut self, note: u8, time: f64, frame: u64) {
        let Some(i) = self.held.iter().position(|&(n, _)| n == note) else { return };
        self.held.remove(i);
        if self.sounding != Some(note) {
            return;
        }
        let previous = self.held.last().copied();
        self.sounding = previous.map(|(n, _)| n);
        match previous {
            Some((n, velocity)) => self.attack(n, velocity, time, frame),
            None => self.mono.trigger_release(time.max(self.attack_time), frame),
        }
    }

    /// `allNotesOff()`: release 5 ms after the context time.
    pub fn all_notes_off(&mut self, frame: u64) {
        self.held.clear();
        self.sounding = None;
        let immediate = param::context_time(frame, self.sample_rate as f64);
        self.mono.trigger_release((immediate + 0.005).max(self.attack_time), frame);
    }

    /// `setPitchBend(semitones)` at Tone time `now`: the oscillator's detune.
    pub fn set_pitch_bend(&mut self, semitones: f64, now: f64, frame: u64) {
        self.mono.detune.param.set_value(semitones * 100.0, now, frame);
    }

    /// `setModulation(depth)` at Tone time `now`; depth 0..1.
    pub fn set_modulation(&mut self, depth: f64, now: f64, frame: u64) {
        self.modulation.set_modulation(depth, now, frame);
    }

    fn process(&mut self, q: u64) {
        let input = self.mono.process(q);
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
