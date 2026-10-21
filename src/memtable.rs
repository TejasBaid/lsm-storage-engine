//! In-memory write buffer.
//!
//! The memtable is a lock-free concurrent skiplist ([`crossbeam_skiplist`])
//! keyed by internal key. Writers are serialized by the database's writer
//! lock, but readers and iterators never block on writers: a reader simply
//! ignores entries whose sequence number is above its snapshot.

use std::collections::VecDeque;
use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_skiplist::SkipMap;

use crate::error::Result;
use crate::iterator::InternalIterator;
use crate::key::{
    InternalKey, LookupResult, SequenceNumber, VALUE_TYPE_FOR_SEEK, ValueType, parse_internal_key,
};

/// Rough per-entry bookkeeping cost of a skiplist node (tower pointers,
/// refcount, two heap allocations), used for flush-size accounting.
const NODE_OVERHEAD: usize = 64;

/// A sorted, concurrent, in-memory table of internal entries.
#[derive(Default)]
pub struct MemTable {
    map: SkipMap<InternalKey, Vec<u8>>,
    approximate_size: AtomicUsize,
}

impl MemTable {
    /// Creates an empty memtable.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts an entry. Callers guarantee `(key, seq)` pairs are unique.
    pub fn add(&self, seq: SequenceNumber, t: ValueType, key: &[u8], value: &[u8]) {
        let ikey = InternalKey::new(key, seq, t);
        self.approximate_size.fetch_add(
            key.len() + value.len() + 8 + NODE_OVERHEAD,
            Ordering::Relaxed,
        );
        self.map.insert(ikey, value.to_vec());
    }

    /// Finds the newest entry for `key` with sequence `<= seq`.
    pub fn get(&self, key: &[u8], seq: SequenceNumber) -> Option<LookupResult> {
        let lookup = InternalKey::new(key, seq, VALUE_TYPE_FOR_SEEK);
        let entry = self.map.lower_bound(Bound::Included(&lookup))?;
        let parsed = parse_internal_key(entry.key().encoded())?;
        if parsed.user_key != key {
            return None;
        }
        Some(match parsed.value_type {
            ValueType::Value => LookupResult::Value(entry.value().clone()),
            ValueType::Deletion => LookupResult::Deleted,
        })
    }

    /// Approximate heap footprint in bytes.
    pub fn approximate_size(&self) -> usize {
        self.approximate_size.load(Ordering::Relaxed)
    }

    /// Number of entries (all versions, including tombstones).
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True if no entries have been added.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Returns an iterator over all entries in internal-key order.
    pub fn iter(self: &Arc<Self>) -> MemTableIterator {
        MemTableIterator {
            mem: Arc::clone(self),
            current: None,
            upcoming: VecDeque::new(),
            exhausted: true,
        }
    }
}

/// Owned cursor over a memtable.
///
/// Skiplist entries borrow the map, so an owning iterator cannot hold one
/// across calls without self-reference. Instead the cursor copies entries in
/// small batches and re-seeks past the last copied key — `O(log n)` per
/// batch, and safe under concurrent inserts.
pub struct MemTableIterator {
    mem: Arc<MemTable>,
    current: Option<(Vec<u8>, Vec<u8>)>,
    upcoming: VecDeque<(Vec<u8>, Vec<u8>)>,
    exhausted: bool,
}

const REFILL_BATCH: usize = 64;

impl MemTableIterator {
    fn refill(&mut self, from: Bound<InternalKey>) {
        self.upcoming.clear();
        for e in self
            .mem
            .map
            .range((from, Bound::Unbounded))
            .take(REFILL_BATCH)
        {
            self.upcoming
                .push_back((e.key().encoded().to_vec(), e.value().clone()));
        }
        self.exhausted = self.upcoming.len() < REFILL_BATCH;
    }

    fn advance(&mut self) {
        if self.upcoming.is_empty()
            && !self.exhausted
            && let Some((k, _)) = self.current.take()
        {
            self.refill(Bound::Excluded(InternalKey::from_encoded(k)));
        }
        self.current = self.upcoming.pop_front();
    }
}

impl InternalIterator for MemTableIterator {
    fn valid(&self) -> bool {
        self.current.is_some()
    }

    fn seek_to_first(&mut self) -> Result<()> {
        self.refill(Bound::Unbounded);
        self.current = self.upcoming.pop_front();
        Ok(())
    }

    fn seek(&mut self, target: &[u8]) -> Result<()> {
        self.refill(Bound::Included(InternalKey::from_encoded(target.to_vec())));
        self.current = self.upcoming.pop_front();
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        debug_assert!(self.valid());
        self.advance();
        Ok(())
    }

    fn key(&self) -> &[u8] {
        &self.current.as_ref().expect("iterator is valid").0
    }

    fn value(&self) -> &[u8] {
        &self.current.as_ref().expect("iterator is valid").1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{make_internal_key, user_key};

    #[test]
    fn get_respects_snapshots_and_tombstones() {
        let m = MemTable::new();
        m.add(1, ValueType::Value, b"a", b"v1");
        m.add(2, ValueType::Value, b"a", b"v2");
        m.add(3, ValueType::Deletion, b"a", b"");
        assert_eq!(m.get(b"a", 0), None);
        assert_eq!(m.get(b"a", 1), Some(LookupResult::Value(b"v1".to_vec())));
        assert_eq!(m.get(b"a", 2), Some(LookupResult::Value(b"v2".to_vec())));
        assert_eq!(m.get(b"a", 3), Some(LookupResult::Deleted));
        assert_eq!(m.get(b"b", 3), None);
        assert_eq!(m.get(b"", 3), None);
    }

    #[test]
    fn iterator_crosses_refill_boundaries_in_order() {
        let m = Arc::new(MemTable::new());
        let n = REFILL_BATCH * 3 + 7;
        for i in (0..n).rev() {
            m.add(
                i as u64 + 1,
                ValueType::Value,
                format!("{i:05}").as_bytes(),
                b"x",
            );
        }
        let mut it = m.iter();
        it.seek_to_first().unwrap();
        let mut count = 0;
        let mut prev: Option<Vec<u8>> = None;
        while it.valid() {
            if let Some(p) = &prev {
                assert!(user_key(p) < user_key(it.key()));
            }
            prev = Some(it.key().to_vec());
            count += 1;
            it.next().unwrap();
        }
        assert_eq!(count, n);

        it.seek(&make_internal_key(
            b"00100",
            u64::MAX >> 8,
            ValueType::Value,
        ))
        .unwrap();
        assert_eq!(user_key(it.key()), b"00100");
    }

    #[test]
    fn size_accounting_grows() {
        let m = MemTable::new();
        assert_eq!(m.approximate_size(), 0);
        m.add(1, ValueType::Value, b"key", b"value");
        assert!(m.approximate_size() >= 8);
        assert_eq!(m.len(), 1);
    }
}
