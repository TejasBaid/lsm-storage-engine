//! A sharded, capacity-bounded LRU cache.
//!
//! Each shard is a hash map from key to a slot in a slab-allocated,
//! index-linked doubly linked list (no `unsafe`, no per-node allocation).
//! Sharding by key hash keeps lock hold times short under concurrent reads.
//! Entries carry a *charge* (e.g. block size in bytes); a shard evicts from
//! the cold end until its total charge fits its capacity.

use std::collections::HashMap;
use std::hash::{BuildHasher, BuildHasherDefault, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

const NIL: usize = usize::MAX;

/// Fast non-cryptographic hasher for cache keys (file numbers and block
/// offsets). Keys are engine-generated, so HashDoS resistance is not needed,
/// and SipHash would cost several times more per lookup. An FxHash-style
/// multiply-rotate combine is followed by a murmur3 finalizer so the low
/// bits (used for shard and bucket selection) depend on every input bit —
/// block offsets are multiples of the block size and have no low entropy.
#[derive(Default, Clone, Copy)]
pub struct CacheKeyHasher(u64);

impl Hasher for CacheKeyHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }

    fn finish(&self) -> u64 {
        let mut h = self.0;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        h ^ (h >> 33)
    }
}

type BuildCacheKeyHasher = BuildHasherDefault<CacheKeyHasher>;

struct Node<K, V> {
    key: K,
    value: V,
    charge: usize,
    prev: usize,
    next: usize,
}

struct Shard<K, V> {
    map: HashMap<K, usize, BuildCacheKeyHasher>,
    slots: Vec<Option<Node<K, V>>>,
    free: Vec<usize>,
    head: usize, // most recently used
    tail: usize, // least recently used
    usage: usize,
    capacity: usize,
}

impl<K: Hash + Eq + Clone, V: Clone> Shard<K, V> {
    fn new(capacity: usize) -> Self {
        Shard {
            map: HashMap::default(),
            slots: Vec::new(),
            free: Vec::new(),
            head: NIL,
            tail: NIL,
            usage: 0,
            capacity,
        }
    }

    fn node(&mut self, i: usize) -> &mut Node<K, V> {
        self.slots[i].as_mut().expect("live slot")
    }

    fn unlink(&mut self, i: usize) {
        let (prev, next) = {
            let n = self.node(i);
            (n.prev, n.next)
        };
        if prev == NIL {
            self.head = next;
        } else {
            self.node(prev).next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.node(next).prev = prev;
        }
    }

    fn push_front(&mut self, i: usize) {
        let old_head = self.head;
        {
            let n = self.node(i);
            n.prev = NIL;
            n.next = old_head;
        }
        if old_head == NIL {
            self.tail = i;
        } else {
            self.node(old_head).prev = i;
        }
        self.head = i;
    }

    fn remove_slot(&mut self, i: usize) -> Node<K, V> {
        self.unlink(i);
        let node = self.slots[i].take().expect("live slot");
        self.free.push(i);
        self.usage -= node.charge;
        self.map.remove(&node.key);
        node
    }

    fn get(&mut self, key: &K) -> Option<V> {
        let i = *self.map.get(key)?;
        self.unlink(i);
        self.push_front(i);
        Some(self.node(i).value.clone())
    }

    fn insert(&mut self, key: K, value: V, charge: usize) {
        if let Some(&i) = self.map.get(&key) {
            self.remove_slot(i);
        }
        let node = Node {
            key: key.clone(),
            value,
            charge,
            prev: NIL,
            next: NIL,
        };
        let i = match self.free.pop() {
            Some(i) => {
                self.slots[i] = Some(node);
                i
            }
            None => {
                self.slots.push(Some(node));
                self.slots.len() - 1
            }
        };
        self.map.insert(key, i);
        self.push_front(i);
        self.usage += charge;
        while self.usage > self.capacity && self.tail != NIL {
            let t = self.tail;
            self.remove_slot(t);
        }
    }

    fn remove(&mut self, key: &K) {
        if let Some(&i) = self.map.get(key) {
            self.remove_slot(i);
        }
    }
}

/// Thread-safe sharded LRU cache.
pub struct LruCache<K, V> {
    shards: Box<[Mutex<Shard<K, V>>]>,
    hasher: BuildCacheKeyHasher,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl<K: Hash + Eq + Clone, V: Clone> LruCache<K, V> {
    /// Creates a cache holding up to `capacity` units of charge, split
    /// evenly across `num_shards` shards.
    pub fn new(capacity: usize, num_shards: usize) -> Self {
        let num_shards = num_shards.max(1);
        let per_shard = capacity.div_ceil(num_shards);
        LruCache {
            shards: (0..num_shards)
                .map(|_| Mutex::new(Shard::new(per_shard)))
                .collect(),
            hasher: BuildCacheKeyHasher::default(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    fn shard(&self, key: &K) -> &Mutex<Shard<K, V>> {
        // High bits for the shard; the map uses the low bits for buckets.
        let h = (self.hasher.hash_one(key) >> 32) as usize;
        &self.shards[h % self.shards.len()]
    }

    /// Looks up `key`, marking it most recently used.
    pub fn get(&self, key: &K) -> Option<V> {
        let r = self.shard(key).lock().get(key);
        let counter = if r.is_some() {
            &self.hits
        } else {
            &self.misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
        r
    }

    /// Inserts or replaces `key`, evicting cold entries as needed. An entry
    /// whose charge exceeds the shard capacity is evicted immediately.
    pub fn insert(&self, key: K, value: V, charge: usize) {
        self.shard(&key).lock().insert(key, value, charge);
    }

    /// Removes `key` if present.
    pub fn remove(&self, key: &K) {
        self.shard(key).lock().remove(key);
    }

    /// Total charge currently held.
    pub fn usage(&self) -> usize {
        self.shards.iter().map(|s| s.lock().usage).sum()
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().map.len()).sum()
    }

    /// True if the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `(hits, misses)` since creation.
    pub fn hit_stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn hasher_spreads_block_offsets_across_shards() {
        let c: LruCache<(u64, u64), ()> = LruCache::new(1 << 20, 16);
        let mut counts = [0usize; 16];
        for offset in (0..16_000u64).map(|i| i * 4096) {
            let h = (c.hasher.hash_one((7u64, offset)) >> 32) as usize;
            counts[h % 16] += 1;
        }
        let (min, max) = (counts.iter().min().unwrap(), counts.iter().max().unwrap());
        assert!(
            *min > 800 && *max < 1200,
            "uneven shard distribution: {counts:?}"
        );
    }

    #[test]
    fn evicts_least_recently_used() {
        let c = LruCache::new(3, 1);
        c.insert(1, "a", 1);
        c.insert(2, "b", 1);
        c.insert(3, "c", 1);
        assert_eq!(c.get(&1), Some("a")); // 1 becomes most recent
        c.insert(4, "d", 1); // evicts 2
        assert_eq!(c.get(&2), None);
        assert_eq!(c.get(&1), Some("a"));
        assert_eq!(c.get(&3), Some("c"));
        assert_eq!(c.get(&4), Some("d"));
        assert_eq!(c.usage(), 3);
    }

    #[test]
    fn charge_based_eviction_and_replacement() {
        let c = LruCache::new(10, 1);
        c.insert("big", 1, 8);
        c.insert("small", 2, 2);
        c.insert("big", 3, 5); // replace: usage = 2 + 5
        assert_eq!(c.usage(), 7);
        c.insert("huge", 4, 11); // larger than capacity: evicts everything
        assert_eq!(c.usage(), 0);
        assert!(c.is_empty());
    }

    #[derive(Debug, Clone)]
    enum Op {
        Get(u8),
        Insert(u8, u32),
        Remove(u8),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u8..16).prop_map(Op::Get),
            (0u8..16, any::<u32>()).prop_map(|(k, v)| Op::Insert(k, v)),
            (0u8..16).prop_map(Op::Remove),
        ]
    }

    proptest! {
        /// Compare against a simple Vec-based LRU model.
        #[test]
        fn matches_model(ops in proptest::collection::vec(op(), 0..200), cap in 1usize..8) {
            let c = LruCache::new(cap, 1);
            let mut model: Vec<(u8, u32)> = Vec::new(); // front = most recent
            for op in ops {
                match op {
                    Op::Get(k) => {
                        let expected = model.iter().position(|e| e.0 == k).map(|i| {
                            let e = model.remove(i);
                            model.insert(0, e);
                            e.1
                        });
                        prop_assert_eq!(c.get(&k), expected);
                    }
                    Op::Insert(k, v) => {
                        model.retain(|e| e.0 != k);
                        model.insert(0, (k, v));
                        model.truncate(cap);
                        c.insert(k, v, 1);
                    }
                    Op::Remove(k) => {
                        model.retain(|e| e.0 != k);
                        c.remove(&k);
                    }
                }
                prop_assert_eq!(c.len(), model.len());
            }
        }
    }
}
