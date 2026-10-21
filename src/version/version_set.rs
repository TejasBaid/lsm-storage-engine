use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use super::Version;
use super::edit::{COMPARATOR_NAME, VersionEdit};
use crate::error::{Error, Result};
use crate::filename::{
    FileType, manifest_file_name, parse_file_name, read_current_file, set_current_file, sync_dir,
};
use crate::key::SequenceNumber;
use crate::options::{Options, WalRecoveryMode};
use crate::wal::{LogReader, LogWriter};

/// Owns the current [`Version`], the manifest, and the global counters.
///
/// Every metadata change goes through [`VersionSet::log_and_apply`], which
/// appends the edit to the manifest, `fsync`s it, and only then installs the
/// new version. A crash at any point therefore leaves the manifest
/// describing either the old or the new state — never a mixture.
pub struct VersionSet {
    pub(super) dir: PathBuf,
    pub(super) opts: Options,
    pub(super) current: Arc<Version>,
    live: Vec<Weak<Version>>,
    next_file_number: u64,
    manifest_file_number: u64,
    log_number: u64,
    /// Highest sequence number known to be persisted; maintained by the DB.
    pub last_sequence: SequenceNumber,
    manifest: Option<LogWriter>,
    pub(super) compact_pointers: Vec<Vec<u8>>,
}

impl VersionSet {
    pub fn new(dir: PathBuf, opts: Options) -> Self {
        let current = Arc::new(Version::empty(opts.max_levels));
        VersionSet {
            dir,
            live: vec![Arc::downgrade(&current)],
            current,
            next_file_number: 2,
            manifest_file_number: 0,
            log_number: 0,
            last_sequence: 0,
            manifest: None,
            compact_pointers: vec![Vec::new(); opts.max_levels],
            opts,
        }
    }

    /// Initializes an empty database: `MANIFEST-000001` plus `CURRENT`.
    pub fn create_new_db(dir: &std::path::Path) -> Result<()> {
        let edit = VersionEdit {
            comparator: Some(COMPARATOR_NAME.to_string()),
            log_number: Some(0),
            next_file_number: Some(2),
            last_sequence: Some(0),
            ..Default::default()
        };
        let path = manifest_file_name(dir, 1);
        let result = (|| {
            let mut w = LogWriter::new(File::create(&path)?);
            w.add_record(&edit.encode())?;
            w.sync()?;
            set_current_file(dir, 1)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&path);
        }
        result
    }

    /// Rebuilds state by replaying the manifest named by `CURRENT`.
    pub fn recover(&mut self) -> Result<()> {
        let name = read_current_file(&self.dir)?;
        let manifest_number = match parse_file_name(&name) {
            Some((FileType::Manifest, n)) => n,
            _ => unreachable!("read_current_file validates the name"),
        };
        let data = fs::read(self.dir.join(&name))?;
        // A torn final manifest record is an edit that was never
        // acknowledged (the version is installed only after the sync), so
        // dropping it is always safe.
        let mut reader = LogReader::new(data, WalRecoveryMode::TolerateCorruptedTailRecords);
        let mut version = Version::empty(self.opts.max_levels);
        let (mut log_number, mut next_file, mut last_seq) = (None, None, None);
        while let Some(record) = reader.next_record()? {
            let edit = VersionEdit::decode(record)?;
            if let Some(c) = &edit.comparator
                && c != COMPARATOR_NAME
            {
                return Err(Error::InvalidArgument(format!(
                    "database uses comparator {c:?}, expected {COMPARATOR_NAME:?}"
                )));
            }
            version = version.apply(&edit)?;
            for (level, key) in &edit.compact_pointers {
                if let Some(slot) = self.compact_pointers.get_mut(*level) {
                    slot.clone_from(key);
                }
            }
            log_number = edit.log_number.or(log_number);
            next_file = edit.next_file_number.or(next_file);
            last_seq = edit.last_sequence.or(last_seq);
        }
        let missing = |what: &str| Error::corruption(format!("manifest has no {what} entry"));
        self.next_file_number = next_file.ok_or_else(|| missing("next_file_number"))?;
        self.log_number = log_number.ok_or_else(|| missing("log_number"))?;
        self.last_sequence = last_seq.ok_or_else(|| missing("last_sequence"))?;
        self.manifest_file_number = manifest_number;
        self.mark_file_number_used(manifest_number);
        self.mark_file_number_used(self.log_number);
        for n in version.file_numbers() {
            self.mark_file_number_used(n);
        }
        self.install(version);
        // The next log_and_apply starts a fresh, compacted manifest.
        self.manifest = None;
        Ok(())
    }

    pub fn current(&self) -> Arc<Version> {
        Arc::clone(&self.current)
    }

    pub fn log_number(&self) -> u64 {
        self.log_number
    }

    pub fn manifest_file_number(&self) -> u64 {
        self.manifest_file_number
    }

    pub fn new_file_number(&mut self) -> u64 {
        let n = self.next_file_number;
        self.next_file_number += 1;
        n
    }

    pub fn mark_file_number_used(&mut self, n: u64) {
        if self.next_file_number <= n {
            self.next_file_number = n + 1;
        }
    }

    fn install(&mut self, version: Version) {
        let v = Arc::new(version);
        self.live.retain(|w| w.strong_count() > 0);
        self.live.push(Arc::downgrade(&v));
        self.current = v;
    }

    /// Numbers of every table referenced by any version still in use.
    pub fn live_files(&mut self) -> HashSet<u64> {
        self.live.retain(|w| w.strong_count() > 0);
        let mut out: HashSet<u64> = self.current.file_numbers().collect();
        for v in self.live.iter().filter_map(Weak::upgrade) {
            out.extend(v.file_numbers());
        }
        out
    }

    /// An edit that recreates the current state from scratch; the first
    /// record of every new manifest.
    fn snapshot_edit(&self) -> VersionEdit {
        let mut edit = VersionEdit {
            comparator: Some(COMPARATOR_NAME.to_string()),
            log_number: Some(self.log_number),
            next_file_number: Some(self.next_file_number),
            last_sequence: Some(self.last_sequence),
            ..Default::default()
        };
        for (level, key) in self.compact_pointers.iter().enumerate() {
            if !key.is_empty() {
                edit.compact_pointers.push((level, key.clone()));
            }
        }
        for level in 0..self.current.num_levels() {
            for f in self.current.files(level) {
                edit.add_file(level, (**f).clone());
            }
        }
        edit
    }

    /// Durably records `edit` in the manifest, then installs the resulting
    /// version as current.
    pub fn log_and_apply(&mut self, edit: &mut VersionEdit) -> Result<()> {
        let mut pending = self.prepare_apply(edit)?;
        let result = pending.write(&self.dir);
        self.finish_apply(pending, result)
    }

    /// First phase of [`VersionSet::log_and_apply`]: validates `edit`, fills
    /// in the counters, computes the new version and serializes the
    /// manifest record. Cheap; meant to run under the DB mutex.
    ///
    /// The caller must run [`PendingApply::write`] (which does the slow
    /// `fsync`s and may run with the mutex released) and then
    /// [`VersionSet::finish_apply`]. Only one apply may be in flight.
    pub fn prepare_apply(&mut self, edit: &mut VersionEdit) -> Result<PendingApply> {
        match edit.log_number {
            Some(n) if n < self.log_number || n >= self.next_file_number => {
                return Err(Error::InvalidArgument(format!(
                    "bad log number {n} in edit"
                )));
            }
            Some(_) => {}
            None => edit.log_number = Some(self.log_number),
        }
        let rotate = self
            .manifest
            .as_ref()
            .is_none_or(|m| m.bytes_written() > self.opts.max_manifest_file_size);
        let rotate_to = rotate.then(|| self.new_file_number());
        edit.next_file_number = Some(self.next_file_number);
        edit.last_sequence = Some(self.last_sequence);

        let version = self.current.apply(edit)?;
        let target = match rotate_to {
            Some(number) => ManifestTarget::Rotate {
                number,
                snapshot: self.snapshot_edit().encode(),
                writer: None,
            },
            None => ManifestTarget::Append(self.manifest.take().expect("manifest is open")),
        };
        Ok(PendingApply {
            version,
            record: edit.encode(),
            target,
            // New tables must be durably linked into the directory before
            // the manifest references them.
            sync_dir: !edit.new_files.is_empty(),
            log_number: edit.log_number.expect("set above"),
            compact_pointers: edit.compact_pointers.clone(),
        })
    }

    /// Last phase of [`VersionSet::log_and_apply`]: installs the new version
    /// if the manifest write succeeded.
    pub fn finish_apply(&mut self, pending: PendingApply, written: Result<()>) -> Result<()> {
        match (pending.target, written) {
            (ManifestTarget::Append(w), Ok(())) => self.manifest = Some(w),
            (ManifestTarget::Append(_), Err(e)) => {
                // The manifest may now end in a torn record; never append
                // after it. The next edit starts a fresh manifest.
                self.manifest = None;
                return Err(e);
            }
            (ManifestTarget::Rotate { number, writer, .. }, Ok(())) => {
                self.manifest = Some(writer.expect("written manifest"));
                self.manifest_file_number = number;
            }
            // The previous manifest (if any) was never touched and stays valid.
            (ManifestTarget::Rotate { .. }, Err(e)) => return Err(e),
        }
        self.log_number = pending.log_number;
        for (level, key) in &pending.compact_pointers {
            if let Some(slot) = self.compact_pointers.get_mut(*level) {
                slot.clone_from(key);
            }
        }
        self.install(pending.version);
        Ok(())
    }
}

enum ManifestTarget {
    /// Append the record to the open manifest.
    Append(LogWriter),
    /// Start manifest `number` with a full snapshot followed by the record,
    /// then point `CURRENT` at it.
    Rotate {
        number: u64,
        snapshot: Vec<u8>,
        writer: Option<LogWriter>,
    },
}

/// A version edit that has been prepared but not yet made durable.
pub struct PendingApply {
    version: Version,
    record: Vec<u8>,
    target: ManifestTarget,
    sync_dir: bool,
    log_number: u64,
    compact_pointers: Vec<(usize, Vec<u8>)>,
}

impl PendingApply {
    /// Performs the manifest I/O. Safe to run without holding the DB mutex.
    pub fn write(&mut self, dir: &Path) -> Result<()> {
        if self.sync_dir {
            sync_dir(dir)?;
        }
        match &mut self.target {
            ManifestTarget::Append(w) => {
                w.add_record(&self.record)?;
                w.sync()
            }
            ManifestTarget::Rotate {
                number,
                snapshot,
                writer,
            } => {
                let path = manifest_file_name(dir, *number);
                let result = (|| {
                    let mut w = LogWriter::new(File::create(&path)?);
                    w.add_record(snapshot)?;
                    w.add_record(&self.record)?;
                    w.sync()?;
                    set_current_file(dir, *number)?;
                    Ok(w)
                })();
                match result {
                    Ok(w) => {
                        *writer = Some(w);
                        Ok(())
                    }
                    Err(e) => {
                        let _ = fs::remove_file(&path);
                        Err(e)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{ValueType, make_internal_key};
    use crate::version::edit::FileMetaData;

    fn meta(number: u64, lo: &str, hi: &str) -> FileMetaData {
        FileMetaData {
            number,
            file_size: 1000,
            smallest: make_internal_key(lo.as_bytes(), 1, ValueType::Value),
            largest: make_internal_key(hi.as_bytes(), 1, ValueType::Value),
        }
    }

    #[test]
    fn edits_survive_reopen_and_manifest_rotation() {
        let dir = tempfile::tempdir().unwrap();
        VersionSet::create_new_db(dir.path()).unwrap();
        let opts = Options {
            max_manifest_file_size: 200, // force frequent rotation
            ..Options::default()
        };
        let mut expected_files = Vec::new();
        {
            let mut vs = VersionSet::new(dir.path().to_path_buf(), opts.clone());
            vs.recover().unwrap();
            for i in 0..20u64 {
                let n = vs.new_file_number();
                let mut e = VersionEdit::default();
                let lo = format!("k{:03}", i * 10);
                let hi = format!("k{:03}", i * 10 + 5);
                e.add_file(1, meta(n, &lo, &hi));
                vs.last_sequence = i * 100;
                vs.log_and_apply(&mut e).unwrap();
                expected_files.push(n);
            }
            // Move one file down a level.
            let moved = vs.current().files(1)[0].clone();
            let mut e = VersionEdit::default();
            e.delete_file(1, moved.number);
            e.add_file(2, (*moved).clone());
            vs.log_and_apply(&mut e).unwrap();
        }
        let mut vs = VersionSet::new(dir.path().to_path_buf(), opts);
        vs.recover().unwrap();
        let v = vs.current();
        assert_eq!(v.num_files(1), 19);
        assert_eq!(v.num_files(2), 1);
        assert_eq!(vs.last_sequence, 1900);
        let mut all: Vec<u64> = v.file_numbers().collect();
        all.sort();
        assert_eq!(all, expected_files);
        assert!(vs.new_file_number() > *expected_files.last().unwrap());
    }

    #[test]
    fn torn_manifest_tail_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        VersionSet::create_new_db(dir.path()).unwrap();
        let manifest;
        {
            let mut vs = VersionSet::new(dir.path().to_path_buf(), Options::default());
            vs.recover().unwrap();
            let n = vs.new_file_number();
            let mut e = VersionEdit::default();
            e.add_file(0, meta(n, "a", "b"));
            vs.log_and_apply(&mut e).unwrap();
            manifest = manifest_file_name(dir.path(), vs.manifest_file_number());
        }
        let mut bytes = fs::read(&manifest).unwrap();
        bytes.extend_from_slice(&[0x12, 0x34, 0x56]); // partial header
        fs::write(&manifest, bytes).unwrap();
        let mut vs = VersionSet::new(dir.path().to_path_buf(), Options::default());
        vs.recover().unwrap();
        assert_eq!(vs.current().num_files(0), 1);
    }

    #[test]
    fn live_files_tracks_old_versions() {
        let dir = tempfile::tempdir().unwrap();
        VersionSet::create_new_db(dir.path()).unwrap();
        let mut vs = VersionSet::new(dir.path().to_path_buf(), Options::default());
        vs.recover().unwrap();
        let n = vs.new_file_number();
        let mut e = VersionEdit::default();
        e.add_file(0, meta(n, "a", "b"));
        vs.log_and_apply(&mut e).unwrap();
        let pinned = vs.current();
        let mut e = VersionEdit::default();
        e.delete_file(0, n);
        vs.log_and_apply(&mut e).unwrap();
        assert!(
            vs.live_files().contains(&n),
            "pinned version keeps file alive"
        );
        drop(pinned);
        assert!(!vs.live_files().contains(&n));
    }
}
