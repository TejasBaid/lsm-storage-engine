//! Internal key encoding.
//!
//! Every entry stored in a memtable or SSTable is keyed by an *internal key*:
//!
//! ```text
//! +---------------------+-------------------------------------+
//! | user key (n bytes)  | trailer: (sequence << 8) | type  u64 LE |
//! +---------------------+-------------------------------------+
//! ```
//!
//! Internal keys sort by user key ascending, then by trailer *descending*, so
//! the newest version of a user key comes first. A lookup at snapshot `s`
//! seeks to `(key, s, VALUE_TYPE_FOR_SEEK)` and lands on the newest entry with
//! sequence `<= s`.

use std::cmp::Ordering;
use std::fmt;

use crate::coding::decode_fixed64;

/// A monotonically increasing write sequence number (56 bits are usable).
pub type SequenceNumber = u64;

/// The largest representable sequence number.
pub const MAX_SEQUENCE_NUMBER: SequenceNumber = (1 << 56) - 1;

/// Length of the `(sequence, type)` trailer appended to each user key.
pub const TRAILER_LEN: usize = 8;

/// Kind of an internal entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ValueType {
    /// A tombstone: the key was deleted at this sequence number.
    Deletion = 0,
    /// A live value.
    Value = 1,
}

impl ValueType {
    /// Decodes a type tag, returning `None` for unknown values.
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(ValueType::Deletion),
            1 => Some(ValueType::Value),
            _ => None,
        }
    }
}

/// Outcome of a point lookup that found an entry for the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupResult {
    /// The newest visible entry is a value.
    Value(Vec<u8>),
    /// The newest visible entry is a tombstone.
    Deleted,
}

/// Because trailers sort descending, the highest type value sorts first among
/// entries with equal user key and sequence; seeking with it finds them all.
pub const VALUE_TYPE_FOR_SEEK: ValueType = ValueType::Value;

#[inline]
pub fn pack_sequence_and_type(seq: SequenceNumber, t: ValueType) -> u64 {
    debug_assert!(seq <= MAX_SEQUENCE_NUMBER);
    (seq << 8) | t as u64
}

pub fn append_internal_key(dst: &mut Vec<u8>, user_key: &[u8], seq: SequenceNumber, t: ValueType) {
    dst.extend_from_slice(user_key);
    dst.extend_from_slice(&pack_sequence_and_type(seq, t).to_le_bytes());
}

/// Builds the internal key for `(user_key, seq, t)`.
pub fn make_internal_key(user_key: &[u8], seq: SequenceNumber, t: ValueType) -> Vec<u8> {
    let mut v = Vec::with_capacity(user_key.len() + TRAILER_LEN);
    append_internal_key(&mut v, user_key, seq, t);
    v
}

/// A decoded view of an internal key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedInternalKey<'a> {
    /// The user-supplied key.
    pub user_key: &'a [u8],
    /// Sequence number at which the entry was written.
    pub sequence: SequenceNumber,
    /// Whether the entry is a value or a tombstone.
    pub value_type: ValueType,
}

/// Parses an internal key, returning `None` if it is malformed.
pub fn parse_internal_key(ikey: &[u8]) -> Option<ParsedInternalKey<'_>> {
    if ikey.len() < TRAILER_LEN {
        return None;
    }
    let split = ikey.len() - TRAILER_LEN;
    let trailer = decode_fixed64(&ikey[split..]);
    Some(ParsedInternalKey {
        user_key: &ikey[..split],
        sequence: trailer >> 8,
        value_type: ValueType::from_u8((trailer & 0xff) as u8)?,
    })
}

/// Extracts the user key. Malformed (too short) keys yield an empty slice
/// rather than panicking, so comparisons over corrupt data stay total.
#[inline]
pub fn user_key(ikey: &[u8]) -> &[u8] {
    &ikey[..ikey.len().saturating_sub(TRAILER_LEN)]
}

#[inline]
fn trailer(ikey: &[u8]) -> u64 {
    if ikey.len() < TRAILER_LEN {
        0
    } else {
        decode_fixed64(&ikey[ikey.len() - TRAILER_LEN..])
    }
}

/// Total order over internal keys: user key ascending, trailer descending.
pub fn compare_internal_keys(a: &[u8], b: &[u8]) -> Ordering {
    user_key(a)
        .cmp(user_key(b))
        .then_with(|| trailer(b).cmp(&trailer(a)))
}

/// Comparator signature used by blocks and iterators.
pub type KeyComparator = fn(&[u8], &[u8]) -> Ordering;

/// Returns a short internal key `k` with `start <= k < limit`, used as the
/// index-block separator between two data blocks. Shortening the user key
/// keeps the index block small.
pub fn shortest_separator(start: &[u8], limit: &[u8]) -> Vec<u8> {
    let (us, ul) = (user_key(start), user_key(limit));
    let common = us.iter().zip(ul).take_while(|(a, b)| a == b).count();
    if common < us.len().min(ul.len()) {
        let byte = us[common];
        if byte < 0xff && byte + 1 < ul[common] {
            let mut sep = us[..=common].to_vec();
            sep[common] += 1;
            debug_assert!(sep.as_slice() > us && sep.as_slice() < ul);
            return make_internal_key(&sep, MAX_SEQUENCE_NUMBER, VALUE_TYPE_FOR_SEEK);
        }
    }
    start.to_vec()
}

/// An owned internal key with the internal-key ordering, used as the
/// memtable's skiplist key.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct InternalKey(Vec<u8>);

impl InternalKey {
    /// Encodes `(user_key, seq, t)` into an owned internal key.
    pub fn new(user_key: &[u8], seq: SequenceNumber, t: ValueType) -> Self {
        InternalKey(make_internal_key(user_key, seq, t))
    }

    /// Wraps already-encoded internal key bytes.
    pub fn from_encoded(bytes: Vec<u8>) -> Self {
        InternalKey(bytes)
    }

    /// The encoded bytes.
    pub fn encoded(&self) -> &[u8] {
        &self.0
    }

    /// The user-key portion.
    pub fn user_key(&self) -> &[u8] {
        user_key(&self.0)
    }
}

impl Ord for InternalKey {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_internal_keys(&self.0, &other.0)
    }
}

impl PartialOrd for InternalKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for InternalKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match parse_internal_key(&self.0) {
            Some(p) => write!(
                f,
                "{:?}@{}:{:?}",
                String::from_utf8_lossy(p.user_key),
                p.sequence,
                p.value_type
            ),
            None => write!(f, "<bad internal key {:?}>", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn newer_sequence_sorts_first() {
        let a = make_internal_key(b"k", 10, ValueType::Value);
        let b = make_internal_key(b"k", 5, ValueType::Value);
        assert_eq!(compare_internal_keys(&a, &b), Ordering::Less);
        let c = make_internal_key(b"j", 1, ValueType::Value);
        assert_eq!(compare_internal_keys(&c, &a), Ordering::Less);
    }

    #[test]
    fn seek_key_precedes_entries_at_same_sequence() {
        let seek = make_internal_key(b"k", 7, VALUE_TYPE_FOR_SEEK);
        let del = make_internal_key(b"k", 7, ValueType::Deletion);
        assert_eq!(compare_internal_keys(&seek, &del), Ordering::Less);
    }

    #[test]
    fn parse_roundtrip_and_rejects_garbage() {
        let k = make_internal_key(b"hello", 42, ValueType::Deletion);
        let p = parse_internal_key(&k).unwrap();
        assert_eq!(p.user_key, b"hello");
        assert_eq!(p.sequence, 42);
        assert_eq!(p.value_type, ValueType::Deletion);
        assert!(parse_internal_key(b"short").is_none());
        let mut bad = k.clone();
        let n = bad.len();
        bad[n - 8] = 9; // invalid type byte
        assert!(parse_internal_key(&bad).is_none());
    }

    #[test]
    fn separator_is_shorter_when_possible() {
        let a = make_internal_key(b"abcdefg", 5, ValueType::Value);
        let b = make_internal_key(b"abzzz", 9, ValueType::Value);
        let s = shortest_separator(&a, &b);
        assert_eq!(user_key(&s), b"abd");
    }

    proptest! {
        #[test]
        fn separator_bounds(a in proptest::collection::vec(any::<u8>(), 0..12),
                            b in proptest::collection::vec(any::<u8>(), 0..12),
                            sa in 0u64..1000, sb in 0u64..1000) {
            let ka = make_internal_key(&a, sa, ValueType::Value);
            let kb = make_internal_key(&b, sb, ValueType::Value);
            let (lo, hi) = if compare_internal_keys(&ka, &kb) == Ordering::Less { (ka, kb) } else { (kb, ka) };
            prop_assume!(lo != hi);
            let s = shortest_separator(&lo, &hi);
            prop_assert!(compare_internal_keys(&lo, &s) != Ordering::Greater);
            prop_assert!(compare_internal_keys(&s, &hi) == Ordering::Less);
        }
    }
}
