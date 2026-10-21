use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::key::SequenceNumber;

/// Multiset of sequence numbers pinned by live snapshots.
#[derive(Default)]
pub(crate) struct SnapshotList {
    pinned: Mutex<BTreeMap<SequenceNumber, usize>>,
}

impl SnapshotList {
    /// Pins the current last sequence. Reading the sequence under the list
    /// lock linearizes snapshot creation with [`SnapshotList::smallest`], so
    /// a compaction can never drop a version a new snapshot still needs.
    pub fn acquire(&self, last_sequence: &AtomicU64) -> SequenceNumber {
        let mut pinned = self.pinned.lock();
        let seq = last_sequence.load(Ordering::Acquire);
        *pinned.entry(seq).or_insert(0) += 1;
        seq
    }

    fn release(&self, seq: SequenceNumber) {
        let mut pinned = self.pinned.lock();
        if let Some(count) = pinned.get_mut(&seq) {
            *count -= 1;
            if *count == 0 {
                pinned.remove(&seq);
            }
        }
    }

    /// Oldest sequence any reader may still observe: the oldest snapshot,
    /// or the latest sequence if there are none. Compaction may discard any
    /// version shadowed by a newer version at or below this sequence.
    pub fn smallest(&self, last_sequence: &AtomicU64) -> SequenceNumber {
        let pinned = self.pinned.lock();
        pinned
            .keys()
            .next()
            .copied()
            .unwrap_or_else(|| last_sequence.load(Ordering::Acquire))
    }

    pub fn len(&self) -> usize {
        self.pinned.lock().values().sum()
    }
}

/// A consistent, read-only view of the database as of the moment it was
/// taken. Pass it via [`ReadOptions::at_snapshot`](crate::ReadOptions::at_snapshot).
///
/// While a snapshot is alive, compaction preserves every entry it can see.
/// Dropping the snapshot releases them.
pub struct Snapshot {
    sequence: SequenceNumber,
    list: Arc<SnapshotList>,
}

impl Snapshot {
    pub(crate) fn new(list: &Arc<SnapshotList>, last_sequence: &AtomicU64) -> Self {
        let sequence = list.acquire(last_sequence);
        Snapshot {
            sequence,
            list: Arc::clone(list),
        }
    }

    /// The sequence number this snapshot reads at.
    pub fn sequence(&self) -> SequenceNumber {
        self.sequence
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        self.list.release(self.sequence);
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("sequence", &self.sequence)
            .finish()
    }
}
