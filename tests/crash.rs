//! Crash-recovery tests.
//!
//! * Simulated crashes: copy a database directory and damage the WAL the
//!   way an interrupted append would (truncation at arbitrary byte offsets,
//!   garbage or zero-filled tails), then reopen and check that exactly a
//!   prefix of the acknowledged batches is recovered, each batch entirely.
//! * Real crashes: a child process writes continuously while flushes and
//!   compactions run, and is killed with SIGKILL at a random moment. Every
//!   write it reported as acknowledged must be present after reopening.

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::{Rng, small_options};
use lsm_engine::internal::{FileType, parse_file_name};
use lsm_engine::{Db, Options, WalRecoveryMode, WriteBatch};

const BATCHES: u64 = 40;

/// Options that keep everything in the WAL (no flush before the "crash").
fn wal_only_options() -> Options {
    Options {
        write_buffer_size: 64 << 20,
        ..Options::default()
    }
}

/// Batch `i` writes keys `b{i}-0 ..= b{i}-{i % 3}`.
fn batch_keys(i: u64) -> Vec<Vec<u8>> {
    (0..=i % 3)
        .map(|j| format!("b{i:04}-{j}").into_bytes())
        .collect()
}

fn write_batches(dir: &Path) {
    let db = Db::open(dir, wal_only_options()).unwrap();
    for i in 0..BATCHES {
        let mut batch = WriteBatch::new();
        for k in batch_keys(i) {
            batch.put(&k, format!("value-for-{}", String::from_utf8_lossy(&k)));
        }
        db.write(batch).unwrap();
    }
    // Dropping without flushing leaves every batch only in the WAL.
}

fn only_file(dir: &Path, kind: FileType) -> PathBuf {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(parse_file_name)
                .is_some_and(|(t, _)| t == kind)
        })
        .collect();
    found.sort();
    found.pop().expect("file of requested kind")
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "LOCK" {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

/// Number of batches recovered; panics unless they form a prefix
/// `0..n` with every batch complete.
fn recovered_prefix(db: &Db) -> u64 {
    let mut n = 0;
    while n < BATCHES {
        let present: Vec<bool> = batch_keys(n)
            .iter()
            .map(|k| db.get(k).unwrap().is_some())
            .collect();
        if present.iter().all(|&p| p) {
            n += 1;
        } else {
            assert!(
                present.iter().all(|&p| !p),
                "batch {n} partially recovered: {present:?}"
            );
            break;
        }
    }
    for later in n..BATCHES {
        for k in batch_keys(later) {
            assert!(
                db.get(&k).unwrap().is_none(),
                "batch {later} recovered after gap at {n}"
            );
        }
    }
    n
}

#[test]
fn torn_wal_tail_recovers_a_prefix_at_every_cut_point() {
    let base = tempfile::tempdir().unwrap();
    let original = base.path().join("original");
    write_batches(&original);
    let log = only_file(&original, FileType::Log);
    let full = fs::read(&log).unwrap();
    assert!(!full.is_empty());

    // Every byte of the last two records, and a stride through the rest.
    let tail_start = full.len().saturating_sub(120);
    let cuts: Vec<usize> = (0..tail_start)
        .step_by(37)
        .chain(tail_start..=full.len())
        .collect();
    let mut last = 0;
    for (i, &cut) in cuts.iter().enumerate() {
        let dir = base.path().join(format!("cut{i}"));
        copy_dir(&original, &dir);
        fs::write(dir.join(log.file_name().unwrap()), &full[..cut]).unwrap();
        let db = Db::open(&dir, wal_only_options()).unwrap();
        let n = recovered_prefix(&db);
        assert!(
            n >= last,
            "recovered prefix shrank from {last} to {n} at cut {cut}"
        );
        last = n;
        let dropped = db.stats().recovery.bytes_dropped;
        if cut == full.len() {
            assert_eq!(n, BATCHES);
            assert_eq!(dropped, 0);
        }
        drop(db);
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn garbage_and_zero_tails_are_dropped() {
    let base = tempfile::tempdir().unwrap();
    let original = base.path().join("original");
    write_batches(&original);
    let log_name = only_file(&original, FileType::Log)
        .file_name()
        .unwrap()
        .to_owned();

    let mut rng = Rng::new(1);
    let garbage: Vec<u8> = (0..300).map(|_| rng.next_u64() as u8).collect();
    for (name, tail) in [("garbage", garbage), ("zeros", vec![0u8; 4096])] {
        let dir = base.path().join(name);
        copy_dir(&original, &dir);
        let path = dir.join(&log_name);
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(&tail);
        fs::write(&path, bytes).unwrap();
        let db = Db::open(&dir, wal_only_options()).unwrap();
        assert_eq!(recovered_prefix(&db), BATCHES, "{name}");
        assert_eq!(
            db.stats().recovery.bytes_dropped,
            tail.len() as u64,
            "{name}"
        );
    }
}

#[test]
fn mid_log_corruption_respects_recovery_mode() {
    let base = tempfile::tempdir().unwrap();
    let original = base.path().join("original");
    write_batches(&original);
    let log_name = only_file(&original, FileType::Log)
        .file_name()
        .unwrap()
        .to_owned();
    let mut bytes = fs::read(original.join(&log_name)).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x40;

    let open_with = |name: &str, mode: WalRecoveryMode| {
        let dir = base.path().join(name);
        copy_dir(&original, &dir);
        fs::write(dir.join(&log_name), &bytes).unwrap();
        let opts = Options {
            wal_recovery_mode: mode,
            ..wal_only_options()
        };
        Db::open(&dir, opts)
    };

    // Default: damage that is not at the tail is real corruption.
    let err = open_with("default", WalRecoveryMode::TolerateCorruptedTailRecords).unwrap_err();
    assert!(err.is_corruption(), "{err}");
    let err = open_with("strict", WalRecoveryMode::AbsoluteConsistency).unwrap_err();
    assert!(err.is_corruption(), "{err}");

    // Point-in-time: recover the consistent prefix before the damage.
    let db = open_with("pit", WalRecoveryMode::PointInTimeRecovery).unwrap();
    let n = recovered_prefix(&db);
    assert!(n > 0 && n < BATCHES, "recovered {n}");
}

#[test]
fn strict_mode_rejects_torn_tail() {
    let base = tempfile::tempdir().unwrap();
    let original = base.path().join("original");
    write_batches(&original);
    let log = only_file(&original, FileType::Log);
    let bytes = fs::read(&log).unwrap();
    fs::write(&log, &bytes[..bytes.len() - 3]).unwrap();
    let opts = Options {
        wal_recovery_mode: WalRecoveryMode::AbsoluteConsistency,
        ..wal_only_options()
    };
    assert!(Db::open(&original, opts).unwrap_err().is_corruption());
    // The default mode accepts it and loses only the torn batch.
    let db = Db::open(&original, wal_only_options()).unwrap();
    assert_eq!(recovered_prefix(&db), BATCHES - 1);
}

#[test]
fn orphaned_tables_from_interrupted_flush_are_removed() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path(), small_options()).unwrap();
        for i in 0..2_000u64 {
            db.put(common::key(i), common::value(i, 0)).unwrap();
        }
    }
    // A table written by a flush that crashed before its manifest commit.
    let orphan = dir.path().join("999999.sst");
    fs::write(&orphan, b"partially written table").unwrap();
    let db = Db::open(dir.path(), small_options()).unwrap();
    assert!(!orphan.exists());
    for i in 0..2_000u64 {
        assert_eq!(db.get(common::key(i)).unwrap(), Some(common::value(i, 0)));
    }
}

#[test]
fn corrupted_table_is_reported_not_silently_ignored() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path(), small_options()).unwrap();
        for i in 0..500u64 {
            db.put(common::key(i), common::value(i, 0)).unwrap();
        }
        db.compact_range(None, None).unwrap();
    }
    let table = only_file(dir.path(), FileType::Table);
    let mut bytes = fs::read(&table).unwrap();
    bytes[20] ^= 0xff;
    fs::write(&table, bytes).unwrap();
    let db = Db::open(dir.path(), small_options()).unwrap();
    let errors = (0..500u64)
        .filter_map(|i| db.get(common::key(i)).err())
        .collect::<Vec<_>>();
    assert!(!errors.is_empty());
    assert!(errors.iter().all(|e| e.is_corruption()), "{errors:?}");
}

// ---------------------------------------------------------------------------
// Real process crashes.

const CHILD_ENV: &str = "LSM_CRASH_CHILD_DIR";

fn seq_key(i: u64) -> Vec<u8> {
    format!("seq{i:010}").into_bytes()
}

fn seq_value(i: u64) -> Vec<u8> {
    let mut v = format!("payload-{i}-").into_bytes();
    v.resize(64 + (i % 64) as usize, b'z');
    v
}

/// Child-process body: resumes after the last recovered batch and writes
/// forever, reporting progress on stdout. Run only via the parent test.
#[test]
#[ignore = "helper process for survives_sigkill_during_writes"]
fn crash_child_writer() {
    let Ok(dir) = std::env::var(CHILD_ENV) else {
        return;
    };
    let db = Db::open(&dir, small_options()).unwrap();
    let mut next = db
        .get("last")
        .unwrap()
        .map(|v| u64::from_le_bytes(v.try_into().unwrap()) + 1)
        .unwrap_or(0);
    println!("ready {next}");
    loop {
        let mut batch = WriteBatch::new();
        batch.put(seq_key(next), seq_value(next));
        batch.put("last", next.to_le_bytes());
        db.write(batch).unwrap();
        if next.is_multiple_of(25) {
            // Acknowledged: `write` returned, so the batch must survive.
            println!("ack {next}");
        }
        next += 1;
    }
}

#[test]
fn survives_sigkill_during_writes() {
    let dir = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let seed = std::env::var("LSM_CRASH_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed);
    let mut rng = Rng::new(seed);
    let mut total_acked = 0u64;

    for round in 0..6 {
        let mut child = Command::new(&exe)
            .args([
                "--exact",
                "crash_child_writer",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        // Skip the test harness's own output until the child is ready.
        loop {
            let line = rx
                .recv_timeout(Duration::from_secs(60))
                .expect("child started");
            if line.contains("ready ") {
                break;
            }
        }

        std::thread::sleep(Duration::from_millis(200 + rng.below(600)));
        child.kill().unwrap(); // SIGKILL on Unix
        child.wait().unwrap();
        reader.join().unwrap();

        let acked = rx
            .try_iter()
            .filter_map(|l| l.strip_prefix("ack ").and_then(|n| n.parse::<u64>().ok()))
            .max();

        let db = Db::open(dir.path(), small_options()).unwrap();
        let last = db
            .get("last")
            .unwrap()
            .map(|v| u64::from_le_bytes(v.try_into().unwrap()));
        if let Some(acked) = acked {
            let last = last.expect("acknowledged writes lost");
            assert!(
                last >= acked,
                "round {round}: recovered up to {last}, acked {acked}"
            );
            total_acked = acked;
        }
        if let Some(last) = last {
            for i in 0..=last {
                assert_eq!(
                    db.get(seq_key(i)).unwrap(),
                    Some(seq_value(i)),
                    "round {round} key {i}"
                );
            }
            assert_eq!(db.get(seq_key(last + 1)).unwrap(), None);
        }
    }
    assert!(total_acked > 0, "child never acknowledged a write");
}
