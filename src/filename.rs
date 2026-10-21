//! Database directory layout.
//!
//! ```text
//! <dir>/CURRENT          name of the live manifest, updated by atomic rename
//! <dir>/LOCK             advisory lock held while the database is open
//! <dir>/MANIFEST-NNNNNN  log of VersionEdits
//! <dir>/NNNNNN.log       write-ahead log for one memtable
//! <dir>/NNNNNN.sst       SSTable
//! <dir>/NNNNNN.dbtmp     temporary file (e.g. CURRENT being replaced)
//! ```
//!
//! All numbered files share one monotonically increasing counter, so a file
//! number uniquely identifies a file for the lifetime of the database.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Kinds of files found in a database directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// Write-ahead log.
    Log,
    /// SSTable.
    Table,
    /// Manifest (version edit log).
    Manifest,
    /// The `CURRENT` pointer file.
    Current,
    /// The `LOCK` file.
    Lock,
    /// A temporary file.
    Temp,
}

pub fn log_file_name(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("{number:06}.log"))
}

pub fn table_file_name(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("{number:06}.sst"))
}

pub fn manifest_file_name(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("MANIFEST-{number:06}"))
}

pub fn current_file_name(dir: &Path) -> PathBuf {
    dir.join("CURRENT")
}

pub fn lock_file_name(dir: &Path) -> PathBuf {
    dir.join("LOCK")
}

fn temp_file_name(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("{number:06}.dbtmp"))
}

/// Classifies a file name, returning its type and number (0 if unnumbered).
pub fn parse_file_name(name: &str) -> Option<(FileType, u64)> {
    match name {
        "CURRENT" => return Some((FileType::Current, 0)),
        "LOCK" => return Some((FileType::Lock, 0)),
        _ => {}
    }
    if let Some(n) = name.strip_prefix("MANIFEST-") {
        return parse_number(n).map(|n| (FileType::Manifest, n));
    }
    let (stem, ext) = name.rsplit_once('.')?;
    let number = parse_number(stem)?;
    let kind = match ext {
        "log" => FileType::Log,
        "sst" => FileType::Table,
        "dbtmp" => FileType::Temp,
        _ => return None,
    };
    Some((kind, number))
}

fn parse_number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `fsync`s a directory so that file creations and renames inside it are
/// durable.
pub fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Atomically points `CURRENT` at `MANIFEST-<manifest_number>`: write a temp
/// file, fsync it, rename over `CURRENT`, fsync the directory.
pub fn set_current_file(dir: &Path, manifest_number: u64) -> Result<()> {
    let tmp = temp_file_name(dir, manifest_number);
    {
        let mut f = File::create(&tmp)?;
        writeln!(f, "MANIFEST-{manifest_number:06}")?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, current_file_name(dir)) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    sync_dir(dir)
}

/// Reads `CURRENT` and returns the manifest file name it names.
pub fn read_current_file(dir: &Path) -> Result<String> {
    let contents = fs::read_to_string(current_file_name(dir))?;
    let name = contents
        .strip_suffix('\n')
        .ok_or_else(|| Error::corruption("CURRENT file does not end with a newline"))?;
    match parse_file_name(name) {
        Some((FileType::Manifest, _)) => Ok(name.to_string()),
        _ => Err(Error::corruption(format!(
            "CURRENT names an invalid manifest {name:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_names() {
        assert_eq!(parse_file_name("000123.log"), Some((FileType::Log, 123)));
        assert_eq!(parse_file_name("000007.sst"), Some((FileType::Table, 7)));
        assert_eq!(
            parse_file_name("MANIFEST-000002"),
            Some((FileType::Manifest, 2))
        );
        assert_eq!(parse_file_name("CURRENT"), Some((FileType::Current, 0)));
        assert_eq!(parse_file_name("LOCK"), Some((FileType::Lock, 0)));
        assert_eq!(parse_file_name("000009.dbtmp"), Some((FileType::Temp, 9)));
        for bad in [
            "foo",
            "12a.log",
            ".log",
            "MANIFEST-",
            "000001.txt",
            "-1.sst",
        ] {
            assert_eq!(parse_file_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn current_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        set_current_file(dir.path(), 42).unwrap();
        assert_eq!(read_current_file(dir.path()).unwrap(), "MANIFEST-000042");
        fs::write(current_file_name(dir.path()), "garbage").unwrap();
        assert!(read_current_file(dir.path()).is_err());
    }
}
