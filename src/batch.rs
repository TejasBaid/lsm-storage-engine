//! Atomic write batches.
//!
//! A batch is serialized once and used verbatim as the WAL record payload:
//!
//! ```text
//! batch  := sequence (u64 LE) | count (u32 LE) | record*
//! record := 0x01 | varint klen | key | varint vlen | value     (put)
//!         | 0x00 | varint klen | key                          (delete)
//! ```
//!
//! Entry `i` of the batch is assigned sequence number `sequence + i`. Because
//! a batch is a single WAL record and becomes visible to readers only after
//! all of its entries are in the memtable, batches are atomic with respect
//! to both crashes and concurrent readers.

use crate::coding::{Decoder, decode_fixed32, decode_fixed64, put_length_prefixed_slice};
use crate::error::{Error, Result};
use crate::key::{SequenceNumber, ValueType};
use crate::memtable::MemTable;

const HEADER_SIZE: usize = 12;

/// A group of puts and deletes applied atomically by [`Db::write`].
///
/// [`Db::write`]: crate::Db::write
#[derive(Clone, PartialEq, Eq)]
pub struct WriteBatch {
    rep: Vec<u8>,
    user_bytes: u64,
}

/// One decoded operation of a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchOp<'a> {
    /// Set `key` to `value`.
    Put(&'a [u8], &'a [u8]),
    /// Delete `key`.
    Delete(&'a [u8]),
}

impl Default for WriteBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for WriteBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteBatch")
            .field("count", &self.count())
            .field("bytes", &self.rep.len())
            .finish()
    }
}

impl WriteBatch {
    /// Creates an empty batch.
    pub fn new() -> Self {
        WriteBatch {
            rep: vec![0; HEADER_SIZE],
            user_bytes: 0,
        }
    }

    /// Queues `key = value`.
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> &mut Self {
        let (key, value) = (key.as_ref(), value.as_ref());
        self.bump_count();
        self.rep.push(ValueType::Value as u8);
        put_length_prefixed_slice(&mut self.rep, key);
        put_length_prefixed_slice(&mut self.rep, value);
        self.user_bytes += (key.len() + value.len()) as u64;
        self
    }

    /// Queues a deletion of `key`.
    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> &mut Self {
        let key = key.as_ref();
        self.bump_count();
        self.rep.push(ValueType::Deletion as u8);
        put_length_prefixed_slice(&mut self.rep, key);
        self.user_bytes += key.len() as u64;
        self
    }

    /// Removes all queued operations.
    pub fn clear(&mut self) {
        self.rep.clear();
        self.rep.resize(HEADER_SIZE, 0);
        self.user_bytes = 0;
    }

    /// Number of queued operations.
    pub fn count(&self) -> u32 {
        decode_fixed32(&self.rep[8..])
    }

    /// True if the batch has no operations.
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// Size of the serialized batch in bytes.
    pub fn approximate_size(&self) -> usize {
        self.rep.len()
    }

    /// Sum of key and value lengths (the "user bytes" used for write
    /// amplification accounting).
    pub(crate) fn user_bytes(&self) -> u64 {
        self.user_bytes
    }

    fn bump_count(&mut self) {
        let n = self.count().checked_add(1).expect("write batch too large");
        self.rep[8..12].copy_from_slice(&n.to_le_bytes());
    }

    /// Sequence number of the first operation (set when the batch is written).
    #[doc(hidden)]
    pub fn sequence(&self) -> SequenceNumber {
        decode_fixed64(&self.rep)
    }

    pub(crate) fn set_sequence(&mut self, seq: SequenceNumber) {
        self.rep[..8].copy_from_slice(&seq.to_le_bytes());
    }

    pub(crate) fn data(&self) -> &[u8] {
        &self.rep
    }

    /// Reconstructs a batch from a WAL record, validating its structure.
    #[doc(hidden)]
    pub fn from_data(data: &[u8]) -> Result<Self> {
        if data.len() < HEADER_SIZE {
            return Err(Error::corruption("write batch shorter than header"));
        }
        let mut batch = WriteBatch {
            rep: data.to_vec(),
            user_bytes: 0,
        };
        let mut found = 0u32;
        let mut user_bytes = 0u64;
        for op in batch.iter() {
            user_bytes += match op? {
                BatchOp::Put(k, v) => (k.len() + v.len()) as u64,
                BatchOp::Delete(k) => k.len() as u64,
            };
            found += 1;
        }
        if found != batch.count() {
            return Err(Error::corruption(format!(
                "write batch header claims {} entries, found {found}",
                batch.count()
            )));
        }
        batch.user_bytes = user_bytes;
        Ok(batch)
    }

    /// Iterates over the batch's operations in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = Result<BatchOp<'_>>> + '_ {
        let mut d = Decoder::new(&self.rep[HEADER_SIZE..]);
        let mut failed = false;
        std::iter::from_fn(move || {
            if failed || d.is_empty() {
                return None;
            }
            let op = (|| {
                let tag = d.u8()?;
                let key = d.length_prefixed()?;
                match ValueType::from_u8(tag) {
                    Some(ValueType::Value) => Ok(BatchOp::Put(key, d.length_prefixed()?)),
                    Some(ValueType::Deletion) => Ok(BatchOp::Delete(key)),
                    None => Err(Error::corruption(format!("unknown batch tag {tag}"))),
                }
            })();
            failed = op.is_err();
            Some(op)
        })
    }

    /// Applies every operation to `mem`, assigning consecutive sequence
    /// numbers starting at [`WriteBatch::sequence`].
    pub(crate) fn insert_into(&self, mem: &MemTable) -> Result<()> {
        for (seq, op) in (self.sequence()..).zip(self.iter()) {
            match op? {
                BatchOp::Put(k, v) => mem.add(seq, ValueType::Value, k, v),
                BatchOp::Delete(k) => mem.add(seq, ValueType::Deletion, k, b""),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn encode_decode() {
        let mut b = WriteBatch::new();
        b.put(b"a", b"1").delete(b"b").put(b"", b"");
        b.set_sequence(100);
        let decoded = WriteBatch::from_data(b.data()).unwrap();
        assert_eq!(decoded.sequence(), 100);
        assert_eq!(decoded.count(), 3);
        let ops: Vec<_> = decoded.iter().map(|o| o.unwrap()).collect();
        assert_eq!(
            ops,
            vec![
                BatchOp::Put(b"a", b"1"),
                BatchOp::Delete(b"b"),
                BatchOp::Put(b"", b"")
            ]
        );
        assert_eq!(decoded.user_bytes(), b.user_bytes());
    }

    #[test]
    fn count_mismatch_is_corruption() {
        let mut b = WriteBatch::new();
        b.put(b"a", b"1");
        let mut data = b.data().to_vec();
        data[8] = 2;
        assert!(WriteBatch::from_data(&data).unwrap_err().is_corruption());
    }

    #[test]
    fn insert_assigns_consecutive_sequences() {
        let mem = MemTable::new();
        let mut b = WriteBatch::new();
        b.put(b"k", b"v1").put(b"k", b"v2");
        b.set_sequence(10);
        b.insert_into(&mem).unwrap();
        use crate::key::LookupResult as MemGet;
        assert_eq!(mem.get(b"k", 10), Some(MemGet::Value(b"v1".to_vec())));
        assert_eq!(mem.get(b"k", 11), Some(MemGet::Value(b"v2".to_vec())));
        assert_eq!(mem.get(b"k", 9), None);
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..128)) {
            let _ = WriteBatch::from_data(&data);
        }
    }
}
