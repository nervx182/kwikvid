//! Tiny seeded PRNG (SplitMix64) so results depend only on the seed, never on
//! a dependency's algorithm changing between versions.

pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub(crate) fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_in_range() {
        let a: Vec<f32> = (0..100)
            .map({
                let mut r = Rng::new(7);
                move |_| r.next_f32()
            })
            .collect();
        let mut r = Rng::new(7);
        assert!(a
            .iter()
            .all(|&x| x == r.next_f32() && (0.0..1.0).contains(&x)));
        assert_ne!(Rng::new(1).next_u64(), Rng::new(2).next_u64());
    }
}
