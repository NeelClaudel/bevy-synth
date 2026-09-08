//! A tiny deterministic RNG.
//!
//! `rand` is not used here for two reasons: the audio thread must not touch
//! anything that might allocate or lock, and a reproducible seed makes the
//! generative sequencer testable — the same seed must always produce the same
//! melody, or "that pattern sounded great, get it back" becomes impossible.
//!
//! This is xorshift64*, which is fast, has no state larger than a `u64`, and is
//! far better distributed than anything hand-rolled. It is emphatically not
//! cryptographic.

#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        // A zero state is a fixed point for xorshift; nudge it off.
        Self {
            state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed },
        }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, 1)`.
    #[inline]
    pub fn next_f32(&mut self) -> f32 {
        // Take the top 24 bits: exactly the f32 mantissa, so every value is
        // representable and the distribution has no gaps.
        ((self.next_u64() >> 40) as f32) * (1.0 / 16_777_216.0)
    }

    /// Uniform in `[-1, 1)`. Used for the noise oscillator.
    #[inline]
    pub fn next_bipolar(&mut self) -> f32 {
        self.next_f32() * 2.0 - 1.0
    }

    /// Uniform integer in `[0, n)`. Returns 0 when `n == 0`.
    #[inline]
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }

    /// Returns `true` with probability `p`.
    #[inline]
    pub fn chance(&mut self, p: f32) -> bool {
        self.next_f32() < p
    }

    /// Picks an index from a weight table, proportional to the weights.
    ///
    /// Used by the generative sequencer to favour some scale degrees over
    /// others. Returns 0 if all weights are zero.
    pub fn weighted(&mut self, weights: &[f32]) -> usize {
        let total: f32 = weights.iter().sum();
        if total <= 0.0 {
            return 0;
        }
        let mut pick = self.next_f32() * total;
        for (i, &w) in weights.iter().enumerate() {
            pick -= w;
            if pick <= 0.0 {
                return i;
            }
        }
        weights.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn floats_stay_in_range() {
        let mut r = Rng::new(7);
        for _ in 0..100_000 {
            let f = r.next_f32();
            assert!((0.0..1.0).contains(&f));
        }
    }

    #[test]
    fn weighted_respects_zero_weights() {
        let mut r = Rng::new(3);
        // Only index 2 can ever be chosen.
        for _ in 0..1000 {
            assert_eq!(r.weighted(&[0.0, 0.0, 1.0, 0.0]), 2);
        }
    }
}
