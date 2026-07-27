//! Deterministic PRNG.
//!
//! Every randomized suite in the gauntlet derives all of its choices from a
//! single u64 seed that is printed on both success and failure. A failing CI run
//! can therefore always be replayed exactly with
//! `gauntlet exec <suite> --seed <printed seed>`, which is the difference
//! between a useful stress test and an unreproducible flake.

/// xorshift64*: tiny, fast, and stable across platforms and Rust versions.
/// Not cryptographic - it only has to be reproducible.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // A zero state is a fixed point for xorshift, so force it non-zero.
        Self(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish value in `0..n`. Returns 0 for n == 0.
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    /// Inclusive range `lo..=hi`.
    pub fn range(&mut self, lo: u32, hi: u32) -> u32 {
        if hi <= lo {
            return lo;
        }
        lo + (self.next_u64() % (hi - lo + 1) as u64) as u32
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// True with probability `percent`/100.
    pub fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent as usize
    }
}

/// Seed to use when none was supplied on the command line.
///
/// Deliberately time-based so repeated CI runs explore different states, and
/// deliberately printed by every suite so any interesting state can be replayed.
pub fn arbitrary_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678)
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}
