//! Background work: memtable flushes, compactions and file garbage
//! collection, all run on a single dedicated thread.

use std::fs::{self, File};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use parking_lot::MutexGuard;

use super::stats::Stats;
use super::{DbInner, DbState, build_table};
use crate::error::{Error, Result};
use crate::filename::{FileType, parse_file_name, table_file_name};
use crate::iterator::{InternalIterator, MergingIterator};
use crate::key::{SequenceNumber, ValueType, parse_internal_key};
use crate::sstable::TableBuilder;
use crate::version::Compaction;
use crate::version::edit::{FileMetaData, VersionEdit};

/// How many entries a compaction merges between checks for a pending
/// memtable flush (which takes priority, since writers may be stalled on it).
const IMM_CHECK_INTERVAL: u64 = 1024;

enum Work {
    Flush,
    Manual(Option<Compaction>),
    Automatic(Compaction),
}

/// An output table of a compaction that is still being written.
struct Output {
    number: u64,
    builder: TableBuilder,
    smallest: Vec<u8>,
    largest: Vec<u8>,
}

impl DbInner {
    pub(super) fn background_loop(self: Arc<Self>) {
        loop {
            {
                let mut st = self.state.lock();
                loop {
                    if self.shutting_down.load(Ordering::Acquire) {
                        return;
                    }
                    if needs_work(&st) {
                        break;
                    }
                    self.bg_work_cv.wait(&mut st);
                }
            }
            match self.background_work() {
                Ok(()) => {}
                Err(Error::ShuttingDown) => return,
                Err(e) => {
                    let mut st = self.state.lock();
                    st.bg_error.get_or_insert(e);
                }
            }
            self.bg_done_cv.notify_all();
        }
    }

    fn background_work(&self) -> Result<()> {
        let work = {
            let st = self.state.lock();
            if st.imm.is_some() {
                Work::Flush
            } else if let Some(m) = &st.manual {
                Work::Manual(st.versions.compact_range(
                    m.level,
                    m.begin.as_deref(),
                    m.end.as_deref(),
                ))
            } else {
                match st.versions.pick_compaction() {
                    Some(c) => Work::Automatic(c),
                    None => return Ok(()),
                }
            }
        };
        match work {
            Work::Flush => self.flush_imm(),
            Work::Manual(c) => {
                let result = c.map_or(Ok(()), |c| self.run_compaction(c));
                self.state.lock().manual = None;
                result
            }
            Work::Automatic(c) => self.run_compaction(c),
        }
    }

    /// Writes the immutable memtable to a level-0 table and retires its WAL.
    pub(super) fn flush_imm(&self) -> Result<()> {
        let (imm, number) = {
            let mut st = self.state.lock();
            let Some(imm) = st.imm.clone() else {
                return Ok(());
            };
            let number = st.versions.new_file_number();
            st.pending_outputs.insert(number);
            (imm, number)
        };

        let built = build_table(&self.dir, &self.opts, &mut imm.iter(), number);

        let mut st = self.state.lock();
        st.pending_outputs.remove(&number);
        let meta = built?;
        let mut edit = VersionEdit::default();
        if let Some(meta) = meta {
            Stats::add(&self.stats.flush_bytes, meta.file_size);
            Stats::add(&self.stats.level_bytes_written[0], meta.file_size);
            edit.add_file(0, meta);
        }
        // Every WAL older than the active memtable's now holds only data
        // that is safely in a table.
        edit.log_number = Some(st.mem_log_number);
        self.commit(&mut st, &mut edit)?;
        st.imm = None;
        self.has_imm.store(false, Ordering::Release);
        Stats::add(&self.stats.flushes, 1);
        drop(st);

        self.bg_done_cv.notify_all();
        self.delete_obsolete_files();
        Ok(())
    }

    /// Commits `edit` to the manifest and installs the new version. The
    /// mutex is released during the manifest write and `fsync`, so readers
    /// and writers are not blocked behind disk flushes. This is safe because
    /// only the background thread applies edits.
    fn commit(&self, st: &mut MutexGuard<'_, DbState>, edit: &mut VersionEdit) -> Result<()> {
        st.versions.last_sequence = self.last_sequence.load(Ordering::Acquire);
        let mut pending = st.versions.prepare_apply(edit)?;
        let written = MutexGuard::unlocked(st, || pending.write(&self.dir));
        st.versions.finish_apply(pending, written)
    }

    fn run_compaction(&self, mut c: Compaction) -> Result<()> {
        if c.is_trivial_move() {
            let f = Arc::clone(&c.inputs[0][0]);
            let mut edit = VersionEdit::default();
            c.add_input_deletions(&mut edit);
            edit.add_file(c.output_level(), (*f).clone());
            self.commit(&mut self.state.lock(), &mut edit)?;
            Stats::add(&self.stats.trivial_moves, 1);
            return Ok(());
        }

        let smallest_snapshot = self.snapshots.smallest(&self.last_sequence);
        let mut created = Vec::new();
        let result = self.merge_inputs(&mut c, smallest_snapshot, &mut created);
        let outputs = match result {
            Ok(outputs) => outputs,
            Err(e) => {
                let mut st = self.state.lock();
                for n in &created {
                    st.pending_outputs.remove(n);
                    let _ = fs::remove_file(table_file_name(&self.dir, *n));
                }
                return Err(e);
            }
        };

        let written: u64 = outputs.iter().map(|o| o.file_size).sum();
        let read = c.input_bytes();
        let mut edit = VersionEdit::default();
        c.add_input_deletions(&mut edit);
        for o in outputs {
            edit.add_file(c.output_level(), o);
        }
        {
            let mut st = self.state.lock();
            for n in &created {
                st.pending_outputs.remove(n);
            }
            self.commit(&mut st, &mut edit)?;
        }

        let s = &self.stats;
        Stats::add(&s.compactions, 1);
        Stats::add(&s.compaction_bytes_read, read);
        Stats::add(&s.compaction_bytes_written, written);
        Stats::add(&s.level_compactions[c.output_level()], 1);
        Stats::add(&s.level_bytes_read[c.output_level()], read);
        Stats::add(&s.level_bytes_written[c.output_level()], written);
        // Release the input version first, or its files would stay "live".
        drop(c);
        self.delete_obsolete_files();
        Ok(())
    }

    /// Merges the compaction inputs into new tables, dropping entries no
    /// reader can observe:
    ///
    /// 1. an older version of a key whose newer version is at or below the
    ///    oldest live snapshot (every reader sees the newer one), and
    /// 2. a tombstone at or below the oldest snapshot when no deeper level
    ///    can hold an older value it would need to mask.
    ///
    /// Output files are cut only between distinct user keys, keeping user-key
    /// ranges of files in a level disjoint.
    fn merge_inputs(
        &self,
        c: &mut Compaction,
        smallest_snapshot: SequenceNumber,
        created: &mut Vec<u64>,
    ) -> Result<Vec<FileMetaData>> {
        let mut input = MergingIterator::new(c.input_iterators(&self.table_cache)?);
        input.seek_to_first()?;

        let mut outputs = Vec::new();
        let mut current: Option<Output> = None;
        let mut current_user_key: Option<Vec<u8>> = None;
        let mut last_sequence_for_key = SequenceNumber::MAX;
        let mut cut_pending = false;
        let mut processed = 0u64;

        while input.valid() {
            if self.shutting_down.load(Ordering::Relaxed) {
                return Err(Error::ShuttingDown);
            }
            processed += 1;
            if processed.is_multiple_of(IMM_CHECK_INTERVAL) && self.has_imm.load(Ordering::Acquire)
            {
                self.flush_imm()?;
            }

            let key = input.key();
            let parsed = parse_internal_key(key)
                .ok_or_else(|| Error::corruption("malformed internal key in compaction input"))?;
            if current_user_key.as_deref() != Some(parsed.user_key) {
                current_user_key = Some(parsed.user_key.to_vec());
                last_sequence_for_key = SequenceNumber::MAX;
            }
            cut_pending |= c.should_stop_before(key);

            let drop = if last_sequence_for_key <= smallest_snapshot {
                true // shadowed by a newer entry visible to every reader
            } else {
                parsed.value_type == ValueType::Deletion
                    && parsed.sequence <= smallest_snapshot
                    && c.is_base_level_for_key(parsed.user_key)
            };
            last_sequence_for_key = parsed.sequence;

            if !drop {
                if let Some(out) = &current
                    && crate::key::user_key(&out.largest) != parsed.user_key
                    && (cut_pending || out.builder.file_size() >= c.max_output_file_size)
                {
                    let out = current.take().expect("checked above");
                    outputs.push(Self::finish_output(out)?);
                    cut_pending = false;
                }
                if current.is_none() {
                    let number = {
                        let mut st = self.state.lock();
                        let n = st.versions.new_file_number();
                        st.pending_outputs.insert(n);
                        n
                    };
                    created.push(number);
                    let file = File::create(table_file_name(&self.dir, number))?;
                    current = Some(Output {
                        number,
                        builder: TableBuilder::new(file, &self.opts),
                        smallest: key.to_vec(),
                        largest: Vec::new(),
                    });
                }
                let out = current.as_mut().expect("opened above");
                out.builder.add(key, input.value())?;
                out.largest.clear();
                out.largest.extend_from_slice(key);
            }
            input.next()?;
        }
        if let Some(out) = current.take() {
            outputs.push(Self::finish_output(out)?);
        }
        Ok(outputs)
    }

    fn finish_output(out: Output) -> Result<FileMetaData> {
        let file_size = out.builder.finish()?;
        Ok(FileMetaData {
            number: out.number,
            file_size,
            smallest: out.smallest,
            largest: out.largest,
        })
    }

    /// Deletes files no longer referenced by any live version, WAL, or
    /// manifest. Errors are ignored: a leftover file is harmless and will be
    /// collected next time.
    pub(super) fn delete_obsolete_files(&self) {
        let (live, log_number, manifest_number) = {
            let mut st = self.state.lock();
            let mut live = st.versions.live_files();
            live.extend(st.pending_outputs.iter().copied());
            (
                live,
                st.versions.log_number(),
                st.versions.manifest_file_number(),
            )
        };
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some((kind, number)) = name.to_str().and_then(parse_file_name) else {
                continue;
            };
            let keep = match kind {
                FileType::Log => number >= log_number,
                FileType::Table => live.contains(&number),
                FileType::Manifest | FileType::Temp => number >= manifest_number,
                FileType::Current | FileType::Lock => true,
            };
            if !keep {
                if kind == FileType::Table {
                    self.table_cache.evict(number);
                }
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn needs_work(st: &DbState) -> bool {
    st.bg_error.is_none()
        && (st.imm.is_some() || st.manual.is_some() || st.versions.needs_compaction())
}
