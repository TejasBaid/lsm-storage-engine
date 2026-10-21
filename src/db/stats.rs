use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

/// Internal counters, updated lock-free on the hot paths.
pub(crate) struct Stats {
    pub user_bytes: AtomicU64,
    pub wal_bytes: AtomicU64,
    pub writes: AtomicU64,
    pub flush_bytes: AtomicU64,
    pub flushes: AtomicU64,
    pub compaction_bytes_read: AtomicU64,
    pub compaction_bytes_written: AtomicU64,
    pub compactions: AtomicU64,
    pub trivial_moves: AtomicU64,
    pub stall_micros: AtomicU64,
    pub level_bytes_written: Vec<AtomicU64>,
    pub level_bytes_read: Vec<AtomicU64>,
    pub level_compactions: Vec<AtomicU64>,
    pub recovery: Mutex<RecoveryStats>,
}

impl Stats {
    pub fn new(levels: usize) -> Self {
        let zeros = || (0..levels).map(|_| AtomicU64::new(0)).collect();
        Stats {
            user_bytes: AtomicU64::new(0),
            wal_bytes: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            flush_bytes: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            compaction_bytes_read: AtomicU64::new(0),
            compaction_bytes_written: AtomicU64::new(0),
            compactions: AtomicU64::new(0),
            trivial_moves: AtomicU64::new(0),
            stall_micros: AtomicU64::new(0),
            level_bytes_written: zeros(),
            level_bytes_read: zeros(),
            level_compactions: zeros(),
            recovery: Mutex::new(RecoveryStats::default()),
        }
    }

    pub fn add(counter: &AtomicU64, v: u64) {
        counter.fetch_add(v, Ordering::Relaxed);
    }

    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

/// What happened while replaying write-ahead logs at open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryStats {
    /// Number of log files replayed.
    pub logs_replayed: usize,
    /// Number of write batches replayed.
    pub batches_replayed: u64,
    /// Bytes discarded because of torn or corrupt records.
    pub bytes_dropped: u64,
    /// Level-0 tables written while replaying.
    pub tables_written: usize,
}

/// Per-level shape and compaction activity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LevelStats {
    /// Level number.
    pub level: usize,
    /// Number of live tables.
    pub files: usize,
    /// Total size of live tables.
    pub bytes: u64,
    /// Compactions whose output landed in this level.
    pub compactions: u64,
    /// Bytes read by those compactions.
    pub bytes_read: u64,
    /// Bytes written into this level (flushes for L0, compactions otherwise).
    pub bytes_written: u64,
}

/// A point-in-time snapshot of engine statistics.
#[derive(Debug, Clone, Default)]
pub struct DbStats {
    /// Sum of key and value bytes submitted by callers.
    pub user_bytes_written: u64,
    /// Bytes appended to write-ahead logs.
    pub wal_bytes_written: u64,
    /// Number of write batches committed.
    pub writes: u64,
    /// Bytes written by memtable flushes.
    pub flush_bytes_written: u64,
    /// Number of memtable flushes.
    pub flushes: u64,
    /// Bytes read by compactions.
    pub compaction_bytes_read: u64,
    /// Bytes written by compactions.
    pub compaction_bytes_written: u64,
    /// Number of merging compactions.
    pub compactions: u64,
    /// Number of compactions satisfied by relinking a file one level down.
    pub trivial_moves: u64,
    /// Total time writers spent stalled waiting on flushes/compactions.
    pub write_stall_micros: u64,
    /// Block cache hits.
    pub block_cache_hits: u64,
    /// Block cache misses.
    pub block_cache_misses: u64,
    /// Table probes skipped because the bloom filter excluded the key.
    pub bloom_filter_negatives: u64,
    /// Latest committed sequence number.
    pub last_sequence: u64,
    /// Per-level breakdown.
    pub levels: Vec<LevelStats>,
    /// Results of WAL replay at open.
    pub recovery: RecoveryStats,
}

impl DbStats {
    /// SSTable bytes written (flush + compaction) per user byte.
    pub fn write_amplification(&self) -> f64 {
        ratio(
            self.flush_bytes_written + self.compaction_bytes_written,
            self.user_bytes_written,
        )
    }

    /// Like [`DbStats::write_amplification`] but also counting WAL bytes.
    pub fn write_amplification_with_wal(&self) -> f64 {
        ratio(
            self.wal_bytes_written + self.flush_bytes_written + self.compaction_bytes_written,
            self.user_bytes_written,
        )
    }
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}

fn mib(b: u64) -> f64 {
    b as f64 / (1u64 << 20) as f64
}

impl fmt::Display for DbStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Level  Files    Size(MiB)  Compactions  Read(MiB)  Write(MiB)"
        )?;
        for l in &self.levels {
            writeln!(
                f,
                "L{:<5} {:>5} {:>12.2} {:>12} {:>10.2} {:>11.2}",
                l.level,
                l.files,
                mib(l.bytes),
                l.compactions,
                mib(l.bytes_read),
                mib(l.bytes_written)
            )?;
        }
        writeln!(
            f,
            "user writes: {} batches, {:.2} MiB; WAL {:.2} MiB; flush {:.2} MiB ({} flushes)",
            self.writes,
            mib(self.user_bytes_written),
            mib(self.wal_bytes_written),
            mib(self.flush_bytes_written),
            self.flushes
        )?;
        writeln!(
            f,
            "compaction: {} merges, {} trivial moves, read {:.2} MiB, wrote {:.2} MiB",
            self.compactions,
            self.trivial_moves,
            mib(self.compaction_bytes_read),
            mib(self.compaction_bytes_written)
        )?;
        writeln!(
            f,
            "write amplification: {:.2}x (SST only), {:.2}x (incl. WAL)",
            self.write_amplification(),
            self.write_amplification_with_wal()
        )?;
        writeln!(
            f,
            "block cache: {} hits / {} misses; bloom filter skipped {} table probes; stalls {:.1} ms",
            self.block_cache_hits,
            self.block_cache_misses,
            self.bloom_filter_negatives,
            self.write_stall_micros as f64 / 1000.0
        )?;
        write!(
            f,
            "recovery: {} logs, {} batches replayed, {} bytes dropped",
            self.recovery.logs_replayed,
            self.recovery.batches_replayed,
            self.recovery.bytes_dropped
        )
    }
}
