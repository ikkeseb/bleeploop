//! D23's playback edges, computed without the engine: the reference the tests hold a lane's output to
//! across an undo's crossfade, a PLAY joining a running loop and an immediate STOP's tail. Each is a
//! linear ramp over the punch ramp's N frames (`dub::ramp`), from the frame the edge lands on; a reversal
//! inside one turns from the level reached. The f64 operations are the specified ones, in their order:
//! a level `from + (to - from) * d / N`, a lane's sample `(gain * (level * x + outgoing)) as f32`.

use lf_engine::grid::Frame;

/// A level ramp `d` frames after it began at `from` toward `to`, over `n` frames: `from` on its first
/// frame, `to` exactly from `n` on.
pub fn level(from: f64, to: f64, d: Frame, n: Frame) -> f64 {
    if d >= n {
        to
    } else if d <= 0 {
        from
    } else {
        from + (to - from) * (d as f64 / n as f64)
    }
}

/// An immediate STOP's tail from full level, `d` frames after the press.
pub fn tail(d: Frame, n: Frame) -> f64 {
    level(1.0, 0.0, d, n)
}

/// A PLAY's fade-in from silence, `d` frames after the press.
pub fn join(d: Frame, n: Frame) -> f64 {
    level(0.0, 1.0, d, n)
}

/// What fades out `k` frames into an undo's crossfade of `n` frames, at the outgoing loop's full level.
pub fn out(k: Frame, n: Frame) -> f64 {
    1.0 - k as f64 / n as f64
}

/// One lane's sample at gain `g` (its settled volume): `level` on what it plays, `x`, plus what fades out
/// from the cache, `outgoing` (its weight already applied).
pub fn sample(g: f64, level: f64, x: f32, outgoing: f64) -> f32 {
    (g * (level * x as f64 + outgoing)) as f32
}
