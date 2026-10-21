//! Append-only, checksummed record log.
//!
//! Used for both the write-ahead log (one record per [`WriteBatch`]) and the
//! manifest (one record per [`VersionEdit`]).
//!
//! ```text
//! record := crc32 (u32 LE) | length (u32 LE) | payload (length bytes)
//! crc32  := CRC-32 (IEEE) over the 4 length bytes followed by the payload
//! ```
//!
//! A crash during an append leaves a *torn* final record: a partial header,
//! a partial payload, or (on some file systems) a zero-filled tail. The
//! reader classifies such damage as a tail fault and, depending on
//! [`WalRecoveryMode`], drops it rather than failing recovery.
//!
//! [`WriteBatch`]: crate::WriteBatch
//! [`VersionEdit`]: crate::version::edit::VersionEdit

use std::fs::File;
use std::io::{BufWriter, Write};

use crate::coding::decode_fixed32;
use crate::error::{Error, Result};
use crate::options::WalRecoveryMode;

/// Size of the per-record header (checksum + length).
pub const HEADER_SIZE: usize = 8;

fn record_crc(len_bytes: &[u8; 4], payload: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(len_bytes);
    h.update(payload);
    h.finalize()
}

/// Appends records to a log file.
pub struct LogWriter {
    file: BufWriter<File>,
    bytes_written: u64,
}

impl LogWriter {
    /// Wraps a freshly created (empty) file.
    pub fn new(file: File) -> Self {
        LogWriter {
            file: BufWriter::with_capacity(64 << 10, file),
            bytes_written: 0,
        }
    }

    /// Appends one record and hands it to the OS (a single `write` syscall
    /// for typical sizes). Call [`LogWriter::sync`] for power-loss durability.
    pub fn add_record(&mut self, payload: &[u8]) -> Result<()> {
        let len = u32::try_from(payload.len())
            .map_err(|_| Error::InvalidArgument("log record exceeds 4 GiB".into()))?;
        let len_bytes = len.to_le_bytes();
        let crc = record_crc(&len_bytes, payload);
        self.file.write_all(&crc.to_le_bytes())?;
        self.file.write_all(&len_bytes)?;
        self.file.write_all(payload)?;
        self.file.flush()?;
        self.bytes_written += (HEADER_SIZE + payload.len()) as u64;
        Ok(())
    }

    /// Flushes buffered data and `fdatasync`s the file.
    pub fn sync(&mut self) -> Result<()> {
        self.file.flush()?;
        self.file.get_ref().sync_data()?;
        Ok(())
    }

    /// Total bytes appended through this writer (headers included).
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

/// Why a record could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    TruncatedHeader,
    TruncatedPayload,
    ChecksumMismatch,
}

/// Reads records back from an in-memory copy of a log file.
///
/// Logs are bounded by the memtable size (and manifests are small), so
/// reading the whole file up front keeps the parser simple and lets it look
/// ahead to classify damage as a torn tail or mid-log corruption.
pub struct LogReader {
    data: Vec<u8>,
    pos: usize,
    mode: WalRecoveryMode,
    dropped_bytes: u64,
    stopped_on_corruption: bool,
}

impl LogReader {
    /// Creates a reader over the full contents of a log file.
    pub fn new(data: Vec<u8>, mode: WalRecoveryMode) -> Self {
        LogReader {
            data,
            pos: 0,
            mode,
            dropped_bytes: 0,
            stopped_on_corruption: false,
        }
    }

    /// Bytes skipped because of torn or corrupt records.
    pub fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }

    /// True if the reader stopped early because of a damaged record.
    pub fn stopped_on_corruption(&self) -> bool {
        self.stopped_on_corruption
    }

    /// Returns the next intact record, `Ok(None)` at the end of the log, or
    /// an error if damage is found that the recovery mode does not tolerate.
    pub fn next_record(&mut self) -> Result<Option<&[u8]>> {
        let pos = self.pos;
        let remaining = self.data.len() - pos;
        if remaining == 0 {
            return Ok(None);
        }
        if remaining < HEADER_SIZE {
            return self.on_fault(pos, Fault::TruncatedHeader, true);
        }
        let crc = decode_fixed32(&self.data[pos..]);
        let len_bytes: [u8; 4] = self.data[pos + 4..pos + 8].try_into().expect("4 bytes");
        let len = u32::from_le_bytes(len_bytes) as usize;
        if len > remaining - HEADER_SIZE {
            return self.on_fault(pos, Fault::TruncatedPayload, true);
        }
        let end = pos + HEADER_SIZE + len;
        if record_crc(&len_bytes, &self.data[pos + HEADER_SIZE..end]) != crc {
            // A bad record that extends to EOF, or that is followed only by
            // zeros, is the signature of an interrupted append.
            let is_tail = end == self.data.len() || self.data[pos..].iter().all(|&b| b == 0);
            return self.on_fault(pos, Fault::ChecksumMismatch, is_tail);
        }
        self.pos = end;
        Ok(Some(&self.data[pos + HEADER_SIZE..end]))
    }

    fn on_fault(&mut self, pos: usize, fault: Fault, is_tail: bool) -> Result<Option<&[u8]>> {
        let tolerate = match self.mode {
            WalRecoveryMode::AbsoluteConsistency => false,
            WalRecoveryMode::TolerateCorruptedTailRecords => is_tail,
            WalRecoveryMode::PointInTimeRecovery => true,
        };
        if !tolerate {
            return Err(Error::corruption(format!(
                "log record at offset {pos}: {fault:?} ({} of {} bytes remain)",
                self.data.len() - pos,
                self.data.len()
            )));
        }
        self.dropped_bytes += (self.data.len() - pos) as u64;
        self.stopped_on_corruption = true;
        self.pos = self.data.len();
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn write_records(records: &[Vec<u8>]) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let mut w = LogWriter::new(File::create(&path).unwrap());
        for r in records {
            w.add_record(r).unwrap();
        }
        w.sync().unwrap();
        std::fs::read(&path).unwrap()
    }

    fn read_all(data: Vec<u8>, mode: WalRecoveryMode) -> Result<(Vec<Vec<u8>>, u64)> {
        let mut r = LogReader::new(data, mode);
        let mut out = Vec::new();
        while let Some(rec) = r.next_record()? {
            out.push(rec.to_vec());
        }
        Ok((out, r.dropped_bytes()))
    }

    #[test]
    fn roundtrip_including_empty_record() {
        let recs = vec![b"hello".to_vec(), Vec::new(), vec![7u8; 100_000]];
        let (got, dropped) = read_all(write_records(&recs), WalRecoveryMode::default()).unwrap();
        assert_eq!(got, recs);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn torn_tail_is_dropped_at_every_cut_point() {
        let recs: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 10 + i as usize]).collect();
        let full = write_records(&recs);
        let boundaries: Vec<usize> = recs
            .iter()
            .scan(0, |off, r| {
                *off += HEADER_SIZE + r.len();
                Some(*off)
            })
            .collect();
        for cut in 0..full.len() {
            let (got, dropped) =
                read_all(full[..cut].to_vec(), WalRecoveryMode::default()).unwrap();
            let complete = boundaries.iter().filter(|&&b| b <= cut).count();
            assert_eq!(got, recs[..complete].to_vec(), "cut at {cut}");
            let expected_drop = cut
                - if complete == 0 {
                    0
                } else {
                    boundaries[complete - 1]
                };
            assert_eq!(dropped as usize, expected_drop);
            // Strict mode must reject any torn tail.
            let strict = read_all(full[..cut].to_vec(), WalRecoveryMode::AbsoluteConsistency);
            assert_eq!(strict.is_err(), expected_drop != 0);
        }
    }

    #[test]
    fn zero_filled_tail_is_tolerated() {
        let mut data = write_records(&[b"a".to_vec(), b"b".to_vec()]);
        data.extend(std::iter::repeat_n(0u8, 64));
        let (got, dropped) = read_all(data, WalRecoveryMode::default()).unwrap();
        assert_eq!(got, vec![b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(dropped, 64);
    }

    #[test]
    fn mid_log_corruption_depends_on_mode() {
        let recs = vec![vec![1u8; 20], vec![2u8; 20], vec![3u8; 20]];
        let mut data = write_records(&recs);
        data[HEADER_SIZE + 20 + HEADER_SIZE + 3] ^= 0xff; // inside record 2's payload
        assert!(read_all(data.clone(), WalRecoveryMode::default()).is_err());
        let (got, _) = read_all(data, WalRecoveryMode::PointInTimeRecovery).unwrap();
        assert_eq!(got, recs[..1].to_vec());
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..256)) {
            for mode in [WalRecoveryMode::default(), WalRecoveryMode::AbsoluteConsistency, WalRecoveryMode::PointInTimeRecovery] {
                let _ = read_all(data.clone(), mode);
            }
        }

        #[test]
        fn records_roundtrip(recs in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..300), 0..20)) {
            let (got, dropped) = read_all(write_records(&recs), WalRecoveryMode::AbsoluteConsistency).unwrap();
            prop_assert_eq!(got, recs);
            prop_assert_eq!(dropped, 0);
        }
    }
}
