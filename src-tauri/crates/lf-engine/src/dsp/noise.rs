//! Tone's noise tables (`tone/build/esm/source/Noise.js`): 44100 × 5 frames × 2 channels of white and
//! pink noise, generated once from the random source in Tone's order. Tone stamps them with the rate of
//! the context that first needed them, so they play back at that rate whatever the table length says.

use super::rng::Mulberry32;

/// Frames per table channel (Tone's `BUFFER_LENGTH`, independent of the sample rate).
pub const TABLE_LENGTH: usize = 44100 * 5;

pub struct NoiseTables {
    pub white: [Vec<f32>; 2],
    pub pink: [Vec<f32>; 2],
}

impl NoiseTables {
    /// Tone's generation order: the white table, the white Noise's start offset (one draw, skipped here:
    /// the caller replays offsets itself), then the pink table.
    pub fn generate(rng: &mut Mulberry32) -> Self {
        let white = [0, 1].map(|_| (0..TABLE_LENGTH).map(|_| (rng.next_f64() * 2.0 - 1.0) as f32).collect());
        rng.next_f64();
        let pink = [0, 1].map(|_| {
            let (mut b0, mut b1, mut b2, mut b3, mut b4, mut b5, mut b6) = (0.0f64, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            (0..TABLE_LENGTH)
                .map(|_| {
                    let white = rng.next_f64() * 2.0 - 1.0;
                    b0 = 0.99886 * b0 + white * 0.0555179;
                    b1 = 0.99332 * b1 + white * 0.0750759;
                    b2 = 0.969 * b2 + white * 0.153852;
                    b3 = 0.8665 * b3 + white * 0.3104856;
                    b4 = 0.55 * b4 + white * 0.5329522;
                    b5 = -0.7616 * b5 - white * 0.016898;
                    // Tone stores the sum in a Float32Array, then scales the stored value.
                    let sum = (b0 + b1 + b2 + b3 + b4 + b5 + b6 + white * 0.5362) as f32;
                    b6 = white * 0.115926;
                    (sum as f64 * 0.11) as f32
                })
                .collect()
        });
        NoiseTables { white, pink }
    }
}
