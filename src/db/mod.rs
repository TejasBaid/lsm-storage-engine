//! The database: write path, read path, recovery and lifecycle.
//!
//! # Concurrency model
//!
//! * `writer` (mutex) serializes writers: it owns the active WAL and is
//!   held while a batch is assigned sequence numbers, logged, and inserted
//!   into the memtable.
//! * `state` (mutex) guards the memtable pointers, the version set and
//!   background-work bookkeeping. It is held only briefly; no I/O-heavy work
//!   (table building, compaction merging) happens under it.
//! * Lock order is always `writer` -> `state`. The background thread only
//!   ever takes `state`.
//! * A write becomes visible when `last_sequence` is advanced past it, which
//!   happens only after every entry of its batch is in the memtable.
//! * Readers capture `(last_sequence, mem, imm, current version)` atomically
//!   under `state`, then read without any lock.

mod background;
mod iter;
mod snapshot;
mod stats;

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, MutexGuard};

pub use iter::{DbIterator, RangeIter};
pub use snapshot::Snapshot;
pub use stats::{DbStats, LevelStats, RecoveryStats};

use crate::batch::WriteBatch;
use crate::cache::LruCache;
use crate::error::{Error, Result};
use crate::filename::{
    FileType, current_file_name, lock_file_name, log_file_name, parse_file_name, sync_dir,
    table_file_name,
};
use crate::iterator::{BoxedIterator, InternalIterator, MergingIterator};
use crate::key::{
    LookupResult, MAX_SEQUENCE_NUMBER, SequenceNumber, VALUE_TYPE_FOR_SEEK, make_internal_key,
};
use crate::memtable::MemTable;
use crate::options::{Options, ReadOptions, WalRecoveryMode, WriteOptions};
use crate::sstable::TableBuilder;
use crate::table_cache::TableCache;
use crate::version::edit::{FileMetaData, VersionEdit};
use crate::version::{Version, VersionSet};
use crate::wal::{HEADER_SIZE, LogReader, LogWriter};
use snapshot::SnapshotList;
use stats::Stats;

/// State guarded by the `state` mutex.
pub(crate) struct DbState {
    mem: Arc<MemTable>,
    imm: Option<Arc<MemTable>>,
    /// WAL number backing `mem`; older logs only hold `imm` data.
    mem_log_number: u64,
    versions: VersionSet,
    /// Sticky error from background work or a failed WAL write. Once set,
    /// the database rejects writes.
    bg_error: Option<Error>,
    manual: Option<ManualCompaction>,
    /// Tables being written that are not yet in any version.
    pending_outputs: HashSet<u64>,
}

struct ManualCompaction {
    level: usize,
    begin: Option<Vec<u8>>,
    end: Option<Vec<u8>>,
}

struct WriterState {
    log: LogWriter,
    /// Whether the directory entry of `log` is known durable. Synced lazily
    /// on the first `sync = true` write, so unsynced workloads never pay it.
    log_dir_synced: bool,
}

pub(crate) struct DbInner {
    dir: PathBuf,
    opts: Options,
    table_cache: Arc<TableCache>,
    state: Mutex<DbState>,
    writer: Mutex<WriterState>,
    manual_lock: Mutex<()>,
    /// Signals the background thread that work may be available.
    bg_work_cv: Condvar,
    /// Signals waiters that a unit of background work finished.
    bg_done_cv: Condvar,
    last_sequence: AtomicU64,
    snapshots: Arc<SnapshotList>,
    has_imm: AtomicBool,
    shutting_down: AtomicBool,
    stats: Stats,
    _lock_file: File,
}

/// An open LSM-tree database. `Db` is `Send + Sync`; share it across
/// threads with an `Arc`.
pub struct Db {
    inner: Arc<DbInner>,
    bg_thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for Db {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Db").field("dir", &self.inner.dir).finish()
    }
}

/// Writes the entries of `iter` into table `number`. Returns `None` (and
/// creates no file) if the iterator is empty.
pub(crate) fn build_table(
    dir: &Path,
    opts: &Options,
    iter: &mut dyn InternalIterator,
    number: u64,
) -> Result<Option<FileMetaData>> {
    let path = table_file_name(dir, number);
    let result = (|| {
        iter.seek_to_first()?;
        if !iter.valid() {
            return Ok(None);
        }
        let mut builder = TableBuilder::new(File::create(&path)?, opts);
        let smallest = iter.key().to_vec();
        let mut largest = Vec::new();
        while iter.valid() {
            builder.add(iter.key(), iter.value())?;
            largest.clear();
            largest.extend_from_slice(iter.key());
            iter.next()?;
        }
        let file_size = builder.finish()?;
        Ok(Some(FileMetaData {
            number,
            file_size,
            smallest,
            largest,
        }))
    })();
    if !matches!(result, Ok(Some(_))) {
        let _ = fs::remove_file(&path);
    }
    result
}

fn list_db_files(dir: &Path) -> Result<Vec<(FileType, u64)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(parsed) = entry.file_name().to_str().and_then(parse_file_name) {
            out.push(parsed);
        }
    }
    Ok(out)
}

/// Result of replaying the WALs at open.
struct Recovered {
    log: LogWriter,
    log_number: u64,
    last_sequence: SequenceNumber,
    stats: RecoveryStats,
}

/// Replays every WAL not yet covered by the manifest into level-0 tables,
/// starts a fresh WAL and commits the result in one version edit.
fn recover_logs(
    dir: &Path,
    opts: &Options,
    versions: &mut VersionSet,
    stats: &Stats,
) -> Result<Recovered> {
    let min_log = versions.log_number();
    let mut logs: Vec<u64> = list_db_files(dir)?
        .into_iter()
        .filter_map(|(t, n)| (t == FileType::Log && n >= min_log).then_some(n))
        .collect();
    logs.sort_unstable();
    for &n in &logs {
        versions.mark_file_number_used(n);
    }

    let mut edit = VersionEdit::default();
    let mut max_seq = versions.last_sequence;
    let mut mem = Arc::new(MemTable::new());
    let mut rs = RecoveryStats::default();
    let flush = |mem: &Arc<MemTable>, versions: &mut VersionSet, edit: &mut VersionEdit| {
        let number = versions.new_file_number();
        if let Some(meta) = build_table(dir, opts, &mut mem.iter(), number)? {
            Stats::add(&stats.flush_bytes, meta.file_size);
            Stats::add(&stats.level_bytes_written[0], meta.file_size);
            edit.add_file(0, meta);
        }
        Ok::<_, Error>(())
    };

    for &n in &logs {
        let data = fs::read(log_file_name(dir, n))?;
        let mut reader = LogReader::new(data, opts.wal_recovery_mode);
        while let Some(record) = reader.next_record()? {
            let batch = WriteBatch::from_data(record)?;
            if batch.is_empty() {
                continue;
            }
            batch.insert_into(&mem)?;
            max_seq = max_seq.max(batch.sequence() + u64::from(batch.count()) - 1);
            rs.batches_replayed += 1;
            if mem.approximate_size() > opts.write_buffer_size {
                flush(&mem, versions, &mut edit)?;
                rs.tables_written += 1;
                mem = Arc::new(MemTable::new());
            }
        }
        rs.logs_replayed += 1;
        rs.bytes_dropped += reader.dropped_bytes();
        if reader.stopped_on_corruption()
            && opts.wal_recovery_mode == WalRecoveryMode::PointInTimeRecovery
        {
            // Later logs would be a non-contiguous suffix of history.
            break;
        }
    }
    if !mem.is_empty() {
        flush(&mem, versions, &mut edit)?;
        rs.tables_written += 1;
    }

    let log_number = versions.new_file_number();
    let log = LogWriter::new(File::create(log_file_name(dir, log_number))?);
    edit.log_number = Some(log_number);
    versions.last_sequence = max_seq;
    versions.log_and_apply(&mut edit)?;
    Ok(Recovered {
        log,
        log_number,
        last_sequence: max_seq,
        stats: rs,
    })
}

impl Db {
    /// Opens (or creates) the database in directory `path`, replaying any
    /// write-ahead logs left by a previous run.
    pub fn open(path: impl AsRef<Path>, opts: Options) -> Result<Db> {
        opts.validate()?;
        let dir = path.as_ref().to_path_buf();
        if opts.create_if_missing {
            fs::create_dir_all(&dir)?;
        } else if !dir.is_dir() {
            return Err(Error::InvalidArgument(format!(
                "{} does not exist (create_if_missing is false)",
                dir.display()
            )));
        }

        let lock_file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_file_name(&dir))?;
        lock_file.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => {
                Error::InvalidArgument(format!("{} is locked by another process", dir.display()))
            }
            TryLockError::Error(e) => e.into(),
        })?;

        if !current_file_name(&dir).exists() {
            if !opts.create_if_missing {
                return Err(Error::InvalidArgument(format!(
                    "no database in {} (create_if_missing is false)",
                    dir.display()
                )));
            }
            VersionSet::create_new_db(&dir)?;
        } else if opts.error_if_exists {
            return Err(Error::InvalidArgument(format!(
                "database already exists in {}",
                dir.display()
            )));
        }

        let block_cache = (opts.block_cache_capacity > 0)
            .then(|| Arc::new(LruCache::new(opts.block_cache_capacity, 16)));
        let table_cache = Arc::new(TableCache::new(
            dir.clone(),
            opts.max_open_files,
            block_cache,
            opts.verify_checksums,
        ));
        let mut versions = VersionSet::new(dir.clone(), opts.clone());
        versions.recover()?;
        let stats = Stats::new(opts.max_levels);
        let recovered = recover_logs(&dir, &opts, &mut versions, &stats)?;
        *stats.recovery.lock() = recovered.stats;

        let inner = Arc::new(DbInner {
            dir,
            table_cache,
            state: Mutex::new(DbState {
                mem: Arc::new(MemTable::new()),
                imm: None,
                mem_log_number: recovered.log_number,
                versions,
                bg_error: None,
                manual: None,
                pending_outputs: HashSet::new(),
            }),
            writer: Mutex::new(WriterState {
                log: recovered.log,
                log_dir_synced: false,
            }),
            manual_lock: Mutex::new(()),
            bg_work_cv: Condvar::new(),
            bg_done_cv: Condvar::new(),
            last_sequence: AtomicU64::new(recovered.last_sequence),
            snapshots: Arc::new(SnapshotList::default()),
            has_imm: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            stats,
            opts,
            _lock_file: lock_file,
        });
        inner.delete_obsolete_files();

        let bg_inner = Arc::clone(&inner);
        let bg_thread = thread::Builder::new()
            .name("lsm-background".into())
            .spawn(move || bg_inner.background_loop())?;
        Ok(Db {
            inner,
            bg_thread: Some(bg_thread),
        })
    }

    /// The database directory.
    pub fn path(&self) -> &Path {
        &self.inner.dir
    }

    /// Sets `key` to `value`.
    pub fn put(&self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        let mut batch = WriteBatch::new();
        batch.put(key, value);
        self.write(batch)
    }

    /// Deletes `key` (writes a tombstone). Deleting a missing key is not an
    /// error.
    pub fn delete(&self, key: impl AsRef<[u8]>) -> Result<()> {
        let mut batch = WriteBatch::new();
        batch.delete(key);
        self.write(batch)
    }

    /// Applies `batch` atomically with default write options.
    pub fn write(&self, batch: WriteBatch) -> Result<()> {
        self.write_opt(batch, &WriteOptions::default())
    }

    /// Applies `batch` atomically: after a crash either all or none of its
    /// operations are recovered, and readers never observe a partial batch.
    pub fn write_opt(&self, mut batch: WriteBatch, opts: &WriteOptions) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let inner = &*self.inner;
        let mut w = inner.writer.lock();
        let mem = inner.make_room_for_write(&mut w, false)?;

        let count = u64::from(batch.count());
        let first = inner.last_sequence.load(Ordering::Relaxed) + 1;
        if first + count > MAX_SEQUENCE_NUMBER {
            return Err(Error::InvalidArgument(
                "sequence number space exhausted".into(),
            ));
        }
        batch.set_sequence(first);

        let logged = w.log.add_record(batch.data()).and_then(|()| {
            if !opts.sync {
                return Ok(());
            }
            w.log.sync()?;
            if !w.log_dir_synced {
                // A synced record is only durable if the file's directory
                // entry is too.
                sync_dir(&inner.dir)?;
                w.log_dir_synced = true;
            }
            Ok(())
        });
        if let Err(e) = logged {
            // The log may end in a partial record; appending after it would
            // turn a recoverable torn tail into mid-log corruption.
            inner.state.lock().bg_error.get_or_insert_with(|| e.clone());
            return Err(e);
        }
        batch.insert_into(&mem)?;
        inner
            .last_sequence
            .store(first + count - 1, Ordering::Release);

        Stats::add(&inner.stats.writes, 1);
        Stats::add(&inner.stats.user_bytes, batch.user_bytes());
        Stats::add(
            &inner.stats.wal_bytes,
            (batch.approximate_size() + HEADER_SIZE) as u64,
        );
        Ok(())
    }

    /// Returns the current value of `key`, if any.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        self.get_opt(key, &ReadOptions::default())
    }

    /// Returns the value of `key` as of `opts.snapshot` (or the latest state).
    pub fn get_opt(
        &self,
        key: impl AsRef<[u8]>,
        opts: &ReadOptions<'_>,
    ) -> Result<Option<Vec<u8>>> {
        let key = key.as_ref();
        let (seq, mem, imm, version) = self.inner.read_view(opts);
        let found = match mem.get(key, seq) {
            Some(r) => Some(r),
            None => match imm.as_ref().and_then(|m| m.get(key, seq)) {
                Some(r) => Some(r),
                None => version.get(
                    &self.inner.table_cache,
                    &make_internal_key(key, seq, VALUE_TYPE_FOR_SEEK),
                    opts.fill_cache,
                )?,
            },
        };
        Ok(match found {
            Some(LookupResult::Value(v)) => Some(v),
            Some(LookupResult::Deleted) | None => None,
        })
    }

    /// Returns an unpositioned iterator over the latest state; call
    /// [`DbIterator::seek_to_first`] or [`DbIterator::seek`] before use.
    pub fn iter(&self) -> Result<DbIterator> {
        self.iter_opt(&ReadOptions::default())
    }

    /// Returns an unpositioned iterator honoring `opts`.
    pub fn iter_opt(&self, opts: &ReadOptions<'_>) -> Result<DbIterator> {
        let (seq, mem, imm, version) = self.inner.read_view(opts);
        let mut children: Vec<BoxedIterator> = vec![Box::new(mem.iter())];
        if let Some(imm) = imm {
            children.push(Box::new(imm.iter()));
        }
        children.extend(version.iterators(&self.inner.table_cache, opts.fill_cache)?);
        Ok(DbIterator::new(
            MergingIterator::new(children),
            seq,
            version,
        ))
    }

    /// Iterates over `(key, value)` pairs whose keys fall in `range`.
    ///
    /// ```
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let db = lsm_engine::Db::open(dir.path(), Default::default()).unwrap();
    /// for kv in db.range(b"a".to_vec()..=b"m".to_vec()) {
    ///     let (key, value) = kv.unwrap();
    /// #   let _ = (key, value);
    /// }
    /// ```
    pub fn range<K: AsRef<[u8]>, R: RangeBounds<K>>(&self, range: R) -> RangeIter {
        self.range_opt(range, &ReadOptions::default())
    }

    /// Like [`Db::range`] with explicit read options.
    pub fn range_opt<K: AsRef<[u8]>, R: RangeBounds<K>>(
        &self,
        range: R,
        opts: &ReadOptions<'_>,
    ) -> RangeIter {
        let own = |b: Bound<&K>| match b {
            Bound::Included(k) => Bound::Included(k.as_ref().to_vec()),
            Bound::Excluded(k) => Bound::Excluded(k.as_ref().to_vec()),
            Bound::Unbounded => Bound::Unbounded,
        };
        RangeIter::new(
            self.iter_opt(opts),
            own(range.start_bound()),
            own(range.end_bound()),
        )
    }

    /// Takes a consistent snapshot of the current state.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot::new(&self.inner.snapshots, &self.inner.last_sequence)
    }

    /// Flushes the memtable to a level-0 table and waits for completion.
    pub fn flush(&self) -> Result<()> {
        let inner = &*self.inner;
        {
            let mut w = inner.writer.lock();
            inner.make_room_for_write(&mut w, true)?;
        }
        let mut st = inner.state.lock();
        while st.imm.is_some() && st.bg_error.is_none() {
            inner.bg_done_cv.wait(&mut st);
        }
        st.bg_error.clone().map_or(Ok(()), Err)
    }

    /// Compacts all data overlapping user keys `[begin, end]` (`None` =
    /// unbounded) down to the deepest level that holds such data, dropping
    /// shadowed versions and tombstones not needed by live snapshots.
    pub fn compact_range(&self, begin: Option<&[u8]>, end: Option<&[u8]>) -> Result<()> {
        self.flush()?;
        let inner = &*self.inner;
        let _manual = inner.manual_lock.lock();
        let max_level = {
            let st = inner.state.lock();
            let v = st.versions.current();
            (1..v.num_levels())
                .filter(|&l| !v.overlapping_inputs(l, begin, end).is_empty())
                .max()
                .unwrap_or(1)
        };
        for level in 0..max_level {
            let mut st = inner.state.lock();
            if let Some(e) = &st.bg_error {
                return Err(e.clone());
            }
            st.manual = Some(ManualCompaction {
                level,
                begin: begin.map(<[u8]>::to_vec),
                end: end.map(<[u8]>::to_vec),
            });
            inner.bg_work_cv.notify_all();
            while st.manual.is_some() && st.bg_error.is_none() {
                inner.bg_done_cv.wait(&mut st);
            }
            if let Some(e) = &st.bg_error {
                return Err(e.clone());
            }
        }
        Ok(())
    }

    /// Blocks until no flush or automatic compaction is pending. Useful for
    /// benchmarks and tests that want a quiescent tree.
    pub fn wait_for_compactions(&self) -> Result<()> {
        let inner = &*self.inner;
        let mut st = inner.state.lock();
        while (st.imm.is_some() || st.versions.needs_compaction()) && st.bg_error.is_none() {
            inner.bg_work_cv.notify_all();
            inner.bg_done_cv.wait(&mut st);
        }
        st.bg_error.clone().map_or(Ok(()), Err)
    }

    /// Current statistics.
    pub fn stats(&self) -> DbStats {
        let inner = &*self.inner;
        let version = inner.state.lock().versions.current();
        let s = &inner.stats;
        let (block_cache_hits, block_cache_misses) = inner
            .table_cache
            .block_cache()
            .map_or((0, 0), |c| c.hit_stats());
        DbStats {
            user_bytes_written: Stats::get(&s.user_bytes),
            wal_bytes_written: Stats::get(&s.wal_bytes),
            writes: Stats::get(&s.writes),
            flush_bytes_written: Stats::get(&s.flush_bytes),
            flushes: Stats::get(&s.flushes),
            compaction_bytes_read: Stats::get(&s.compaction_bytes_read),
            compaction_bytes_written: Stats::get(&s.compaction_bytes_written),
            compactions: Stats::get(&s.compactions),
            trivial_moves: Stats::get(&s.trivial_moves),
            write_stall_micros: Stats::get(&s.stall_micros),
            block_cache_hits,
            block_cache_misses,
            bloom_filter_negatives: inner.table_cache.bloom_negatives(),
            last_sequence: inner.last_sequence.load(Ordering::Acquire),
            levels: (0..version.num_levels())
                .map(|level| LevelStats {
                    level,
                    files: version.num_files(level),
                    bytes: version.level_bytes(level),
                    compactions: Stats::get(&s.level_compactions[level]),
                    bytes_read: Stats::get(&s.level_bytes_read[level]),
                    bytes_written: Stats::get(&s.level_bytes_written[level]),
                })
                .collect(),
            recovery: s.recovery.lock().clone(),
        }
    }

    /// One-line summary of the tree shape, e.g. `"L0: 2 files, 7.10 MiB; ..."`.
    pub fn level_summary(&self) -> String {
        self.inner.state.lock().versions.current().summary()
    }

    /// Number of live snapshots.
    pub fn num_snapshots(&self) -> usize {
        self.inner.snapshots.len()
    }

    /// Syncs the WAL and closes the database, reporting any error. Dropping
    /// a `Db` closes it too, but cannot report errors.
    pub fn close(self) -> Result<()> {
        let r = self.inner.writer.lock().log.sync();
        drop(self);
        r
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        {
            let _st = self.inner.state.lock();
            self.inner.shutting_down.store(true, Ordering::SeqCst);
            self.inner.bg_work_cv.notify_all();
        }
        if let Some(handle) = self.bg_thread.take() {
            let _ = handle.join();
        }
    }
}

type ReadView = (
    SequenceNumber,
    Arc<MemTable>,
    Option<Arc<MemTable>>,
    Arc<Version>,
);

impl DbInner {
    /// Captures a consistent read view. Loading the sequence under the
    /// state lock matters: compaction installs a version that may have
    /// dropped entries older than `last_sequence` at compaction start, so a
    /// reader that sees that version must also use a sequence at least that
    /// new.
    fn read_view(&self, opts: &ReadOptions<'_>) -> ReadView {
        let st = self.state.lock();
        let seq = match opts.snapshot {
            Some(s) => s.sequence(),
            None => self.last_sequence.load(Ordering::Acquire),
        };
        (
            seq,
            Arc::clone(&st.mem),
            st.imm.clone(),
            st.versions.current(),
        )
    }

    /// Ensures the active memtable has room, switching to a new memtable
    /// and WAL when it is full (or when `force` is set and it is non-empty).
    /// Applies backpressure when flushes or level-0 compaction fall behind.
    fn make_room_for_write(&self, w: &mut WriterState, force: bool) -> Result<Arc<MemTable>> {
        let mut allow_delay = !force;
        let mut force = force;
        let mut st = self.state.lock();
        loop {
            if let Some(e) = &st.bg_error {
                return Err(e.clone());
            }
            let l0_files = st.versions.current().num_files(0);
            if allow_delay && l0_files >= self.opts.l0_slowdown_writes_trigger {
                // Soft limit: hand compaction ~1 ms per write rather than
                // stopping writes outright once the hard limit is hit.
                let start = Instant::now();
                MutexGuard::unlocked(&mut st, || thread::sleep(Duration::from_millis(1)));
                Stats::add(&self.stats.stall_micros, start.elapsed().as_micros() as u64);
                allow_delay = false;
                continue;
            }
            if !force && st.mem.approximate_size() < self.opts.write_buffer_size {
                return Ok(Arc::clone(&st.mem));
            }
            if force && st.mem.is_empty() {
                return Ok(Arc::clone(&st.mem));
            }
            if st.imm.is_some() || l0_files >= self.opts.l0_stop_writes_trigger {
                let start = Instant::now();
                self.bg_work_cv.notify_one();
                self.bg_done_cv.wait(&mut st);
                Stats::add(&self.stats.stall_micros, start.elapsed().as_micros() as u64);
                continue;
            }
            // Switch to a fresh memtable and WAL.
            let number = st.versions.new_file_number();
            let file = File::create(log_file_name(&self.dir, number))?;
            w.log = LogWriter::new(file);
            w.log_dir_synced = false;
            st.mem_log_number = number;
            let full = std::mem::replace(&mut st.mem, Arc::new(MemTable::new()));
            st.imm = Some(full);
            self.has_imm.store(true, Ordering::Release);
            self.bg_work_cv.notify_one();
            force = false;
        }
    }
}

#[cfg(test)]
mod tests;
