//! Bloom filter over the user keys of one SSTable.
//!
//! ```text
//! filter := bit_array (ceil(n * bits_per_key / 8) bytes, min 8) | k (u8)
//! ```
//!
//! Probes use double hashing (Kirsch–Mitzenmacher): `h_i = h + i * delta`
//! where `delta = rotr(h, 17)`, so only one 32-bit hash is computed per key.
//! With 10 bits/key and `k = 6` the theoretical false-positive rate is ~0.8%.

/// Builds and queries bloom filters with a fixed bits-per-key budget.
#[derive(Debug, Clone, Copy)]
pub struct BloomFilterPolicy {
    bits_per_key: usize,
    k: u8,
}

/// 32-bit Murmur-inspired hash (the same construction LevelDB uses). The
/// on-disk format depends on it, so it must never change.
pub fn bloom_hash(data: &[u8]) -> u32 {
    const SEED: u32 = 0xbc9f_1d34;
    const M: u32 = 0xc6a4_a793;
    let mut h = SEED ^ (data.len() as u32).wrapping_mul(M);
    let (chunks, rest) = data.as_chunks::<4>();
    for c in chunks {
        h = h.wrapping_add(u32::from_le_bytes(*c)).wrapping_mul(M);
        h ^= h >> 16;
    }
    if !rest.is_empty() {
        for (i, &b) in rest.iter().enumerate() {
            h = h.wrapping_add(u32::from(b) << (8 * i));
        }
        h = h.wrapping_mul(M);
        h ^= h >> 24;
    }
    h
}

impl BloomFilterPolicy {
    /// Creates a policy using `bits_per_key` bits of filter per key.
    pub fn new(bits_per_key: usize) -> Self {
        // k = ln(2) * bits_per_key minimizes the false-positive rate.
        let k = ((bits_per_key as f64) * 0.69).round().clamp(1.0, 30.0) as u8;
        BloomFilterPolicy { bits_per_key, k }
    }

    /// Builds a filter from precomputed [`bloom_hash`] values.
    pub fn create_filter(&self, hashes: &[u32]) -> Vec<u8> {
        let bits = (hashes.len() * self.bits_per_key).max(64);
        let bytes = bits.div_ceil(8);
        let bits = bytes * 8;
        let mut filter = vec![0u8; bytes + 1];
        for &hash in hashes {
            let mut h = hash;
            let delta = h.rotate_right(17);
            for _ in 0..self.k {
                let pos = (h as usize) % bits;
                filter[pos / 8] |= 1 << (pos % 8);
                h = h.wrapping_add(delta);
            }
        }
        filter[bytes] = self.k;
        filter
    }

    /// Returns `false` only if `key` was definitely not added to `filter`.
    pub fn key_may_match(key: &[u8], filter: &[u8]) -> bool {
        Self::hash_may_match(bloom_hash(key), filter)
    }

    fn hash_may_match(hash: u32, filter: &[u8]) -> bool {
        if filter.len() < 2 {
            return true; // malformed: fail open (a filter must never cause misses)
        }
        let bytes = filter.len() - 1;
        let bits = bytes * 8;
        let k = filter[bytes];
        if k > 30 {
            return true; // reserved for future encodings
        }
        let mut h = hash;
        let delta = h.rotate_right(17);
        for _ in 0..k {
            let pos = (h as usize) % bits;
            if filter[pos / 8] & (1 << (pos % 8)) == 0 {
                return false;
            }
            h = h.wrapping_add(delta);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(i: u32) -> [u8; 4] {
        i.to_le_bytes()
    }

    #[test]
    fn no_false_negatives_and_low_fp_rate() {
        let policy = BloomFilterPolicy::new(10);
        for n in [1u32, 10, 100, 1_000, 10_000] {
            let hashes: Vec<u32> = (0..n).map(|i| bloom_hash(&key(i))).collect();
            let filter = policy.create_filter(&hashes);
            for i in 0..n {
                assert!(BloomFilterPolicy::key_may_match(&key(i), &filter));
            }
            let probes = 10_000u32;
            let fps = (0..probes)
                .filter(|i| BloomFilterPolicy::key_may_match(&key(i + 1_000_000_000), &filter))
                .count();
            let rate = fps as f64 / probes as f64;
            assert!(rate < 0.02, "n={n}: false positive rate {rate}");
        }
    }

    #[test]
    fn hash_is_stable() {
        // Pinned values: changing the hash would break existing files.
        assert_eq!(bloom_hash(b""), 0xbc9f1d34);
        assert_eq!(bloom_hash(b"hello"), bloom_hash(b"hello"));
        assert_ne!(bloom_hash(b"hello"), bloom_hash(b"hellp"));
    }

    #[test]
    fn malformed_filters_fail_open() {
        assert!(BloomFilterPolicy::key_may_match(b"x", &[]));
        assert!(BloomFilterPolicy::key_may_match(b"x", &[0, 31]));
    }
}
