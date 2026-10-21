//! Version edits: the delta records stored in the manifest.
//!
//! ```text
//! edit  := field*
//! field := varint tag | payload
//!   1 comparator       length-prefixed name
//!   2 log_number       varint
//!   3 next_file_number varint
//!   4 last_sequence    varint
//!   5 compact_pointer  varint level | length-prefixed internal key
//!   6 deleted_file     varint level | varint file number
//!   7 new_file         varint level | varint number | varint size
//!                      | length-prefixed smallest | length-prefixed largest
//! ```

use std::collections::BTreeSet;

use crate::coding::{Decoder, put_length_prefixed_slice, put_varint64};
use crate::error::{Error, Result};
use crate::key::{SequenceNumber, user_key};

/// Comparator name recorded in the manifest to prevent opening a database
/// with an incompatible key order.
pub const COMPARATOR_NAME: &str = "lsm-engine.BytewiseComparator";

/// Metadata for one SSTable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMetaData {
    /// File number (`NNNNNN.sst`).
    pub number: u64,
    /// File size in bytes.
    pub file_size: u64,
    /// Smallest internal key in the table.
    pub smallest: Vec<u8>,
    /// Largest internal key in the table.
    pub largest: Vec<u8>,
}

impl FileMetaData {
    /// Smallest user key.
    pub fn smallest_user_key(&self) -> &[u8] {
        user_key(&self.smallest)
    }

    /// Largest user key.
    pub fn largest_user_key(&self) -> &[u8] {
        user_key(&self.largest)
    }
}

/// An atomic change to the set of live files plus counter updates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionEdit {
    /// Comparator name (written in the first record of each manifest).
    pub comparator: Option<String>,
    /// WALs with numbers below this are obsolete.
    pub log_number: Option<u64>,
    /// Next unused file number.
    pub next_file_number: Option<u64>,
    /// Last sequence number covered by persisted state.
    pub last_sequence: Option<SequenceNumber>,
    /// Round-robin compaction cursors per level.
    pub compact_pointers: Vec<(usize, Vec<u8>)>,
    /// Files removed, as `(level, number)`.
    pub deleted_files: BTreeSet<(usize, u64)>,
    /// Files added, as `(level, metadata)`.
    pub new_files: Vec<(usize, FileMetaData)>,
}

const TAG_COMPARATOR: u64 = 1;
const TAG_LOG_NUMBER: u64 = 2;
const TAG_NEXT_FILE_NUMBER: u64 = 3;
const TAG_LAST_SEQUENCE: u64 = 4;
const TAG_COMPACT_POINTER: u64 = 5;
const TAG_DELETED_FILE: u64 = 6;
const TAG_NEW_FILE: u64 = 7;

/// Sanity bound on level numbers read from disk.
const MAX_ENCODED_LEVEL: u64 = 64;

impl VersionEdit {
    /// Records the addition of a file at `level`.
    pub fn add_file(&mut self, level: usize, meta: FileMetaData) {
        self.new_files.push((level, meta));
    }

    /// Records the removal of file `number` from `level`.
    pub fn delete_file(&mut self, level: usize, number: u64) {
        self.deleted_files.insert((level, number));
    }

    /// Serializes the edit.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(c) = &self.comparator {
            put_varint64(&mut out, TAG_COMPARATOR);
            put_length_prefixed_slice(&mut out, c.as_bytes());
        }
        for (tag, value) in [
            (TAG_LOG_NUMBER, self.log_number),
            (TAG_NEXT_FILE_NUMBER, self.next_file_number),
            (TAG_LAST_SEQUENCE, self.last_sequence),
        ] {
            if let Some(v) = value {
                put_varint64(&mut out, tag);
                put_varint64(&mut out, v);
            }
        }
        for (level, key) in &self.compact_pointers {
            put_varint64(&mut out, TAG_COMPACT_POINTER);
            put_varint64(&mut out, *level as u64);
            put_length_prefixed_slice(&mut out, key);
        }
        for &(level, number) in &self.deleted_files {
            put_varint64(&mut out, TAG_DELETED_FILE);
            put_varint64(&mut out, level as u64);
            put_varint64(&mut out, number);
        }
        for (level, f) in &self.new_files {
            put_varint64(&mut out, TAG_NEW_FILE);
            put_varint64(&mut out, *level as u64);
            put_varint64(&mut out, f.number);
            put_varint64(&mut out, f.file_size);
            put_length_prefixed_slice(&mut out, &f.smallest);
            put_length_prefixed_slice(&mut out, &f.largest);
        }
        out
    }

    /// Parses an edit, rejecting unknown tags and out-of-range fields.
    pub fn decode(src: &[u8]) -> Result<Self> {
        let mut d = Decoder::new(src);
        let mut edit = VersionEdit::default();
        let level = |d: &mut Decoder<'_>| -> Result<usize> {
            let l = d.varint64()?;
            if l >= MAX_ENCODED_LEVEL {
                return Err(Error::corruption(format!(
                    "level {l} out of range in version edit"
                )));
            }
            Ok(l as usize)
        };
        while !d.is_empty() {
            match d.varint64()? {
                TAG_COMPARATOR => {
                    let name = std::str::from_utf8(d.length_prefixed()?)
                        .map_err(|_| Error::corruption("comparator name is not UTF-8"))?;
                    edit.comparator = Some(name.to_string());
                }
                TAG_LOG_NUMBER => edit.log_number = Some(d.varint64()?),
                TAG_NEXT_FILE_NUMBER => edit.next_file_number = Some(d.varint64()?),
                TAG_LAST_SEQUENCE => edit.last_sequence = Some(d.varint64()?),
                TAG_COMPACT_POINTER => {
                    let l = level(&mut d)?;
                    edit.compact_pointers
                        .push((l, d.length_prefixed()?.to_vec()));
                }
                TAG_DELETED_FILE => {
                    let l = level(&mut d)?;
                    edit.deleted_files.insert((l, d.varint64()?));
                }
                TAG_NEW_FILE => {
                    let l = level(&mut d)?;
                    let meta = FileMetaData {
                        number: d.varint64()?,
                        file_size: d.varint64()?,
                        smallest: d.length_prefixed()?.to_vec(),
                        largest: d.length_prefixed()?.to_vec(),
                    };
                    edit.new_files.push((l, meta));
                }
                tag => return Err(Error::corruption(format!("unknown version edit tag {tag}"))),
            }
        }
        Ok(edit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{ValueType, make_internal_key};
    use proptest::prelude::*;

    #[test]
    fn roundtrip_full_edit() {
        let mut e = VersionEdit {
            comparator: Some(COMPARATOR_NAME.into()),
            log_number: Some(7),
            next_file_number: Some(100),
            last_sequence: Some(1 << 40),
            ..Default::default()
        };
        e.compact_pointers
            .push((2, make_internal_key(b"m", 5, ValueType::Value)));
        e.delete_file(1, 42);
        e.delete_file(0, 41);
        e.add_file(
            3,
            FileMetaData {
                number: 99,
                file_size: 123_456,
                smallest: make_internal_key(b"a", 1, ValueType::Value),
                largest: make_internal_key(b"z", 9, ValueType::Deletion),
            },
        );
        assert_eq!(VersionEdit::decode(&e.encode()).unwrap(), e);
    }

    #[test]
    fn rejects_unknown_tag_and_bad_level() {
        assert!(VersionEdit::decode(&[99]).is_err());
        let mut buf = Vec::new();
        put_varint64(&mut buf, TAG_DELETED_FILE);
        put_varint64(&mut buf, 1000);
        put_varint64(&mut buf, 1);
        assert!(VersionEdit::decode(&buf).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..128)) {
            let _ = VersionEdit::decode(&data);
        }
    }
}
