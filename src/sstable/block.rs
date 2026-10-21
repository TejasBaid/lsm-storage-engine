//! Prefix-compressed sorted blocks (used for data and index blocks).
//!
//! ```text
//! block   := entry* restart* (u32 LE) num_restarts (u32 LE)
//! entry   := varint shared | varint non_shared | varint value_len
//!            | key_suffix (non_shared bytes) | value (value_len bytes)
//! ```
//!
//! Each key stores only the suffix that differs from the previous key.
//! Every `restart_interval` entries the prefix is reset (`shared == 0`) and
//! the entry's offset is recorded as a *restart point*, enabling binary
//! search over restart points followed by a short linear scan.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::coding::{decode_fixed32, decode_varint32, put_fixed32, put_varint32};
use crate::error::{Error, Result};
use crate::iterator::InternalIterator;
use crate::key::KeyComparator;

/// Incrementally builds a block.
pub struct BlockBuilder {
    buf: Vec<u8>,
    restarts: Vec<u32>,
    counter: usize,
    restart_interval: usize,
    last_key: Vec<u8>,
    num_entries: usize,
}

impl BlockBuilder {
    /// Creates a builder that resets prefix compression every
    /// `restart_interval` keys.
    pub fn new(restart_interval: usize) -> Self {
        assert!(restart_interval >= 1);
        BlockBuilder {
            buf: Vec::new(),
            restarts: vec![0],
            counter: 0,
            restart_interval,
            last_key: Vec::new(),
            num_entries: 0,
        }
    }

    /// Appends an entry. Keys must be added in strictly increasing order.
    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        let shared = if self.counter < self.restart_interval {
            self.last_key
                .iter()
                .zip(key)
                .take_while(|(a, b)| a == b)
                .count()
        } else {
            self.restarts.push(self.buf.len() as u32);
            self.counter = 0;
            0
        };
        let non_shared = key.len() - shared;
        put_varint32(&mut self.buf, shared as u32);
        put_varint32(&mut self.buf, non_shared as u32);
        put_varint32(&mut self.buf, value.len() as u32);
        self.buf.extend_from_slice(&key[shared..]);
        self.buf.extend_from_slice(value);
        self.last_key.truncate(shared);
        self.last_key.extend_from_slice(&key[shared..]);
        self.counter += 1;
        self.num_entries += 1;
    }

    /// Serialized size if `finish` were called now.
    pub fn size_estimate(&self) -> usize {
        self.buf.len() + self.restarts.len() * 4 + 4
    }

    /// True if no entries were added since the last reset.
    pub fn is_empty(&self) -> bool {
        self.num_entries == 0
    }

    /// Appends the restart array and returns the finished block contents,
    /// resetting the builder for reuse.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.buf);
        for &r in &self.restarts {
            put_fixed32(&mut out, r);
        }
        put_fixed32(&mut out, self.restarts.len() as u32);
        self.restarts.clear();
        self.restarts.push(0);
        self.counter = 0;
        self.last_key.clear();
        self.num_entries = 0;
        out
    }
}

/// An immutable, validated block.
pub struct Block {
    data: Vec<u8>,
    restart_offset: usize,
    num_restarts: usize,
}

impl Block {
    /// Validates the restart trailer of `data`.
    pub fn new(data: Vec<u8>) -> Result<Self> {
        if data.len() < 4 {
            return Err(Error::corruption("block too small"));
        }
        let num_restarts = decode_fixed32(&data[data.len() - 4..]) as usize;
        let max_restarts = (data.len() - 4) / 4;
        if num_restarts > max_restarts {
            return Err(Error::corruption("block restart count out of range"));
        }
        let restart_offset = data.len() - 4 - num_restarts * 4;
        Ok(Block {
            data,
            restart_offset,
            num_restarts,
        })
    }

    /// Size of the block contents in bytes.
    pub fn size(&self) -> usize {
        self.data.len()
    }

    fn restart_point(&self, i: usize) -> Result<usize> {
        let off = decode_fixed32(&self.data[self.restart_offset + 4 * i..]) as usize;
        if off > self.restart_offset {
            return Err(Error::corruption("block restart point out of range"));
        }
        Ok(off)
    }

    /// Decodes the entry header at `offset`: `(shared, non_shared,
    /// value_len, header_len)`, with bounds checks.
    fn decode_entry(&self, offset: usize) -> Result<(usize, usize, usize, usize)> {
        let limit = self.restart_offset;
        let bad = || Error::corruption("bad block entry");
        // Fast path: all three lengths fit in one byte (the common case).
        if let Some(&[shared, non_shared, value_len]) = self.data.get(offset..offset + 3)
            && (shared | non_shared | value_len) < 0x80
        {
            let (shared, non_shared, value_len) =
                (shared as usize, non_shared as usize, value_len as usize);
            if offset + 3 + non_shared + value_len > limit {
                return Err(bad());
            }
            return Ok((shared, non_shared, value_len, 3));
        }
        let mut p = offset;
        let mut next = || -> Result<usize> {
            let (v, n) =
                decode_varint32(self.data.get(p..limit).ok_or_else(bad)?).ok_or_else(bad)?;
            p += n;
            Ok(v as usize)
        };
        let shared = next()?;
        let non_shared = next()?;
        let value_len = next()?;
        let header_len = p - offset;
        if p + non_shared + value_len > limit {
            return Err(bad());
        }
        Ok((shared, non_shared, value_len, header_len))
    }

    /// Returns an iterator ordered by `cmp`.
    pub fn iter(self: &Arc<Self>, cmp: KeyComparator) -> BlockIterator {
        BlockIterator {
            block: Arc::clone(self),
            cmp,
            next_offset: self.restart_offset,
            key: Vec::new(),
            value_range: (0, 0),
            valid: false,
        }
    }
}

/// Cursor over a [`Block`].
pub struct BlockIterator {
    block: Arc<Block>,
    cmp: KeyComparator,
    next_offset: usize,
    key: Vec<u8>,
    value_range: (usize, usize),
    valid: bool,
}

impl BlockIterator {
    fn seek_to_restart(&mut self, index: usize) -> Result<()> {
        self.key.clear();
        self.next_offset = self.block.restart_point(index)?;
        Ok(())
    }

    /// Decodes the entry at `next_offset`, or becomes invalid at the end.
    fn parse_next(&mut self) -> Result<()> {
        let offset = self.next_offset;
        if offset >= self.block.restart_offset {
            self.valid = false;
            return Ok(());
        }
        let decoded = self.block.decode_entry(offset);
        let (shared, non_shared, value_len, header_len) = match decoded {
            Ok(d) if d.0 <= self.key.len() => d,
            Ok(_) => return self.corrupt("block entry shares more bytes than previous key"),
            Err(e) => {
                self.valid = false;
                return Err(e);
            }
        };
        let key_start = offset + header_len;
        self.key.truncate(shared);
        self.key
            .extend_from_slice(&self.block.data[key_start..key_start + non_shared]);
        let value_start = key_start + non_shared;
        self.value_range = (value_start, value_start + value_len);
        self.next_offset = value_start + value_len;
        self.valid = true;
        Ok(())
    }

    fn corrupt(&mut self, msg: &str) -> Result<()> {
        self.valid = false;
        Err(Error::corruption(msg.to_string()))
    }

    /// Key stored at restart point `index` (whose prefix is never shared).
    fn restart_key(&self, index: usize) -> Result<&[u8]> {
        let off = self.block.restart_point(index)?;
        let (shared, non_shared, _, header_len) = self.block.decode_entry(off)?;
        if shared != 0 {
            return Err(Error::corruption("restart entry has shared prefix"));
        }
        Ok(&self.block.data[off + header_len..off + header_len + non_shared])
    }
}

impl InternalIterator for BlockIterator {
    fn valid(&self) -> bool {
        self.valid
    }

    fn seek_to_first(&mut self) -> Result<()> {
        self.valid = false;
        if self.block.num_restarts == 0 {
            return Ok(());
        }
        self.seek_to_restart(0)?;
        self.parse_next()
    }

    fn seek(&mut self, target: &[u8]) -> Result<()> {
        self.valid = false;
        if self.block.num_restarts == 0 {
            return Ok(());
        }
        // Binary search for the last restart point with key < target.
        let (mut left, mut right) = (0usize, self.block.num_restarts - 1);
        while left < right {
            let mid = left + (right - left).div_ceil(2);
            if (self.cmp)(self.restart_key(mid)?, target) == Ordering::Less {
                left = mid;
            } else {
                right = mid - 1;
            }
        }
        self.seek_to_restart(left)?;
        loop {
            self.parse_next()?;
            if !self.valid || (self.cmp)(&self.key, target) != Ordering::Less {
                return Ok(());
            }
        }
    }

    fn next(&mut self) -> Result<()> {
        debug_assert!(self.valid);
        self.parse_next()
    }

    fn key(&self) -> &[u8] {
        debug_assert!(self.valid);
        &self.key
    }

    fn value(&self) -> &[u8] {
        debug_assert!(self.valid);
        &self.block.data[self.value_range.0..self.value_range.1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn bytewise(a: &[u8], b: &[u8]) -> Ordering {
        a.cmp(b)
    }

    fn build(entries: &[(Vec<u8>, Vec<u8>)], interval: usize) -> Arc<Block> {
        let mut b = BlockBuilder::new(interval);
        for (k, v) in entries {
            b.add(k, v);
        }
        Arc::new(Block::new(b.finish()).unwrap())
    }

    fn collect(it: &mut BlockIterator) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        while it.valid() {
            out.push((it.key().to_vec(), it.value().to_vec()));
            it.next().unwrap();
        }
        out
    }

    #[test]
    fn prefix_compression_shrinks_block() {
        let entries: Vec<_> = (0..100)
            .map(|i| (format!("user:profile:{i:06}").into_bytes(), b"v".to_vec()))
            .collect();
        let raw: usize = entries.iter().map(|(k, v)| k.len() + v.len()).sum();
        let block = build(&entries, 16);
        assert!(block.size() < raw, "{} >= {raw}", block.size());
    }

    #[test]
    fn empty_block_iterates_nothing() {
        let block = build(&[], 16);
        let mut it = block.iter(bytewise);
        it.seek_to_first().unwrap();
        assert!(!it.valid());
        it.seek(b"x").unwrap();
        assert!(!it.valid());
    }

    #[test]
    fn rejects_bad_restart_count() {
        assert!(Block::new(vec![0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(Block::new(vec![1, 2]).is_err());
    }

    proptest! {
        #[test]
        fn roundtrip_and_seek(keys in proptest::collection::btree_set(proptest::collection::vec(any::<u8>(), 0..12), 0..200),
                              interval in 1usize..20,
                              probe in proptest::collection::vec(any::<u8>(), 0..12)) {
            let entries: Vec<_> = keys.iter().map(|k| (k.clone(), k.iter().rev().cloned().collect::<Vec<u8>>())).collect();
            let block = build(&entries, interval);
            let mut it = block.iter(bytewise);
            it.seek_to_first().unwrap();
            prop_assert_eq!(collect(&mut it), entries.clone());

            it.seek(&probe).unwrap();
            let expected: Vec<_> = entries.iter().filter(|(k, _)| k.as_slice() >= probe.as_slice()).cloned().collect();
            prop_assert_eq!(collect(&mut it), expected);
        }

        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..200),
                                       probe in proptest::collection::vec(any::<u8>(), 0..8)) {
            if let Ok(block) = Block::new(data) {
                let block = Arc::new(block);
                let mut it = block.iter(bytewise);
                if it.seek_to_first().is_ok() {
                    let mut steps = 0;
                    while it.valid() && steps < 1000 {
                        let _ = (it.key(), it.value());
                        if it.next().is_err() { break; }
                        steps += 1;
                    }
                }
                let _ = it.seek(&probe);
            }
        }
    }
}
