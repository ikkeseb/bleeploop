//! What verify/probes/synth-note-ownership.mjs measured on the Web Audio synths, on the engine's
//! instruments through its commands: the mono bass's held-note stack (release either of two held notes
//! and the other one sounds, and the last release ends the note), and a poly synth's voice steal (with
//! every voice busy a fresh note sounds, and the stolen note's late note-off leaves it be). Levels are
//! read off the bus over the probe's analyser window. The probe's sustained lead run stays with the
//! sender: sustain is the router's, not the engine's (`src/instruments.rs`).

mod common;

use common::Rig;
use lf_engine::dsp::synth::PolyKind;
use lf_engine::{Command, Instrument, NoteTarget};

/// The probe's analyser: the last 4096 frames.
const WINDOW: usize = 4096;
/// The probe's velocity, 110 of 127.
const VELOCITY: f32 = 110.0 / 127.0;

/// Render `seconds` from now; the bus over the last WINDOW frames of it.
fn listen(rig: &mut Rig, seconds: f64) -> Vec<f32> {
    rig.keep_output();
    rig.advance(rig.seconds(seconds));
    rig.bus[rig.bus.len() - WINDOW..].to_vec()
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| v as f64 * v as f64).sum::<f64>() / x.len() as f64).sqrt()
}

/// The amplitude of `note`'s fundamental in `x` (one DFT bin at its frequency, as the probe's).
fn amplitude(x: &[f32], note: u8, sr: u32) -> f64 {
    let w = std::f64::consts::TAU * 440.0 * 2f64.powf((note as f64 - 69.0) / 12.0) / sr as f64;
    let (re, im) = x.iter().enumerate().fold((0.0, 0.0), |(re, im), (k, &v)| (re + v as f64 * (w * k as f64).cos(), im + v as f64 * (w * k as f64).sin()));
    2.0 * re.hypot(im) / x.len() as f64
}

#[test]
fn releasing_either_of_two_held_bass_notes_leaves_the_other_sounding() {
    // 55 is the newer, sounding note: its release falls back to 48 (the held stack's legato). 48's
    // release drops it from the stack and 55 sounds on.
    for released in [55u8, 48] {
        let held = if released == 55 { 48 } else { 55 };
        let mut rig = Rig::new();
        rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Bass)));
        rig.press(Command::NoteOn(48, VELOCITY));
        rig.advance(rig.seconds(0.18));
        rig.press(Command::NoteOn(55, VELOCITY));
        let before = rms(&listen(&mut rig, 0.18));
        rig.press(Command::NoteOff(released));
        let after = listen(&mut rig, 0.65);
        let (right, wrong) = (amplitude(&after, held, rig.sr), amplitude(&after, released, rig.sr));
        rig.press(Command::NoteOff(held));
        let end = rms(&listen(&mut rig, 0.65));
        println!("bass, {released} released: rms {before:.4} before, {:.4} after; {held} at {right:.4}, {released} at {wrong:.4}; {end:e} after the last release", rms(&after));
        assert!(before > 0.01, "the bass sounds with two notes held: {before}");
        assert!(rms(&after) > 0.2 * before, "releasing {released} keeps {held} sounding");
        assert!(right > 3.0 * wrong, "the bass sounds the held {held}, not the released {released}");
        assert!(end < 1e-5, "the last release ends the note: {end}");
    }
}

/// Every voice of `instrument` busy (notes 36 up, one frame apart, 36 the oldest), then 83; `then` at
/// `wait` seconds; the bus from there over 0.18 s, and the rms after ALL NOTES OFF and its release.
fn steal(instrument: Instrument, kind: PolyKind, then: Option<Command>) -> (f64, Vec<f32>, f64) {
    let pad = kind == PolyKind::Pad;
    let mut rig = Rig::new();
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(instrument)));
    for note in 36..36 + kind.spec().max_polyphony as u8 {
        rig.press(Command::NoteOn(note, VELOCITY));
    }
    rig.press(Command::NoteOn(83, VELOCITY));
    let fresh = amplitude(&listen(&mut rig, if pad { 1.0 } else { 0.2 }), 83, rig.sr);
    rig.keep_output();
    match then {
        Some(command) => rig.press(command),
        None => rig.advance(1),
    }
    rig.advance(rig.seconds(0.18));
    let after = rig.bus.clone();
    rig.press(Command::AllNotesOff);
    let end = rms(&listen(&mut rig, if pad { 2.3 } else { 1.1 }));
    (fresh, after, end)
}

#[test]
fn a_stolen_voices_late_note_off_leaves_the_note_that_took_it() {
    for (instrument, kind) in [(Instrument::Lead, PolyKind::Lead), (Instrument::Pad, PolyKind::Pad), (Instrument::Piano, PolyKind::Piano), (Instrument::Organ, PolyKind::Organ)] {
        let (fresh, stale, end) = steal(instrument, kind, Some(Command::NoteOff(36)));
        let (_, untouched, _) = steal(instrument, kind, None);
        let (_, released, _) = steal(instrument, kind, Some(Command::NoteOff(37)));
        let tail = &stale[stale.len() - WINDOW..];
        let after = amplitude(tail, 83, 48_000);
        println!("{instrument:?}: 83 at {fresh:.4} with every voice busy, {after:.4} after 36's late note-off; {end:e} after ALL NOTES OFF");
        assert!(fresh > 0.002, "{instrument:?}: the fresh note sounds with every voice busy: {fresh}");
        // The stolen note's note-off does nothing at all, where a held note's releases it.
        assert!(stale == untouched, "{instrument:?}: 36's late note-off changed the output");
        assert!(released != untouched, "{instrument:?}: 37's note-off releases 37 (so 36's voice was the one taken)");
        assert!(after > 0.15 * fresh, "{instrument:?}: the note that took the voice sounds on: {after} of {fresh}");
        assert!(end < 1e-5, "{instrument:?}: ALL NOTES OFF releases every voice: {end}");
    }
}
