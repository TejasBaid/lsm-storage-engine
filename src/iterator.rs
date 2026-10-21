//! The internal iterator abstraction and the k-way merging iterator.

use std::cmp::Ordering;

use crate::error::Result;
use crate::key::compare_internal_keys;

/// A forward cursor over internal entries sorted by internal key.
///
/// Positioning methods return `Err` on I/O failure or corruption; after an
/// error the iterator is invalid.
pub trait InternalIterator: Send {
    /// True if positioned at an entry.
    fn valid(&self) -> bool;
    /// Positions at the first entry.
    fn seek_to_first(&mut self) -> Result<()>;
    /// Positions at the first entry with key `>= target` (an internal key).
    fn seek(&mut self, target: &[u8]) -> Result<()>;
    /// Advances to the next entry. Requires `valid()`.
    fn next(&mut self) -> Result<()>;
    /// Current internal key. Requires `valid()`.
    fn key(&self) -> &[u8];
    /// Current value. Requires `valid()`.
    fn value(&self) -> &[u8];
}

/// Heap-allocated iterator trait object.
pub type BoxedIterator = Box<dyn InternalIterator>;

impl<I: InternalIterator + ?Sized> InternalIterator for Box<I> {
    fn valid(&self) -> bool {
        (**self).valid()
    }
    fn seek_to_first(&mut self) -> Result<()> {
        (**self).seek_to_first()
    }
    fn seek(&mut self, target: &[u8]) -> Result<()> {
        (**self).seek(target)
    }
    fn next(&mut self) -> Result<()> {
        (**self).next()
    }
    fn key(&self) -> &[u8] {
        (**self).key()
    }
    fn value(&self) -> &[u8] {
        (**self).value()
    }
}

/// Merges `n` sorted child iterators into one sorted stream.
///
/// The child count is small (memtables + level-0 files + one per deeper
/// level), so the minimum is found with a linear scan: for `n` below ~16
/// this beats a binary heap, which must re-compare keys on every sift.
pub struct MergingIterator {
    children: Vec<BoxedIterator>,
    current: Option<usize>,
}

impl MergingIterator {
    /// Creates a merging iterator. It is unpositioned until a seek.
    pub fn new(children: Vec<BoxedIterator>) -> Self {
        MergingIterator {
            children,
            current: None,
        }
    }

    fn find_smallest(&mut self) {
        let mut best: Option<usize> = None;
        for (i, child) in self.children.iter().enumerate() {
            if !child.valid() {
                continue;
            }
            best = match best {
                Some(b)
                    if compare_internal_keys(child.key(), self.children[b].key())
                        != Ordering::Less =>
                {
                    Some(b)
                }
                _ => Some(i),
            };
        }
        self.current = best;
    }
}

impl InternalIterator for MergingIterator {
    fn valid(&self) -> bool {
        self.current.is_some()
    }

    fn seek_to_first(&mut self) -> Result<()> {
        self.current = None;
        for c in &mut self.children {
            c.seek_to_first()?;
        }
        self.find_smallest();
        Ok(())
    }

    fn seek(&mut self, target: &[u8]) -> Result<()> {
        self.current = None;
        for c in &mut self.children {
            c.seek(target)?;
        }
        self.find_smallest();
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        let i = self.current.expect("next() on invalid MergingIterator");
        if let Err(e) = self.children[i].next() {
            self.current = None;
            return Err(e);
        }
        self.find_smallest();
        Ok(())
    }

    fn key(&self) -> &[u8] {
        self.children[self.current.expect("valid")].key()
    }

    fn value(&self) -> &[u8] {
        self.children[self.current.expect("valid")].value()
    }
}

/// An iterator over an in-memory sorted vector; used in tests.
#[cfg(test)]
pub(crate) struct VecIterator {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    pos: usize,
}

#[cfg(test)]
impl VecIterator {
    pub(crate) fn new(mut entries: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        entries.sort_by(|a, b| compare_internal_keys(&a.0, &b.0));
        let pos = entries.len();
        VecIterator { entries, pos }
    }
}

#[cfg(test)]
impl InternalIterator for VecIterator {
    fn valid(&self) -> bool {
        self.pos < self.entries.len()
    }
    fn seek_to_first(&mut self) -> Result<()> {
        self.pos = 0;
        Ok(())
    }
    fn seek(&mut self, target: &[u8]) -> Result<()> {
        self.pos = self
            .entries
            .partition_point(|(k, _)| compare_internal_keys(k, target) == Ordering::Less);
        Ok(())
    }
    fn next(&mut self) -> Result<()> {
        self.pos += 1;
        Ok(())
    }
    fn key(&self) -> &[u8] {
        &self.entries[self.pos].0
    }
    fn value(&self) -> &[u8] {
        &self.entries[self.pos].1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{ValueType, make_internal_key};
    use proptest::prelude::*;

    fn entry(k: &str, seq: u64) -> (Vec<u8>, Vec<u8>) {
        (
            make_internal_key(k.as_bytes(), seq, ValueType::Value),
            format!("{k}{seq}").into_bytes(),
        )
    }

    #[test]
    fn merges_and_orders_versions() {
        let a = VecIterator::new(vec![entry("a", 1), entry("c", 3)]);
        let b = VecIterator::new(vec![entry("a", 5), entry("b", 2)]);
        let mut m = MergingIterator::new(vec![Box::new(a), Box::new(b)]);
        m.seek_to_first().unwrap();
        let mut got = Vec::new();
        while m.valid() {
            got.push(m.value().to_vec());
            m.next().unwrap();
        }
        assert_eq!(
            got,
            vec![
                b"a5".to_vec(),
                b"a1".to_vec(),
                b"b2".to_vec(),
                b"c3".to_vec()
            ]
        );
    }

    proptest! {
        #[test]
        fn merge_equals_sorted_union(parts in proptest::collection::vec(
            proptest::collection::btree_set((0u8..20, 0u64..50), 0..20), 0..5)) {
            // Make (key, seq) pairs globally unique by tagging seq with the part index.
            let mut all = Vec::new();
            let children: Vec<BoxedIterator> = parts.iter().enumerate().map(|(i, set)| {
                let v: Vec<_> = set.iter().map(|&(k, s)| entry(&format!("{k:02}"), s * 8 + i as u64)).collect();
                all.extend(v.clone());
                Box::new(VecIterator::new(v)) as BoxedIterator
            }).collect();
            all.sort_by(|a, b| compare_internal_keys(&a.0, &b.0));
            let mut m = MergingIterator::new(children);
            m.seek_to_first().unwrap();
            let mut got = Vec::new();
            while m.valid() {
                got.push((m.key().to_vec(), m.value().to_vec()));
                m.next().unwrap();
            }
            prop_assert_eq!(got, all);
        }
    }
}
