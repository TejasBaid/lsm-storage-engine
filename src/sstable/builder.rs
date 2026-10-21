use std::fs::File;
use std::io::{BufWriter, Write};

use super::block::BlockBuilder;
use super::bloom::{BloomFilterPolicy, bloom_hash};
use super::format::{BlockHandle, Footer, block_trailer};
use crate::error::Result;
use crate::key::{shortest_separator, user_key};
use crate::options::Options;

/// Writes a sorted run of internal entries as an SSTable.
///
/// Entries must be added in strictly increasing internal-key order.
/// [`TableBuilder::finish`] writes the filter, index and footer and
/// `fsync`s the file.
pub struct TableBuilder {
    writer: BufWriter<File>,
    offset: u64,
    block_size: usize,
    data_block: BlockBuilder,
    index_block: BlockBuilder,
    last_key: Vec<u8>,
    num_entries: u64,
    /// Handle of the last flushed data block, whose index entry is deferred
    /// until the next key is known so a short separator can be chosen.
    pending_index: Option<BlockHandle>,
    bloom: Option<BloomFilterPolicy>,
    key_hashes: Vec<u32>,
}

impl TableBuilder {
    /// Creates a builder writing to `file` (which should be empty).
    pub fn new(file: File, opts: &Options) -> Self {
        TableBuilder {
            writer: BufWriter::with_capacity(256 << 10, file),
            offset: 0,
            block_size: opts.block_size,
            data_block: BlockBuilder::new(opts.block_restart_interval),
            // Index lookups are binary searches; prefix compression would
            // only slow them down for little space gain.
            index_block: BlockBuilder::new(1),
            last_key: Vec::new(),
            num_entries: 0,
            pending_index: None,
            bloom: (opts.bloom_bits_per_key > 0)
                .then(|| BloomFilterPolicy::new(opts.bloom_bits_per_key)),
            key_hashes: Vec::new(),
        }
    }

    /// Appends an entry.
    pub fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        debug_assert!(
            self.num_entries == 0 || crate::key::compare_internal_keys(&self.last_key, key).is_lt(),
            "keys added out of order"
        );
        if let Some(handle) = self.pending_index.take() {
            let sep = shortest_separator(&self.last_key, key);
            let mut enc = Vec::with_capacity(20);
            handle.encode_to(&mut enc);
            self.index_block.add(&sep, &enc);
        }
        if self.bloom.is_some() {
            let uk = user_key(key);
            if self.num_entries == 0 || user_key(&self.last_key) != uk {
                self.key_hashes.push(bloom_hash(uk));
            }
        }
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.num_entries += 1;
        self.data_block.add(key, value);
        if self.data_block.size_estimate() >= self.block_size {
            self.flush_data_block()?;
        }
        Ok(())
    }

    fn flush_data_block(&mut self) -> Result<()> {
        if self.data_block.is_empty() {
            return Ok(());
        }
        let contents = self.data_block.finish();
        let handle = self.write_raw_block(&contents)?;
        self.pending_index = Some(handle);
        Ok(())
    }

    fn write_raw_block(&mut self, contents: &[u8]) -> Result<BlockHandle> {
        let handle = BlockHandle {
            offset: self.offset,
            size: contents.len() as u64,
        };
        let trailer = block_trailer(contents);
        self.writer.write_all(contents)?;
        self.writer.write_all(&trailer)?;
        self.offset += (contents.len() + trailer.len()) as u64;
        Ok(handle)
    }

    /// Number of entries added so far.
    pub fn num_entries(&self) -> u64 {
        self.num_entries
    }

    /// Bytes written so far plus the pending data block.
    pub fn file_size(&self) -> u64 {
        self.offset + self.data_block.size_estimate() as u64
    }

    /// Writes the remaining blocks and footer, syncs, and returns the final
    /// file size.
    pub fn finish(mut self) -> Result<u64> {
        self.flush_data_block()?;

        let filter = match self.bloom {
            Some(policy) => {
                let data = policy.create_filter(&self.key_hashes);
                self.write_raw_block(&data)?
            }
            None => BlockHandle::default(),
        };

        if let Some(handle) = self.pending_index.take() {
            let mut enc = Vec::with_capacity(20);
            handle.encode_to(&mut enc);
            self.index_block.add(&self.last_key, &enc);
        }
        let index_contents = self.index_block.finish();
        let index = self.write_raw_block(&index_contents)?;

        let footer = Footer { filter, index }.encode();
        self.writer.write_all(&footer)?;
        self.offset += footer.len() as u64;
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(self.offset)
    }
}
