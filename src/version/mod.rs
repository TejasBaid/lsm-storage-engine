//! Versions: immutable snapshots of the LSM tree's file layout.
//!
//! A [`Version`] lists the SSTables in each level. Every flush or compaction
//! produces a [`VersionEdit`](edit::VersionEdit) that the
//! [`VersionSet`] durably appends to the manifest and then applies to the
//! current version to produce a new one. Readers and iterators hold an
//! `Arc<Version>`, which keeps that version's files alive (they are not
//! deleted) until the reader finishes.
//!
//! Invariants:
//! * Level 0 files may overlap; they are ordered newest first (by file
//!   number) and searched in that order.
//! * Files in levels >= 1 have disjoint user-key ranges and are ordered by
//!   smallest key, so a point lookup touches at most one file per level.

mod compaction;
pub mod edit;
mod version_set;

use std::cmp::Ordering;
use std::sync::Arc;

pub use compaction::Compaction;
pub use version_set::VersionSet;

use crate::error::{Error, Result};
use crate::iterator::{BoxedIterator, InternalIterator};
use crate::key::{LookupResult, compare_internal_keys, user_key};
use crate::sstable::TableIterator;
use crate::table_cache::TableCache;
use edit::{FileMetaData, VersionEdit};

/// The set of live files at one point in time.
#[derive(Debug, Clone)]
pub struct Version {
    files: Vec<Vec<Arc<FileMetaData>>>,
}

/// Index of the first file whose largest key is `>= ikey` (levels >= 1).
pub fn find_file(files: &[Arc<FileMetaData>], ikey: &[u8]) -> usize {
    files.partition_point(|f| compare_internal_keys(&f.largest, ikey) == Ordering::Less)
}

/// `(smallest, largest)` internal keys spanned by `files`.
pub fn key_range<'a>(
    files: impl IntoIterator<Item = &'a Arc<FileMetaData>>,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut range: Option<(&[u8], &[u8])> = None;
    for f in files {
        range = Some(match range {
            None => (&f.smallest, &f.largest),
            Some((lo, hi)) => (
                if compare_internal_keys(&f.smallest, lo).is_lt() {
                    &f.smallest
                } else {
                    lo
                },
                if compare_internal_keys(&f.largest, hi).is_gt() {
                    &f.largest
                } else {
                    hi
                },
            ),
        });
    }
    range.map(|(lo, hi)| (lo.to_vec(), hi.to_vec()))
}

impl Version {
    /// An empty version with `num_levels` levels.
    pub fn empty(num_levels: usize) -> Self {
        Version {
            files: vec![Vec::new(); num_levels],
        }
    }

    pub fn num_levels(&self) -> usize {
        self.files.len()
    }

    pub fn files(&self, level: usize) -> &[Arc<FileMetaData>] {
        &self.files[level]
    }

    pub fn num_files(&self, level: usize) -> usize {
        self.files[level].len()
    }

    pub fn level_bytes(&self, level: usize) -> u64 {
        self.files[level].iter().map(|f| f.file_size).sum()
    }

    /// All file numbers referenced by this version.
    pub fn file_numbers(&self) -> impl Iterator<Item = u64> + '_ {
        self.files.iter().flatten().map(|f| f.number)
    }

    /// Produces the version that results from applying `edit`.
    pub fn apply(&self, edit: &VersionEdit) -> Result<Version> {
        let mut files = self.files.clone();
        let num_levels = files.len();
        let check_level = |level: usize| {
            if level >= num_levels {
                Err(Error::InvalidArgument(format!(
                    "manifest references level {level} but max_levels is {num_levels}"
                )))
            } else {
                Ok(())
            }
        };
        for &(level, number) in &edit.deleted_files {
            check_level(level)?;
            let before = files[level].len();
            files[level].retain(|f| f.number != number);
            if files[level].len() == before {
                return Err(Error::corruption(format!(
                    "version edit deletes missing file {number} at level {level}"
                )));
            }
        }
        for (level, meta) in &edit.new_files {
            check_level(*level)?;
            files[*level].push(Arc::new(meta.clone()));
        }
        files[0].sort_by_key(|f| std::cmp::Reverse(f.number));
        for (level, level_files) in files.iter_mut().enumerate().skip(1) {
            level_files.sort_by(|a, b| compare_internal_keys(&a.smallest, &b.smallest));
            for pair in level_files.windows(2) {
                if pair[0].largest_user_key() >= pair[1].smallest_user_key() {
                    return Err(Error::corruption(format!(
                        "files {} and {} overlap in level {level}",
                        pair[0].number, pair[1].number
                    )));
                }
            }
        }
        Ok(Version { files })
    }

    /// Point lookup of internal key `ikey` (built from the user key and the
    /// read snapshot) across all levels.
    pub fn get(
        &self,
        tables: &TableCache,
        ikey: &[u8],
        fill_cache: bool,
    ) -> Result<Option<LookupResult>> {
        let ukey = user_key(ikey);
        // Level 0: every overlapping file, newest first.
        for f in &self.files[0] {
            if f.smallest_user_key() <= ukey
                && ukey <= f.largest_user_key()
                && let Some(r) = search_file(tables, f, ikey, fill_cache)?
            {
                return Ok(Some(r));
            }
        }
        // Deeper levels: at most one candidate per level.
        for files in &self.files[1..] {
            let i = find_file(files, ikey);
            if let Some(f) = files.get(i)
                && f.smallest_user_key() <= ukey
                && let Some(r) = search_file(tables, f, ikey, fill_cache)?
            {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    /// Files in `level` overlapping the user-key range `[begin, end]`
    /// (`None` = unbounded). For level 0 the range is widened until no
    /// further file overlaps it, so that newer level-0 data is never moved
    /// below older overlapping data.
    pub fn overlapping_inputs(
        &self,
        level: usize,
        begin: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Vec<Arc<FileMetaData>> {
        let mut begin = begin.map(<[u8]>::to_vec);
        let mut end = end.map(<[u8]>::to_vec);
        let files = &self.files[level];
        let mut out = Vec::new();
        let mut i = 0;
        while i < files.len() {
            let f = &files[i];
            i += 1;
            let (lo, hi) = (f.smallest_user_key(), f.largest_user_key());
            if begin.as_deref().is_some_and(|b| hi < b) || end.as_deref().is_some_and(|e| lo > e) {
                continue;
            }
            out.push(Arc::clone(f));
            if level == 0 {
                if begin.as_deref().is_some_and(|b| lo < b) {
                    begin = Some(lo.to_vec());
                    out.clear();
                    i = 0;
                } else if end.as_deref().is_some_and(|e| hi > e) {
                    end = Some(hi.to_vec());
                    out.clear();
                    i = 0;
                }
            }
        }
        out
    }

    /// One iterator per level-0 file plus one concatenating iterator per
    /// non-empty deeper level.
    pub fn iterators(
        &self,
        tables: &Arc<TableCache>,
        fill_cache: bool,
    ) -> Result<Vec<BoxedIterator>> {
        let mut out: Vec<BoxedIterator> = Vec::new();
        for f in &self.files[0] {
            out.push(Box::new(
                tables.get(f.number, f.file_size)?.iter(fill_cache),
            ));
        }
        for files in &self.files[1..] {
            if !files.is_empty() {
                out.push(Box::new(LevelIterator::new(
                    files.clone(),
                    Arc::clone(tables),
                    fill_cache,
                )));
            }
        }
        Ok(out)
    }

    /// Human-readable per-level summary.
    pub fn summary(&self) -> String {
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.is_empty())
            .map(|(l, f)| {
                format!(
                    "L{l}: {} files, {:.2} MiB",
                    f.len(),
                    f.iter().map(|f| f.file_size).sum::<u64>() as f64 / (1 << 20) as f64
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

fn search_file(
    tables: &TableCache,
    f: &FileMetaData,
    ikey: &[u8],
    fill_cache: bool,
) -> Result<Option<LookupResult>> {
    let table = tables.get(f.number, f.file_size)?;
    let ukey = user_key(ikey);
    if !table.may_contain(ukey) {
        tables.record_bloom_negative();
        return Ok(None);
    }
    table.get(ikey, fill_cache)
}

/// Concatenates the (disjoint, sorted) files of one level, opening each
/// table only when the cursor reaches it.
pub struct LevelIterator {
    files: Vec<Arc<FileMetaData>>,
    tables: Arc<TableCache>,
    fill_cache: bool,
    index: usize,
    current: Option<TableIterator>,
}

impl LevelIterator {
    pub fn new(files: Vec<Arc<FileMetaData>>, tables: Arc<TableCache>, fill_cache: bool) -> Self {
        LevelIterator {
            index: files.len(),
            files,
            tables,
            fill_cache,
            current: None,
        }
    }

    fn open_current(&mut self) -> Result<()> {
        self.current = None;
        if let Some(f) = self.files.get(self.index) {
            let table = self.tables.get(f.number, f.file_size)?;
            self.current = Some(table.iter(self.fill_cache));
        }
        Ok(())
    }

    fn skip_exhausted_files(&mut self) -> Result<()> {
        while self.current.as_ref().is_some_and(|c| !c.valid()) {
            self.index += 1;
            self.open_current()?;
            if let Some(c) = &mut self.current {
                c.seek_to_first()?;
            }
        }
        Ok(())
    }

    fn guard(&mut self, r: Result<()>) -> Result<()> {
        if r.is_err() {
            self.current = None;
        }
        r
    }
}

impl InternalIterator for LevelIterator {
    fn valid(&self) -> bool {
        self.current.as_ref().is_some_and(|c| c.valid())
    }

    fn seek_to_first(&mut self) -> Result<()> {
        let r = (|| {
            self.index = 0;
            self.open_current()?;
            if let Some(c) = &mut self.current {
                c.seek_to_first()?;
            }
            self.skip_exhausted_files()
        })();
        self.guard(r)
    }

    fn seek(&mut self, target: &[u8]) -> Result<()> {
        let r = (|| {
            self.index = find_file(&self.files, target);
            self.open_current()?;
            if let Some(c) = &mut self.current {
                c.seek(target)?;
            }
            self.skip_exhausted_files()
        })();
        self.guard(r)
    }

    fn next(&mut self) -> Result<()> {
        let r = (|| {
            if let Some(c) = &mut self.current {
                c.next()?;
            }
            self.skip_exhausted_files()
        })();
        self.guard(r)
    }

    fn key(&self) -> &[u8] {
        self.current.as_ref().expect("valid").key()
    }

    fn value(&self) -> &[u8] {
        self.current.as_ref().expect("valid").value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{ValueType, make_internal_key};

    fn meta(number: u64, lo: &str, hi: &str) -> FileMetaData {
        FileMetaData {
            number,
            file_size: 100,
            smallest: make_internal_key(lo.as_bytes(), 1, ValueType::Value),
            largest: make_internal_key(hi.as_bytes(), 1, ValueType::Value),
        }
    }

    #[test]
    fn apply_orders_levels_and_rejects_overlap() {
        let mut e = VersionEdit::default();
        e.add_file(0, meta(1, "a", "m"));
        e.add_file(0, meta(2, "c", "z"));
        e.add_file(1, meta(4, "n", "p"));
        e.add_file(1, meta(3, "a", "c"));
        let v = Version::empty(4).apply(&e).unwrap();
        assert_eq!(
            v.files(0).iter().map(|f| f.number).collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(
            v.files(1).iter().map(|f| f.number).collect::<Vec<_>>(),
            vec![3, 4]
        );

        let mut bad = VersionEdit::default();
        bad.add_file(1, meta(5, "b", "d"));
        assert!(v.apply(&bad).is_err());

        let mut missing = VersionEdit::default();
        missing.delete_file(2, 77);
        assert!(v.apply(&missing).is_err());
    }

    #[test]
    fn level0_overlap_expands_transitively() {
        let mut e = VersionEdit::default();
        e.add_file(0, meta(1, "a", "c"));
        e.add_file(0, meta(2, "b", "f"));
        e.add_file(0, meta(3, "e", "h"));
        e.add_file(0, meta(4, "x", "z"));
        let v = Version::empty(3).apply(&e).unwrap();
        let mut got: Vec<u64> = v
            .overlapping_inputs(0, Some(b"a"), Some(b"a"))
            .iter()
            .map(|f| f.number)
            .collect();
        got.sort();
        assert_eq!(got, vec![1, 2, 3]);
    }
}
