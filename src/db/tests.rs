//! White-box tests of engine internals. Black-box behaviour is covered by
//! the integration tests under `tests/`.

use super::*;

fn small_opts() -> Options {
    Options {
        write_buffer_size: 16 << 10,
        target_file_size: 16 << 10,
        block_size: 512,
        max_bytes_for_level_base: 32 << 10,
        l0_compaction_trigger: 2,
        l0_slowdown_writes_trigger: 4,
        l0_stop_writes_trigger: 8,
        ..Options::default()
    }
}

fn count_files(dir: &Path, kind: FileType) -> usize {
    list_db_files(dir)
        .unwrap()
        .into_iter()
        .filter(|(t, _)| *t == kind)
        .count()
}

#[test]
fn obsolete_files_are_collected() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small_opts()).unwrap();
    for i in 0..5_000u32 {
        db.put(format!("key{:05}", i % 1000), format!("value{i}"))
            .unwrap();
    }
    db.compact_range(None, None).unwrap();
    db.wait_for_compactions().unwrap();
    let live: usize = db.stats().levels.iter().map(|l| l.files).sum();
    assert_eq!(count_files(dir.path(), FileType::Table), live);
    assert_eq!(count_files(dir.path(), FileType::Log), 1);
    assert_eq!(count_files(dir.path(), FileType::Manifest), 1);
}

#[test]
fn iterator_pins_files_across_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small_opts()).unwrap();
    for i in 0..2_000u32 {
        db.put(format!("key{i:05}"), vec![b'x'; 50]).unwrap();
    }
    db.flush().unwrap();
    let mut it = db.iter().unwrap();
    it.seek_to_first().unwrap();
    // Rewrite and compact everything while the iterator is open.
    for i in 0..2_000u32 {
        db.put(format!("key{i:05}"), vec![b'y'; 50]).unwrap();
    }
    db.compact_range(None, None).unwrap();
    let mut n = 0;
    while it.valid() {
        assert_eq!(it.value(), &[b'x'; 50][..]);
        n += 1;
        it.next().unwrap();
    }
    assert_eq!(n, 2_000);
}

#[test]
fn snapshot_preserves_versions_through_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small_opts()).unwrap();
    db.put("k", "v1").unwrap();
    let snap = db.snapshot();
    db.put("k", "v2").unwrap();
    db.delete("gone").unwrap();
    for i in 0..3_000u32 {
        db.put(format!("filler{i:05}"), vec![0u8; 40]).unwrap();
    }
    db.compact_range(None, None).unwrap();
    assert_eq!(
        db.get_opt("k", &ReadOptions::at_snapshot(&snap)).unwrap(),
        Some(b"v1".to_vec())
    );
    assert_eq!(db.get("k").unwrap(), Some(b"v2".to_vec()));
    assert_eq!(db.inner.snapshots.len(), 1);
    drop(snap);
    assert_eq!(db.inner.snapshots.len(), 0);
}

#[test]
fn write_amplification_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let opts = Options {
        write_buffer_size: 64 << 10,
        ..small_opts()
    };
    let db = Db::open(dir.path(), opts).unwrap();
    for i in 0..20_000u32 {
        db.put(
            format!("key{:08}", (i.wrapping_mul(2_654_435_761)) % 20_000),
            [0u8; 32],
        )
        .unwrap();
    }
    db.wait_for_compactions().unwrap();
    let s = db.stats();
    assert!(s.flushes > 0 && s.compactions > 0, "{s}");
    assert!(s.write_amplification() > 1.0);
    assert!(s.write_amplification_with_wal() > s.write_amplification());
}

#[test]
fn second_open_is_rejected_while_locked() {
    let dir = tempfile::tempdir().unwrap();
    let _db = Db::open(dir.path(), Options::default()).unwrap();
    let err = Db::open(dir.path(), Options::default()).unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
}
