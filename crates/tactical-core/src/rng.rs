//! Deterministic PRNG (xorshift64*) so battles are reproducible from a seed.

#[derive(Debug, Clone)]
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    /// Seeds are mixed with a fixed odd constant: xorshift needs a non-zero
    /// state, and plain substitution (the old `seed.max(1)`) made seed 0 and
    /// seed 1 the same stream — a scenario script asking for seed 0 silently
    /// reproduced seed 1. The multiply is invertible, so distinct seeds stay
    /// distinct, and an odd constant can never map a seed onto the zero
    /// state that would stick every following draw at 0.
    pub fn new(seed: u64) -> Self {
        XorShift64 {
            state: (seed ^ 0x9E37_79B9_7F4A_7C15).wrapping_mul(0x2545_F491_4F6C_DD1D),
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Uniform float in [0, 1).
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Uniform integer in [0, n).
    pub fn next_below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let mut a = XorShift64::new(42);
        let mut b = XorShift64::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn floats_in_range() {
        let mut r = XorShift64::new(7);
        for _ in 0..1000 {
            let f = r.next_f32();
            assert!((0.0..1.0).contains(&f));
        }
    }

    #[test]
    fn distinct_seeds_give_distinct_streams() {
        // Seed 0 is a legal scenario value and used to be rewritten to 1,
        // so `seed=0` quietly reproduced `seed=1`.
        let mut zero = XorShift64::new(0);
        let mut one = XorShift64::new(1);
        assert_ne!(zero.next_u64(), one.next_u64());
        // Neighbouring seeds stay distinct too (mixing, not clamping).
        for seed in 0..64u64 {
            let mut a = XorShift64::new(seed);
            let mut b = XorShift64::new(seed + 1);
            assert_ne!(
                (a.next_u64(), a.next_u64()),
                (b.next_u64(), b.next_u64()),
                "seed {seed} collides with {}",
                seed + 1
            );
        }
    }

    #[test]
    fn zero_seed_does_not_stick_at_zero() {
        // xorshift degenerates on a zero state; the mix must never land there.
        let mut r = XorShift64::new(0);
        assert!(r.next_u64() != 0);
        assert!(r.next_f32() > 0.0);
    }
}
