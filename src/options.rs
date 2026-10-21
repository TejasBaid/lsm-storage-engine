use crate::db::Snapshot;
use crate::error::{Error, Result};

/// How strictly write-ahead-log recovery treats damaged records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WalRecoveryMode {
    /// Drop an incomplete or corrupt record at the *end* of a log (the
    /// signature of a crash mid-append), but fail on corruption anywhere
    /// else. This is the default.
    #[default]
    TolerateCorruptedTailRecords,
    /// Fail on any incomplete or corrupt record, including a torn tail.
    AbsoluteConsistency,
    /// Stop replay at the first bad record and discard everything after it,
    /// recovering a consistent prefix of the write history.
    PointInTimeRecovery,
}

/// Database-wide configuration. Construct with `Options::default()` and
/// override fields as needed.
#[derive(Debug, Clone)]
pub struct Options {
    /// Create the database directory if it does not exist. Default: `true`.
    pub create_if_missing: bool,
    /// Fail to open if the database already exists. Default: `false`.
    pub error_if_exists: bool,
    /// Memtable size that triggers a flush to level 0. Default: 4 MiB.
    pub write_buffer_size: usize,
    /// Number of levels in the LSM tree. Default: 7.
    pub max_levels: usize,
    /// Number of level-0 files that triggers a level-0 compaction. Default: 4.
    pub l0_compaction_trigger: usize,
    /// Number of level-0 files at which each write is delayed by 1 ms to let
    /// compaction catch up. Default: 8.
    pub l0_slowdown_writes_trigger: usize,
    /// Number of level-0 files at which writes stop until compaction catches
    /// up. Default: 12.
    pub l0_stop_writes_trigger: usize,
    /// Target total size of level 1; level `n` targets
    /// `base * multiplier^(n-1)`. Default: 10 MiB.
    pub max_bytes_for_level_base: u64,
    /// Growth factor between adjacent levels. Default: 10.
    pub level_size_multiplier: u64,
    /// Target size of SSTables written by compaction. Default: 2 MiB.
    pub target_file_size: u64,
    /// Approximate uncompressed size of each data block. Default: 4 KiB.
    pub block_size: usize,
    /// Number of keys between restart points in a block (prefix-compression
    /// resets). Default: 16.
    pub block_restart_interval: usize,
    /// Bloom filter bits per key; 0 disables filters. Default: 10 (~1% FPR).
    pub bloom_bits_per_key: usize,
    /// Capacity of the shared LRU block cache in bytes; 0 disables it.
    /// Default: 8 MiB.
    pub block_cache_capacity: usize,
    /// Maximum number of SSTables kept open in the table cache. Default: 500.
    pub max_open_files: usize,
    /// Verify block checksums on every read from disk. Default: `true`.
    pub verify_checksums: bool,
    /// Policy for damaged write-ahead-log records found during recovery.
    pub wal_recovery_mode: WalRecoveryMode,
    /// Rotate the manifest once it grows beyond this size. Default: 64 MiB.
    pub max_manifest_file_size: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            create_if_missing: true,
            error_if_exists: false,
            write_buffer_size: 4 << 20,
            max_levels: 7,
            l0_compaction_trigger: 4,
            l0_slowdown_writes_trigger: 8,
            l0_stop_writes_trigger: 12,
            max_bytes_for_level_base: 10 << 20,
            level_size_multiplier: 10,
            target_file_size: 2 << 20,
            block_size: 4 << 10,
            block_restart_interval: 16,
            bloom_bits_per_key: 10,
            block_cache_capacity: 8 << 20,
            max_open_files: 500,
            verify_checksums: true,
            wal_recovery_mode: WalRecoveryMode::default(),
            max_manifest_file_size: 64 << 20,
        }
    }
}

impl Options {
    /// Maximum total bytes allowed in `level` (levels >= 1) before it is
    /// eligible for compaction.
    pub fn max_bytes_for_level(&self, level: usize) -> u64 {
        debug_assert!(level >= 1);
        let mut bytes = self.max_bytes_for_level_base;
        for _ in 1..level {
            bytes = bytes.saturating_mul(self.level_size_multiplier);
        }
        bytes
    }

    /// Bytes of grandparent (level+2) overlap after which a compaction output
    /// file is cut, bounding the cost of the next compaction.
    pub(crate) fn max_grandparent_overlap_bytes(&self) -> u64 {
        self.target_file_size.saturating_mul(10)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let bad = |msg: &str| Err(Error::InvalidArgument(msg.to_string()));
        if self.max_levels < 2 {
            return bad("max_levels must be at least 2");
        }
        if self.write_buffer_size < 1024 {
            return bad("write_buffer_size must be at least 1 KiB");
        }
        if self.block_restart_interval == 0 {
            return bad("block_restart_interval must be positive");
        }
        if self.block_size == 0 || self.target_file_size == 0 {
            return bad("block_size and target_file_size must be positive");
        }
        if self.l0_compaction_trigger == 0
            || self.l0_slowdown_writes_trigger < self.l0_compaction_trigger
            || self.l0_stop_writes_trigger < self.l0_slowdown_writes_trigger
        {
            return bad("require 0 < l0_compaction_trigger <= l0_slowdown <= l0_stop");
        }
        if self.level_size_multiplier < 2 {
            return bad("level_size_multiplier must be at least 2");
        }
        if self.max_open_files < 1 {
            return bad("max_open_files must be at least 1");
        }
        Ok(())
    }
}

/// Per-write options.
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteOptions {
    /// `fsync` the write-ahead log before acknowledging the write.
    ///
    /// With `false` (the default) the record is handed to the OS before the
    /// call returns, so it survives a process crash but not a power failure.
    pub sync: bool,
}

/// Per-read options.
#[derive(Clone, Copy)]
pub struct ReadOptions<'a> {
    /// Read as of this snapshot instead of the latest state.
    pub snapshot: Option<&'a Snapshot>,
    /// Insert blocks read for this operation into the block cache. Disable
    /// for large one-off scans to avoid evicting the hot set.
    pub fill_cache: bool,
}

impl Default for ReadOptions<'_> {
    fn default() -> Self {
        ReadOptions {
            snapshot: None,
            fill_cache: true,
        }
    }
}

impl<'a> ReadOptions<'a> {
    /// Default read options pinned to `snapshot`.
    pub fn at_snapshot(snapshot: &'a Snapshot) -> Self {
        ReadOptions {
            snapshot: Some(snapshot),
            ..Default::default()
        }
    }
}
