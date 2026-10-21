//! # lsm-engine
//!
//! An embeddable, crash-safe key-value storage engine built on a
//! log-structured merge tree, in the spirit of LevelDB and RocksDB.
//!
//! * **Durability** — every write batch is appended to a CRC32-checksummed
//!   write-ahead log before it is applied. Recovery replays the log and
//!   tolerates a torn final record.
//! * **MVCC** — every entry carries a 56-bit sequence number, so readers can
//!   pin a consistent [`Snapshot`] while writers continue.
//! * **Leveled compaction** — a background thread flushes memtables into
//!   sorted string tables (SSTables) and merges them down a configurable
//!   number of levels. Metadata changes are committed atomically through a
//!   manifest log.
//! * **Read path** — bloom filters skip tables that cannot contain a key,
//!   and a sharded LRU block cache keeps hot blocks in memory.
//!
//! ```
//! use lsm_engine::{Db, Options, WriteBatch};
//!
//! # fn main() -> lsm_engine::Result<()> {
//! let dir = tempfile::tempdir().unwrap();
//! let db = Db::open(dir.path(), Options::default())?;
//!
//! db.put(b"apple", b"red")?;
//! db.put(b"banana", b"yellow")?;
//! assert_eq!(db.get(b"apple")?, Some(b"red".to_vec()));
//!
//! // Atomic multi-key update.
//! let mut batch = WriteBatch::new();
//! batch.delete(b"apple");
//! batch.put(b"cherry", b"dark red");
//! db.write(batch)?;
//!
//! // Ordered range scan.
//! let keys: Vec<Vec<u8>> = db
//!     .range(b"a".to_vec()..b"c".to_vec())
//!     .map(|kv| kv.map(|(k, _)| k))
//!     .collect::<lsm_engine::Result<_>>()?;
//! assert_eq!(keys, vec![b"banana".to_vec()]);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod batch;
mod cache;
mod coding;
mod db;
mod error;
mod filename;
mod iterator;
mod key;
mod memtable;
mod options;
mod sstable;
mod table_cache;
mod version;
mod wal;

pub use batch::WriteBatch;
pub use db::{Db, DbIterator, DbStats, LevelStats, RangeIter, RecoveryStats, Snapshot};
pub use error::{Error, Result};
pub use options::{Options, ReadOptions, WalRecoveryMode, WriteOptions};

/// Low-level building blocks, exposed for the CLI's inspection commands,
/// benchmarks and fuzz targets. Not covered by semver guarantees.
#[doc(hidden)]
pub mod internal {
    pub use crate::batch::BatchOp;
    pub use crate::cache::LruCache;
    pub use crate::coding::{decode_varint64, put_varint64};
    pub use crate::filename::{FileType, parse_file_name};
    pub use crate::iterator::{InternalIterator, MergingIterator};
    pub use crate::key::{
        InternalKey, MAX_SEQUENCE_NUMBER, ParsedInternalKey, ValueType, compare_internal_keys,
        make_internal_key, parse_internal_key,
    };
    pub use crate::memtable::MemTable;
    pub use crate::sstable::{
        Block, BlockBuilder, BlockHandle, BlockIterator, BloomFilterPolicy, Footer, Table,
        TableBuilder, bloom_hash,
    };
    pub use crate::version::edit::{FileMetaData, VersionEdit};
    pub use crate::wal::{LogReader, LogWriter};
}
