//! The engine's random source: mulberry32, the PRNG `verify/probes/tone-refs.mjs` puts in place of
//! `Math.random`, so a port replays exactly the draws Tone made (noise tables, noise start offsets,
//! the reverb IR).

/// mulberry32, as a JS double in [0, 1).
#[derive(Clone, Debug)]
pub struct Mulberry32(u32);

impl Mulberry32 {
    pub fn new(seed: u32) -> Self {
        Mulberry32(seed)
    }

    pub fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6d2b_79f5);
        let a = self.0;
        let mut t = (a ^ (a >> 15)).wrapping_mul(1 | a);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        (t ^ (t >> 14)) as f64 / 4_294_967_296.0
    }
}
