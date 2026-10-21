//! Compaction picking (leveled strategy).
//!
//! Each level `L >= 1` has a byte budget `base * multiplier^(L-1)`; level 0
//! is budgeted by file count because its files overlap and every one of them
//! must be consulted on reads. The level with the highest
//! `actual / budget` score above 1.0 is compacted into the next level:
//!
//! * **Level 0:** all level-0 files plus every overlapping level-1 file.
//! * **Level L >= 1:** one file chosen round-robin by a per-level compaction
//!   pointer, plus every overlapping file in level L+1.

use std::cmp::Ordering;
use std::sync::Arc;

use super::edit::{FileMetaData, VersionEdit};
use super::{LevelIterator, Version, VersionSet, key_range};
use crate::error::Result;
use crate::iterator::BoxedIterator;
use crate::key::{compare_internal_keys, user_key};
use crate::table_cache::TableCache;

/// A planned compaction of `inputs[0]` (at `level`) with `inputs[1]` (at
/// `level + 1`), plus the state needed while writing its output.
pub struct Compaction {
    pub level: usize,
    pub inputs: [Vec<Arc<FileMetaData>>; 2],
    pub input_version: Arc<Version>,
    pub is_manual: bool,
    pub max_output_file_size: u64,
    compact_pointer: Vec<u8>,
    grandparents: Vec<Arc<FileMetaData>>,
    max_grandparent_overlap: u64,
    grandparent_index: usize,
    seen_key: bool,
    overlapped_bytes: u64,
    level_ptrs: Vec<usize>,
}

impl Compaction {
    pub fn output_level(&self) -> usize {
        self.level + 1
    }

    pub fn input_bytes(&self) -> u64 {
        self.inputs.iter().flatten().map(|f| f.file_size).sum()
    }

    /// A single input file with nothing to merge against can simply be
    /// relinked one level down, as long as that does not create a file that
    /// overlaps too much of the grandparent level.
    pub fn is_trivial_move(&self) -> bool {
        !self.is_manual
            && self.inputs[0].len() == 1
            && self.inputs[1].is_empty()
            && self.grandparents.iter().map(|f| f.file_size).sum::<u64>()
                <= self.max_grandparent_overlap
    }

    /// Adds deletions of all input files and the compaction-pointer update.
    pub fn add_input_deletions(&self, edit: &mut VersionEdit) {
        for (which, files) in self.inputs.iter().enumerate() {
            for f in files {
                edit.delete_file(self.level + which, f.number);
            }
        }
        edit.compact_pointers
            .push((self.level, self.compact_pointer.clone()));
    }

    /// True if no level below the output level can contain `ukey`, so a
    /// tombstone for it can be dropped. Keys must be passed in increasing
    /// order (per-level cursors only move forward).
    pub fn is_base_level_for_key(&mut self, ukey: &[u8]) -> bool {
        for level in self.level + 2..self.input_version.num_levels() {
            let files = self.input_version.files(level);
            let ptr = &mut self.level_ptrs[level];
            while *ptr < files.len() {
                let f = &files[*ptr];
                if ukey <= f.largest_user_key() {
                    if ukey >= f.smallest_user_key() {
                        return false;
                    }
                    break;
                }
                *ptr += 1;
            }
        }
        true
    }

    /// True if the output file should be cut before `ikey` because it
    /// already overlaps too many bytes of the grandparent level.
    pub fn should_stop_before(&mut self, ikey: &[u8]) -> bool {
        while self.grandparent_index < self.grandparents.len()
            && compare_internal_keys(ikey, &self.grandparents[self.grandparent_index].largest)
                == Ordering::Greater
        {
            if self.seen_key {
                self.overlapped_bytes += self.grandparents[self.grandparent_index].file_size;
            }
            self.grandparent_index += 1;
        }
        self.seen_key = true;
        if self.overlapped_bytes > self.max_grandparent_overlap {
            self.overlapped_bytes = 0;
            true
        } else {
            false
        }
    }

    /// Iterators over all inputs (block cache bypassed: compaction reads
    /// each block once and would only evict the hot set).
    pub fn input_iterators(&self, tables: &Arc<TableCache>) -> Result<Vec<BoxedIterator>> {
        let mut out: Vec<BoxedIterator> = Vec::new();
        for (which, files) in self.inputs.iter().enumerate() {
            if files.is_empty() {
                continue;
            }
            if self.level + which == 0 {
                for f in files {
                    out.push(Box::new(tables.get(f.number, f.file_size)?.iter(false)));
                }
            } else {
                out.push(Box::new(LevelIterator::new(
                    files.clone(),
                    Arc::clone(tables),
                    false,
                )));
            }
        }
        Ok(out)
    }
}

impl VersionSet {
    /// `(level, score)` of the level most in need of compaction.
    pub fn compaction_score(&self) -> (usize, f64) {
        let v = &self.current;
        let mut best = (0, 0.0);
        for level in 0..v.num_levels() - 1 {
            let score = if level == 0 {
                v.num_files(0) as f64 / self.opts.l0_compaction_trigger as f64
            } else {
                v.level_bytes(level) as f64 / self.opts.max_bytes_for_level(level) as f64
            };
            if score > best.1 {
                best = (level, score);
            }
        }
        best
    }

    pub fn needs_compaction(&self) -> bool {
        self.compaction_score().1 >= 1.0
    }

    /// Picks the next automatic compaction, if any level is over budget.
    pub fn pick_compaction(&self) -> Option<Compaction> {
        let (level, score) = self.compaction_score();
        if score < 1.0 {
            return None;
        }
        let current = self.current();
        let inputs0 = if level == 0 {
            current.files(0).to_vec()
        } else {
            let files = current.files(level);
            let pointer = &self.compact_pointers[level];
            let pick = files
                .iter()
                .find(|f| {
                    pointer.is_empty()
                        || compare_internal_keys(&f.largest, pointer) == Ordering::Greater
                })
                .or_else(|| files.first())?;
            vec![Arc::clone(pick)]
        };
        Some(self.setup_compaction(current, level, inputs0, false))
    }

    /// Plans a manual compaction of `level` restricted to user keys in
    /// `[begin, end]`.
    pub fn compact_range(
        &self,
        level: usize,
        begin: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Option<Compaction> {
        let current = self.current();
        let inputs0 = current.overlapping_inputs(level, begin, end);
        if inputs0.is_empty() {
            return None;
        }
        Some(self.setup_compaction(current, level, inputs0, true))
    }

    fn setup_compaction(
        &self,
        current: Arc<Version>,
        level: usize,
        inputs0: Vec<Arc<FileMetaData>>,
        is_manual: bool,
    ) -> Compaction {
        let (smallest, largest) = key_range(&inputs0).expect("non-empty inputs");
        let inputs1 = current.overlapping_inputs(
            level + 1,
            Some(user_key(&smallest)),
            Some(user_key(&largest)),
        );
        let grandparents = if level + 2 < current.num_levels() {
            let (lo, hi) = key_range(inputs0.iter().chain(&inputs1)).expect("non-empty");
            current.overlapping_inputs(level + 2, Some(user_key(&lo)), Some(user_key(&hi)))
        } else {
            Vec::new()
        };
        Compaction {
            level,
            inputs: [inputs0, inputs1],
            level_ptrs: vec![0; current.num_levels()],
            input_version: current,
            is_manual,
            max_output_file_size: self.opts.target_file_size,
            compact_pointer: largest,
            grandparents,
            max_grandparent_overlap: self.opts.max_grandparent_overlap_bytes(),
            grandparent_index: 0,
            seen_key: false,
            overlapped_bytes: 0,
        }
    }
}
