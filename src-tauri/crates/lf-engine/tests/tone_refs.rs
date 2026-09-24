//! The Tone reference fixtures themselves (verify/probes/tone-refs.mjs): every file reads at its
//! manifest length, the seeded inputs regenerate bit for bit, the noise tables regenerate from their
//! seed, and the classes score a reference against itself and against a known error.

mod common;

use common::refs::{self, Class};
use lf_engine::dsp::noise::{NoiseTables, TABLE_LENGTH};
use lf_engine::dsp::rng::Mulberry32;

#[test]
fn every_fixture_reads_at_its_manifest_shape() {
    let m = refs::manifest();
    assert!(!m.scenarios.is_empty());
    for s in &m.scenarios {
        let r = refs::reference(&s.id);
        assert_eq!(r.len(), s.channels, "{}", s.id);
        let peak = r.iter().flatten().fold(0.0f32, |p, v| p.max(v.abs()));
        assert!((peak as f64 - s.peak).abs() < 1e-6, "{}: peak {peak} vs {}", s.id, s.peak);
    }
}

#[test]
fn seeded_inputs_regenerate_bit_for_bit() {
    for s in &refs::manifest().scenarios {
        match s.group.as_str() {
            "fx" => assert_eq!(refs::fx_input(s).len(), s.frames),
            "limiter" => assert_eq!(refs::limiter_input(s)[0].len(), s.frames),
            _ => {}
        }
    }
}

#[test]
fn noise_tables_regenerate_from_their_seed() {
    let t = &refs::manifest().tables;
    assert_eq!((t.prng.as_str(), t.length, t.channels), ("mulberry32", TABLE_LENGTH, 2));
    assert_eq!(t.order, ["white", "pink"]);
    let tables = NoiseTables::generate(&mut Mulberry32::new(t.seed));
    refs::assert_sha256("white table", &[&tables.white[0], &tables.white[1]], &t.sha256["white"]);
    refs::assert_sha256("pink table", &[&tables.pink[0], &tables.pink[1]], &t.sha256["pink"]);
}

#[test]
fn classes_score_identity_and_a_known_error() {
    let id = "synth-lead-poly-48000";
    let r = refs::reference(id);
    refs::assert_class(id, &r, Class::N);
    // A −40 dB error fails N (residual −40 dB) but passes S (0.09 dB louder everywhere).
    let louder: Vec<Vec<f32>> = r.iter().map(|c| c.iter().map(|v| v * 1.01).collect()).collect();
    let s = refs::score(&r, &louder, 48000);
    assert!((s.residual_db + 40.0).abs() < 0.01, "{s:?}");
    assert!(s.band_db < 0.1 && s.envelope_db < 0.1, "{s:?}");
    refs::assert_class(id, &louder, Class::S);
}
