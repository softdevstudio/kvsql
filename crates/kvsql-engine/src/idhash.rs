//! A fast hasher for page ids. Page ids are small integers chosen by the
//! engine, not by users, so the standard hasher's protection against chosen
//! keys buys nothing here and costs time on every page lookup.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

#[derive(Default, Clone, Copy)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u64(&mut self, n: u64) {
        // Fibonacci hashing; the high bits are well mixed, and hashbrown
        // uses the top bits for its control bytes.
        self.0 = (self.0 ^ n).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

pub type IdMap<V> = HashMap<u64, V, BuildHasherDefault<IdHasher>>;
pub type IdSet = HashSet<u64, BuildHasherDefault<IdHasher>>;
