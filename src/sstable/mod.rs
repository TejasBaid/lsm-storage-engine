//! Sorted String Tables: immutable, sorted, checksummed on-disk runs.
//!
//! ```text
//! +------------------+
//! | data block 0     |  prefix-compressed entries + restart array
//! | trailer (5 B)    |  compression type (u8) + crc32 (u32)
//! +------------------+
//! | ...              |
//! +------------------+
//! | data block N     |
//! | trailer          |
//! +------------------+
//! | filter block     |  bloom filter over all user keys in the table
//! | trailer          |
//! +------------------+
//! | index block      |  one entry per data block:
//! | trailer          |  separator key -> BlockHandle(offset, size)
//! +------------------+
//! | footer (48 B)    |  filter handle, index handle, version, crc, magic
//! +------------------+
//! ```

mod block;
mod bloom;
mod builder;
mod format;
mod table;

pub use block::{Block, BlockBuilder, BlockIterator};
pub use bloom::{BloomFilterPolicy, bloom_hash};
pub use builder::TableBuilder;
pub use format::{BlockHandle, Footer};
pub use table::{BlockCache, Table, TableIterator};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::cache::LruCache;
    use crate::iterator::InternalIterator;
    use crate::key::{ValueType, make_internal_key, parse_internal_key};
    use crate::options::Options;
    use proptest::prelude::*;

    fn build_table(
        dir: &std::path::Path,
        entries: &[(Vec<u8>, Vec<u8>)],
        opts: &Options,
        cache: Option<Arc<BlockCache>>,
    ) -> Arc<Table> {
        let path = dir.join("000001.sst");
        let mut b = TableBuilder::new(std::fs::File::create(&path).unwrap(), opts);
        for (k, v) in entries {
            b.add(k, v).unwrap();
        }
        let size = b.finish().unwrap();
        assert_eq!(size, std::fs::metadata(&path).unwrap().len());
        Arc::new(Table::open(std::fs::File::open(&path).unwrap(), 1, size, cache, true).unwrap())
    }

    fn entries(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..n)
            .map(|i| {
                (
                    make_internal_key(
                        format!("key{i:08}").as_bytes(),
                        i as u64 + 1,
                        ValueType::Value,
                    ),
                    format!("value-{i}-{}", "x".repeat(i % 50)).into_bytes(),
                )
            })
            .collect()
    }

    fn small_blocks() -> Options {
        Options {
            block_size: 256,
            ..Options::default()
        }
    }

    #[test]
    fn roundtrip_many_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let data = entries(2_000);
        let t = build_table(dir.path(), &data, &small_blocks(), None);
        assert!(t.data_block_handles().unwrap().len() > 50);
        let mut it = t.iter(true);
        it.seek_to_first().unwrap();
        for (k, v) in &data {
            assert!(it.valid());
            assert_eq!(it.key(), &k[..]);
            assert_eq!(it.value(), &v[..]);
            it.next().unwrap();
        }
        assert!(!it.valid());
    }

    #[test]
    fn point_lookups_with_bloom_and_cache() {
        let dir = tempfile::tempdir().unwrap();
        let data = entries(1_000);
        let cache = Arc::new(LruCache::new(1 << 20, 4));
        let t = build_table(dir.path(), &data, &small_blocks(), Some(cache.clone()));
        for (i, (k, v)) in data.iter().enumerate() {
            let uk = format!("key{i:08}");
            assert!(t.may_contain(uk.as_bytes()));
            let seek = make_internal_key(uk.as_bytes(), u64::MAX >> 8, ValueType::Value);
            let (fk, fv) = t.seek_entry(&seek, true).unwrap().unwrap();
            assert_eq!((&fk, &fv), (k, v));
        }
        let (_, misses_before) = cache.hit_stats();
        for (k, _) in data.iter().take(100) {
            t.seek_entry(k, true).unwrap();
        }
        let (hits, misses_after) = cache.hit_stats();
        assert!(hits > 0);
        assert_eq!(
            misses_before, misses_after,
            "second pass should be fully cached"
        );

        let absent = (0..1_000)
            .filter(|i| t.may_contain(format!("missing{i}").as_bytes()))
            .count();
        assert!(absent < 50, "bloom filter passed {absent}/1000 absent keys");
    }

    #[test]
    fn seek_past_end_and_between_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let data = entries(500);
        let t = build_table(dir.path(), &data, &small_blocks(), None);
        let past = make_internal_key(b"zzz", 1, ValueType::Value);
        assert!(t.seek_entry(&past, true).unwrap().is_none());
        // A key between two existing keys lands on the successor.
        let between = make_internal_key(b"key00000100a", 1, ValueType::Value);
        let (k, _) = t.seek_entry(&between, true).unwrap().unwrap();
        assert_eq!(parse_internal_key(&k).unwrap().user_key, b"key00000101");
    }

    #[test]
    fn detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let data = entries(300);
        let t = build_table(dir.path(), &data, &small_blocks(), None);
        drop(t);
        let path = dir.path().join("000001.sst");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[10] ^= 0x55; // inside the first data block
        std::fs::write(&path, &bytes).unwrap();
        let t = Arc::new(
            Table::open(
                std::fs::File::open(&path).unwrap(),
                1,
                bytes.len() as u64,
                None,
                true,
            )
            .unwrap(),
        );
        let mut it = t.iter(false);
        assert!(it.seek_to_first().unwrap_err().is_corruption());

        // Truncated file: footer is gone.
        std::fs::write(&path, &bytes[..bytes.len() - 10]).unwrap();
        let r = Table::open(
            std::fs::File::open(&path).unwrap(),
            1,
            bytes.len() as u64 - 10,
            None,
            true,
        );
        assert!(r.is_err());
    }

    #[test]
    fn empty_table() {
        let dir = tempfile::tempdir().unwrap();
        let t = build_table(dir.path(), &[], &Options::default(), None);
        let mut it = t.iter(true);
        it.seek_to_first().unwrap();
        assert!(!it.valid());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn table_seek_matches_model(keys in proptest::collection::btree_set(proptest::collection::vec(any::<u8>(), 0..10), 1..300),
                                    probe in proptest::collection::vec(any::<u8>(), 0..10),
                                    block_size in 64usize..2048,
                                    restart in 1usize..32) {
            let dir = tempfile::tempdir().unwrap();
            let opts = Options { block_size, block_restart_interval: restart, ..Options::default() };
            let data: Vec<_> = keys.iter().enumerate()
                .map(|(i, k)| (make_internal_key(k, i as u64, ValueType::Value), k.clone()))
                .collect();
            let t = build_table(dir.path(), &data, &opts, None);
            let seek = make_internal_key(&probe, u64::MAX >> 8, ValueType::Value);
            let expected = data.iter().find(|(_, uk)| uk.as_slice() >= probe.as_slice()).cloned();
            prop_assert_eq!(t.seek_entry(&seek, false).unwrap(), expected);
            for (_, uk) in &data {
                prop_assert!(t.may_contain(uk));
            }
        }

        #[test]
        fn opening_garbage_never_panics(data in proptest::collection::vec(any::<u8>(), 0..512)) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("garbage.sst");
            std::fs::write(&path, &data).unwrap();
            if let Ok(t) = Table::open(std::fs::File::open(&path).unwrap(), 9, data.len() as u64, None, true) {
                let t = Arc::new(t);
                let mut it = t.iter(false);
                let _ = it.seek_to_first();
            }
        }
    }
}
