//! Cache of open SSTable readers, bounded by `max_open_files`.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cache::LruCache;
use crate::error::Result;
use crate::filename::table_file_name;
use crate::sstable::{BlockCache, Table};

/// Opens tables lazily and keeps the most recently used ones open.
pub struct TableCache {
    dir: PathBuf,
    tables: LruCache<u64, Arc<Table>>,
    block_cache: Option<Arc<BlockCache>>,
    verify_checksums: bool,
    bloom_negatives: AtomicU64,
}

impl TableCache {
    pub fn new(
        dir: PathBuf,
        max_open_files: usize,
        block_cache: Option<Arc<BlockCache>>,
        verify_checksums: bool,
    ) -> Self {
        TableCache {
            dir,
            // Few shards: table opens are rare compared to block reads.
            tables: LruCache::new(max_open_files, 4),
            block_cache,
            verify_checksums,
            bloom_negatives: AtomicU64::new(0),
        }
    }

    /// Returns the open table for `number`, opening it if needed.
    pub fn get(&self, number: u64, file_size: u64) -> Result<Arc<Table>> {
        if let Some(t) = self.tables.get(&number) {
            return Ok(t);
        }
        let file = File::open(table_file_name(&self.dir, number))?;
        let table = Arc::new(Table::open(
            file,
            number,
            file_size,
            self.block_cache.clone(),
            self.verify_checksums,
        )?);
        self.tables.insert(number, Arc::clone(&table), 1);
        Ok(table)
    }

    /// Drops the cached reader for a deleted table.
    pub fn evict(&self, number: u64) {
        self.tables.remove(&number);
    }

    pub fn block_cache(&self) -> Option<&Arc<BlockCache>> {
        self.block_cache.as_ref()
    }

    pub fn record_bloom_negative(&self) {
        self.bloom_negatives.fetch_add(1, Ordering::Relaxed);
    }

    pub fn bloom_negatives(&self) -> u64 {
        self.bloom_negatives.load(Ordering::Relaxed)
    }
}
