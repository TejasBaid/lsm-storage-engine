use std::fs::File;
use std::sync::Arc;

use super::block::{Block, BlockIterator};
use super::bloom::BloomFilterPolicy;
use super::format::{BlockHandle, FOOTER_SIZE, Footer, read_block, read_exact_at};
use crate::cache::LruCache;
use crate::error::{Error, Result};
use crate::iterator::InternalIterator;
use crate::key::{LookupResult, ValueType, compare_internal_keys, parse_internal_key, user_key};

/// Shared block cache keyed by `(table file number, block offset)`.
pub type BlockCache = LruCache<(u64, u64), Arc<Block>>;

/// An open, immutable SSTable.
///
/// The index block and bloom filter are pinned in memory for the lifetime
/// of the table; data blocks are read on demand through the block cache.
pub struct Table {
    file: File,
    file_number: u64,
    file_size: u64,
    index: Arc<Block>,
    filter: Option<Vec<u8>>,
    block_cache: Option<Arc<BlockCache>>,
    verify_checksums: bool,
}

impl Table {
    /// Opens a table, validating its footer and loading index and filter.
    pub fn open(
        file: File,
        file_number: u64,
        file_size: u64,
        block_cache: Option<Arc<BlockCache>>,
        verify_checksums: bool,
    ) -> Result<Self> {
        if file_size < FOOTER_SIZE as u64 {
            return Err(Error::corruption(format!(
                "table {file_number} is too short ({file_size} bytes) to be an SSTable"
            )));
        }
        let mut footer_buf = [0u8; FOOTER_SIZE];
        read_exact_at(&file, &mut footer_buf, file_size - FOOTER_SIZE as u64)?;
        let footer = Footer::decode(&footer_buf)?;
        // Metadata blocks are always verified: they are read once per open.
        let index = Arc::new(Block::new(read_block(
            &file,
            file_size,
            footer.index,
            true,
        )?)?);
        let filter = if footer.filter.size > 0 {
            Some(read_block(&file, file_size, footer.filter, true)?)
        } else {
            None
        };
        Ok(Table {
            file,
            file_number,
            file_size,
            index,
            filter,
            block_cache,
            verify_checksums,
        })
    }

    /// The table's file number.
    pub fn file_number(&self) -> u64 {
        self.file_number
    }

    /// Size of the bloom filter in bytes (0 if absent).
    pub fn filter_size(&self) -> usize {
        self.filter.as_ref().map_or(0, Vec::len)
    }

    /// Returns `false` if the bloom filter proves `user_key` is absent.
    pub fn may_contain(&self, user_key: &[u8]) -> bool {
        self.filter
            .as_ref()
            .is_none_or(|f| BloomFilterPolicy::key_may_match(user_key, f))
    }

    /// Handles of every data block, in key order.
    pub fn data_block_handles(&self) -> Result<Vec<BlockHandle>> {
        let mut it = self.index.iter(compare_internal_keys);
        it.seek_to_first()?;
        let mut out = Vec::new();
        while it.valid() {
            out.push(BlockHandle::decode(it.value())?);
            it.next()?;
        }
        Ok(out)
    }

    /// Reads a data block, consulting and optionally filling the cache.
    pub fn read_data_block(&self, handle: BlockHandle, fill_cache: bool) -> Result<Arc<Block>> {
        let key = (self.file_number, handle.offset);
        if let Some(cache) = &self.block_cache
            && let Some(block) = cache.get(&key)
        {
            return Ok(block);
        }
        let data = read_block(&self.file, self.file_size, handle, self.verify_checksums)?;
        let block = Arc::new(Block::new(data)?);
        if fill_cache && let Some(cache) = &self.block_cache {
            cache.insert(key, Arc::clone(&block), block.size());
        }
        Ok(block)
    }

    /// Returns the first entry with internal key `>= ikey`, if any. Callers
    /// should consult [`Table::may_contain`] first.
    pub fn seek_entry(
        self: &Arc<Self>,
        ikey: &[u8],
        fill_cache: bool,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut it = self.iter(fill_cache);
        it.seek(ikey)?;
        Ok(it.valid().then(|| (it.key().to_vec(), it.value().to_vec())))
    }

    /// Point lookup: the newest entry for `user_key(ikey)` with sequence
    /// at or below the one encoded in `ikey`, if this table has one.
    /// Callers should consult [`Table::may_contain`] first.
    pub fn get(&self, ikey: &[u8], fill_cache: bool) -> Result<Option<LookupResult>> {
        let ukey = user_key(ikey);
        let mut index = self.index.iter(compare_internal_keys);
        index.seek(ikey)?;
        while index.valid() {
            let block = self.read_data_block(BlockHandle::decode(index.value())?, fill_cache)?;
            let mut it = block.iter(compare_internal_keys);
            it.seek(ikey)?;
            if it.valid() {
                let parsed = parse_internal_key(it.key()).ok_or_else(|| {
                    Error::corruption(format!("bad internal key in table {}", self.file_number))
                })?;
                if parsed.user_key != ukey {
                    return Ok(None);
                }
                return Ok(Some(match parsed.value_type {
                    ValueType::Value => LookupResult::Value(it.value().to_vec()),
                    ValueType::Deletion => LookupResult::Deleted,
                }));
            }
            // Target sorts after this block's last key: try the next block.
            index.next()?;
        }
        Ok(None)
    }

    /// Two-level iterator over the table's entries.
    pub fn iter(self: &Arc<Self>, fill_cache: bool) -> TableIterator {
        TableIterator {
            index_iter: self.index.iter(compare_internal_keys),
            table: Arc::clone(self),
            data_iter: None,
            fill_cache,
        }
    }
}

/// Iterates the index block, opening each referenced data block in turn.
pub struct TableIterator {
    table: Arc<Table>,
    index_iter: BlockIterator,
    data_iter: Option<BlockIterator>,
    fill_cache: bool,
}

impl TableIterator {
    fn load_data_block(&mut self) -> Result<()> {
        self.data_iter = None;
        if !self.index_iter.valid() {
            return Ok(());
        }
        let handle = BlockHandle::decode(self.index_iter.value())?;
        let block = self.table.read_data_block(handle, self.fill_cache)?;
        self.data_iter = Some(block.iter(compare_internal_keys));
        Ok(())
    }

    /// Moves forward past exhausted data blocks.
    fn skip_empty_blocks(&mut self) -> Result<()> {
        while !self.data_iter.as_ref().is_some_and(|d| d.valid()) {
            if !self.index_iter.valid() {
                self.data_iter = None;
                return Ok(());
            }
            self.index_iter.next()?;
            self.load_data_block()?;
            if let Some(d) = &mut self.data_iter {
                d.seek_to_first()?;
            }
        }
        Ok(())
    }

    fn on_error<T>(&mut self, r: Result<T>) -> Result<T> {
        if r.is_err() {
            self.data_iter = None;
        }
        r
    }
}

impl InternalIterator for TableIterator {
    fn valid(&self) -> bool {
        self.data_iter.as_ref().is_some_and(|d| d.valid())
    }

    fn seek_to_first(&mut self) -> Result<()> {
        let r = (|| {
            self.index_iter.seek_to_first()?;
            self.load_data_block()?;
            if let Some(d) = &mut self.data_iter {
                d.seek_to_first()?;
            }
            self.skip_empty_blocks()
        })();
        self.on_error(r)
    }

    fn seek(&mut self, target: &[u8]) -> Result<()> {
        let r = (|| {
            self.index_iter.seek(target)?;
            self.load_data_block()?;
            if let Some(d) = &mut self.data_iter {
                d.seek(target)?;
            }
            self.skip_empty_blocks()
        })();
        self.on_error(r)
    }

    fn next(&mut self) -> Result<()> {
        let r = (|| {
            if let Some(d) = &mut self.data_iter {
                d.next()?;
            }
            self.skip_empty_blocks()
        })();
        self.on_error(r)
    }

    fn key(&self) -> &[u8] {
        self.data_iter.as_ref().expect("valid").key()
    }

    fn value(&self) -> &[u8] {
        self.data_iter.as_ref().expect("valid").value()
    }
}
