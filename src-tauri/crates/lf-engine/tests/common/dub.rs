//! D23's overdub punch ramps, computed without the engine: the reference the tests hold a stored layer
//! to. The engine fades a layer in as it writes and, at a clean end, rewrites its last writes toward
//! what each overwrote; this computes the same layer in one chronological pass instead, each write
//! taking both its punch-in and its punch-out weight as it lands (the end is known here beforehand).
//! The two agree while the ramp's writes are distinct positions, which a loop of a bar or more keeps.
//! The f32 operations are the specified ones, in their order: an interior write is `x + fb * old`.

use lf_engine::grid::Frame;

/// The punch ramp at `sr`: 5 ms, rounded, at least one frame (240 at 48 kHz, 221 at 44.1, 40 at 8).
pub fn ramp(sr: u32) -> Frame {
    ((sr as f64 * 0.005).round() as Frame).max(1)
}

/// The sample one overdub write leaves over `old` from input `x` at feedback `fb`, for the write
/// `from_start` frames after its window's first input frame and `to_end` frames before its exclusive
/// end, under a ramp of `n` frames. Punch-in `a = from_start / n` (0 at the first frame: `old`);
/// punch-out `b = to_end / n` (the last write: `1 / n`); 1 past either ramp.
pub fn write(old: f32, x: f32, fb: f32, from_start: Frame, to_end: Frame, n: Frame) -> f32 {
    let y = if from_start >= n {
        x + fb * old
    } else if from_start == 0 {
        old
    } else {
        let a = from_start as f32 / n as f32;
        a * x + (1.0 + a * (fb - 1.0)) * old
    };
    if to_end >= n || y == old {
        y
    } else {
        let b = to_end as f32 / n as f32;
        old + b * (y - old)
    }
}

/// An overdub of input frames `[start, end)` over `pcm`, in capture order: input frame `f` writes
/// position `pos(f)` from `input(f)` at feedback `fb`, under a ramp of `n` frames.
pub fn dub(pcm: &mut [f32], (start, end): (Frame, Frame), n: Frame, fb: f32, pos: impl Fn(Frame) -> usize, input: impl Fn(Frame) -> f32) {
    for f in start..end {
        let p = pos(f);
        pcm[p] = write(pcm[p], input(f), fb, f - start, end - f, n);
    }
}

/// The loop position input frame `f` is dubbed onto: `(f - align - anchor) mod master`.
pub fn pos_fn(anchor: Frame, master: Frame, align: Frame) -> impl Fn(Frame) -> usize {
    move |f| (f - align - anchor).rem_euclid(master) as usize
}
