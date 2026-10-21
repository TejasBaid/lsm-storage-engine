use std::ops::Bound;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::iterator::{InternalIterator, MergingIterator};
use crate::key::{
    SequenceNumber, VALUE_TYPE_FOR_SEEK, ValueType, make_internal_key, parse_internal_key, user_key,
};
use crate::version::Version;

/// A forward iterator over the live user keys visible at one sequence
/// number, merged across the memtables and every level.
///
/// For each user key only the newest version visible at the iterator's
/// sequence is returned; keys whose newest visible version is a tombstone
/// are skipped. The iterator pins the version it was created from, so the
/// files it reads are never deleted underneath it.
pub struct DbIterator {
    inner: MergingIterator,
    sequence: SequenceNumber,
    _version: Arc<Version>,
    skip_key: Vec<u8>,
    valid: bool,
}

impl DbIterator {
    pub(crate) fn new(
        inner: MergingIterator,
        sequence: SequenceNumber,
        version: Arc<Version>,
    ) -> Self {
        DbIterator {
            inner,
            sequence,
            _version: version,
            skip_key: Vec::new(),
            valid: false,
        }
    }

    /// True if positioned at an entry.
    pub fn valid(&self) -> bool {
        self.valid
    }

    /// Positions at the first visible key.
    pub fn seek_to_first(&mut self) -> Result<()> {
        let r = self.inner.seek_to_first();
        self.settle(r, false)
    }

    /// Positions at the first visible key `>= key`.
    pub fn seek(&mut self, key: &[u8]) -> Result<()> {
        let target = make_internal_key(key, self.sequence, VALUE_TYPE_FOR_SEEK);
        let r = self.inner.seek(&target);
        self.settle(r, false)
    }

    /// Advances to the next visible key.
    ///
    /// # Panics
    /// Panics if the iterator is not valid.
    // A fallible cursor rather than `Iterator`; `RangeIter` is the adapter.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<()> {
        assert!(self.valid, "next() called on an invalid DbIterator");
        self.skip_key.clear();
        self.skip_key.extend_from_slice(user_key(self.inner.key()));
        let r = self.inner.next();
        self.settle(r, true)
    }

    /// The current user key.
    pub fn key(&self) -> &[u8] {
        debug_assert!(self.valid);
        user_key(self.inner.key())
    }

    /// The current value.
    pub fn value(&self) -> &[u8] {
        debug_assert!(self.valid);
        self.inner.value()
    }

    fn settle(&mut self, moved: Result<()>, skipping: bool) -> Result<()> {
        let r = moved.and_then(|()| self.find_next_user_entry(skipping));
        if r.is_err() {
            self.valid = false;
        }
        r
    }

    /// Advances `inner` to the newest visible, non-deleted version of the
    /// next user key. With `skipping`, entries for user keys `<= skip_key`
    /// are hidden (they are older versions of keys already returned or
    /// deleted).
    fn find_next_user_entry(&mut self, mut skipping: bool) -> Result<()> {
        while self.inner.valid() {
            let parsed = parse_internal_key(self.inner.key())
                .ok_or_else(|| Error::corruption("malformed internal key during iteration"))?;
            if parsed.sequence <= self.sequence
                && !(skipping && parsed.user_key <= self.skip_key.as_slice())
            {
                match parsed.value_type {
                    ValueType::Deletion => {
                        self.skip_key.clear();
                        self.skip_key.extend_from_slice(parsed.user_key);
                        skipping = true;
                    }
                    ValueType::Value => {
                        self.valid = true;
                        return Ok(());
                    }
                }
            }
            self.inner.next()?;
        }
        self.valid = false;
        Ok(())
    }
}

/// A Rust [`Iterator`] over `(key, value)` pairs in a key range, created by
/// [`Db::range`](crate::Db::range). I/O errors are yielded as items.
pub struct RangeIter {
    iter: Option<DbIterator>,
    end: Bound<Vec<u8>>,
    pending_error: Option<Error>,
    done: bool,
}

impl RangeIter {
    pub(crate) fn new(
        iter: Result<DbIterator>,
        start: Bound<Vec<u8>>,
        end: Bound<Vec<u8>>,
    ) -> Self {
        let mut this = RangeIter {
            iter: None,
            end,
            pending_error: None,
            done: false,
        };
        let positioned = iter.and_then(|mut it| {
            match &start {
                Bound::Unbounded => it.seek_to_first()?,
                Bound::Included(k) => it.seek(k)?,
                Bound::Excluded(k) => {
                    it.seek(k)?;
                    if it.valid() && it.key() == k.as_slice() {
                        it.next()?;
                    }
                }
            }
            Ok(it)
        });
        match positioned {
            Ok(it) => this.iter = Some(it),
            Err(e) => this.pending_error = Some(e),
        }
        this
    }
}

impl Iterator for RangeIter {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if let Some(e) = self.pending_error.take() {
            self.done = true;
            return Some(Err(e));
        }
        let it = self.iter.as_mut()?;
        if !it.valid() {
            self.done = true;
            return None;
        }
        let key = it.key();
        let in_range = match &self.end {
            Bound::Unbounded => true,
            Bound::Included(e) => key <= e.as_slice(),
            Bound::Excluded(e) => key < e.as_slice(),
        };
        if !in_range {
            self.done = true;
            return None;
        }
        let item = (key.to_vec(), it.value().to_vec());
        if let Err(e) = it.next() {
            self.pending_error = Some(e);
        }
        Some(Ok(item))
    }
}
