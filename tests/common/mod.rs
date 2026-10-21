//! Shared helpers for integration tests.
#![allow(dead_code)]

use lsm_engine::Options;

/// Options with tiny buffers so that a few thousand writes exercise
/// flushes, multi-level compaction, and multi-block tables.
pub fn small_options() -> Options {
    Options {
        write_buffer_size: 16 << 10,
        target_file_size: 16 << 10,
        block_size: 512,
        block_restart_interval: 4,
        max_bytes_for_level_base: 64 << 10,
        l0_compaction_trigger: 2,
        l0_slowdown_writes_trigger: 6,
        l0_stop_writes_trigger: 10,
        block_cache_capacity: 64 << 10,
        max_open_files: 20,
        ..Options::default()
    }
}

/// Deterministic, dependency-free PRNG (SplitMix64).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

pub fn key(i: u64) -> Vec<u8> {
    format!("key{i:08}").into_bytes()
}

pub fn value(i: u64, generation: u64) -> Vec<u8> {
    let mut v = format!("value-{i}-{generation}-").into_bytes();
    v.resize(v.len() + (i % 97) as usize, b'.');
    v
}
