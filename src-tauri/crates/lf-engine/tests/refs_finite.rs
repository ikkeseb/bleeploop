//! Reference scoring rejects non-finite samples before any tolerance comparison, without fixtures.

mod common;

use common::refs::score;

#[test]
#[should_panic(expected = "non-finite sample in render: channel 1, frame 2")]
fn rejects_one_nan_in_render() {
    let reference = vec![vec![0.5; 4]; 2];
    let mut render = reference.clone();
    render[1][2] = f32::NAN;
    score(&reference, &render, 48_000);
}

#[test]
#[should_panic(expected = "non-finite sample in render: channel 0, frame 0")]
fn rejects_all_nan_render() {
    score(&[vec![0.5; 4]], &[vec![f32::NAN; 4]], 48_000);
}

#[test]
#[should_panic(expected = "non-finite sample in render: channel 0, frame 1")]
fn rejects_positive_infinity_in_render() {
    score(&[vec![0.5; 3]], &[vec![0.5, f32::INFINITY, 0.5]], 48_000);
}

#[test]
#[should_panic(expected = "non-finite sample in render: channel 0, frame 1")]
fn rejects_negative_infinity_in_render() {
    score(&[vec![0.5; 3]], &[vec![0.5, f32::NEG_INFINITY, 0.5]], 48_000);
}

#[test]
#[should_panic(expected = "non-finite sample in reference: channel 1, frame 2")]
fn rejects_nan_in_reference() {
    let render = vec![vec![0.5; 4]; 2];
    let mut reference = render.clone();
    reference[1][2] = f32::NAN;
    score(&reference, &render, 48_000);
}

#[test]
fn identical_finite_signals_score_at_floor() {
    let signal = vec![vec![0.5; 4]];
    let result = score(&signal, &signal, 48_000);
    let expected = 10.0 * 1e-300f64.log10();
    assert_eq!(result.residual_db.to_bits(), expected.to_bits());
    assert_eq!(result.band_db.to_bits(), 0.0f64.to_bits());
    assert_eq!(result.envelope_db.to_bits(), 0.0f64.to_bits());
}

#[test]
fn finite_offset_has_expected_residual() {
    let result = score(&[vec![0.5; 4]], &[vec![0.5 + 1.0 / 1024.0; 4]], 48_000);
    let expected = 10.0 * (1.0f64 / 262_144.0).log10();
    assert_eq!(result.residual_db.to_bits(), expected.to_bits());
    assert!(result.band_db.is_finite());
    assert!(result.envelope_db.is_finite());
}
