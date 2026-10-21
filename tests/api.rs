//! End-to-end tests of the public API.

mod common;

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;
use std::thread;

use common::{Rng, key, small_options, value};
use lsm_engine::{Db, Error, Options, ReadOptions, WriteBatch, WriteOptions};

fn collect(db: &Db) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.range::<Vec<u8>, _>(..).map(Result::unwrap).collect()
}

#[test]
fn put_get_delete_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), Options::default()).unwrap();
    assert_eq!(db.get("missing").unwrap(), None);
    db.put("a", "1").unwrap();
    db.put("a", "2").unwrap();
    assert_eq!(db.get("a").unwrap(), Some(b"2".to_vec()));
    db.delete("a").unwrap();
    assert_eq!(db.get("a").unwrap(), None);
    db.delete("never-existed").unwrap();
    db.put("", "empty key").unwrap();
    db.put("empty value", "").unwrap();
    assert_eq!(db.get("").unwrap(), Some(b"empty key".to_vec()));
    assert_eq!(db.get("empty value").unwrap(), Some(Vec::new()));
}

#[test]
fn data_survives_reopen_from_wal_and_tables() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path(), small_options()).unwrap();
        for i in 0..3_000 {
            db.put(key(i), value(i, 0)).unwrap();
        }
        for i in (0..3_000).step_by(3) {
            db.delete(key(i)).unwrap();
        }
        // Some data is in tables, the tail only in the WAL.
        db.close().unwrap();
    }
    let db = Db::open(dir.path(), small_options()).unwrap();
    for i in 0..3_000 {
        let expected = (i % 3 != 0).then(|| value(i, 0));
        assert_eq!(db.get(key(i)).unwrap(), expected, "key {i}");
    }
    assert!(db.stats().recovery.batches_replayed > 0);
}

#[test]
fn large_values_span_many_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small_options()).unwrap();
    let big: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    db.put("big", &big).unwrap();
    db.put("small", "x").unwrap();
    db.flush().unwrap();
    assert_eq!(db.get("big").unwrap().as_deref(), Some(&big[..]));
    drop(db);
    let db = Db::open(dir.path(), small_options()).unwrap();
    assert_eq!(db.get("big").unwrap().as_deref(), Some(&big[..]));
    assert_eq!(db.get("small").unwrap(), Some(b"x".to_vec()));
}

#[test]
fn write_batch_is_atomic_and_ordered() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), Options::default()).unwrap();
    db.put("x", "old").unwrap();
    let mut batch = WriteBatch::new();
    batch
        .put("x", "first")
        .put("y", "1")
        .delete("x")
        .put("x", "last");
    assert_eq!(batch.count(), 4);
    db.write_opt(batch, &WriteOptions { sync: true }).unwrap();
    assert_eq!(db.get("x").unwrap(), Some(b"last".to_vec()));
    assert_eq!(db.get("y").unwrap(), Some(b"1".to_vec()));
    // Empty batches are a no-op.
    let seq = db.stats().last_sequence;
    db.write(WriteBatch::new()).unwrap();
    assert_eq!(db.stats().last_sequence, seq);
}

#[test]
fn range_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small_options()).unwrap();
    for k in ["a", "b", "c", "d", "e"] {
        db.put(k, k.to_uppercase()).unwrap();
    }
    db.flush().unwrap();
    db.put("c", "C2").unwrap();
    db.delete("d").unwrap();

    let keys = |it: lsm_engine::RangeIter| -> Vec<String> {
        it.map(|kv| String::from_utf8(kv.unwrap().0).unwrap())
            .collect()
    };
    assert_eq!(keys(db.range("b".."d")), ["b", "c"]);
    assert_eq!(keys(db.range("b"..="e")), ["b", "c", "e"]);
    assert_eq!(keys(db.range(.."c")), ["a", "b"]);
    assert_eq!(keys(db.range("c"..)), ["c", "e"]);
    assert_eq!(
        keys(db.range::<&str, _>((Bound::Excluded("a"), Bound::Excluded("e")))),
        ["b", "c"]
    );
    assert_eq!(keys(db.range("x"..)), Vec::<String>::new());
    let all = collect(&db);
    assert_eq!(all[2], (b"c".to_vec(), b"C2".to_vec()));
}

#[test]
fn iterator_seek_and_scan_across_levels() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small_options()).unwrap();
    let mut model = BTreeMap::new();
    let mut rng = Rng::new(7);
    for generation in 0..4 {
        for _ in 0..2_000 {
            let i = rng.below(1_500);
            if rng.below(5) == 0 {
                db.delete(key(i)).unwrap();
                model.remove(&key(i));
            } else {
                db.put(key(i), value(i, generation)).unwrap();
                model.insert(key(i), value(i, generation));
            }
        }
    }
    let stats = db.stats();
    assert!(
        stats.levels.iter().filter(|l| l.files > 0).count() >= 2,
        "{stats}"
    );

    let mut it = db.iter().unwrap();
    it.seek_to_first().unwrap();
    let mut got = Vec::new();
    while it.valid() {
        got.push((it.key().to_vec(), it.value().to_vec()));
        it.next().unwrap();
    }
    assert_eq!(got, model.clone().into_iter().collect::<Vec<_>>());

    for probe in [0u64, 1, 499, 750, 1_499, 2_000] {
        it.seek(&key(probe)).unwrap();
        let expected = model.range(key(probe)..).next();
        match expected {
            Some((k, v)) => {
                assert!(it.valid());
                assert_eq!((it.key(), it.value()), (&k[..], &v[..]));
            }
            None => assert!(!it.valid()),
        }
    }
}

#[test]
fn snapshots_are_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small_options()).unwrap();
    for i in 0..500 {
        db.put(key(i), value(i, 0)).unwrap();
    }
    let snap = db.snapshot();
    for i in 0..500 {
        if i % 2 == 0 {
            db.delete(key(i)).unwrap();
        } else {
            db.put(key(i), value(i, 1)).unwrap();
        }
    }
    db.put("new", "key").unwrap();
    db.compact_range(None, None).unwrap();

    let ro = ReadOptions::at_snapshot(&snap);
    for i in 0..500 {
        assert_eq!(db.get_opt(key(i), &ro).unwrap(), Some(value(i, 0)));
    }
    assert_eq!(db.get_opt("new", &ro).unwrap(), None);
    let at_snap: Vec<_> = db
        .range_opt::<Vec<u8>, _>(.., &ro)
        .map(Result::unwrap)
        .collect();
    assert_eq!(at_snap.len(), 500);
    assert_eq!(collect(&db).len(), 251); // 250 odd keys + "new"
    assert_eq!(db.num_snapshots(), 1);
    drop(snap);
    assert_eq!(db.num_snapshots(), 0);
}

#[test]
fn compaction_drops_tombstones_and_old_versions() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small_options()).unwrap();
    for generation in 0..5 {
        for i in 0..1_000 {
            db.put(key(i), value(i, generation)).unwrap();
        }
    }
    for i in 0..1_000 {
        db.delete(key(i)).unwrap();
    }
    db.compact_range(None, None).unwrap();
    let stats = db.stats();
    let total_bytes: u64 = stats.levels.iter().map(|l| l.bytes).sum();
    assert_eq!(
        total_bytes, 0,
        "all data deleted, tables should be gone: {stats}"
    );
    assert!(collect(&db).is_empty());
}

#[test]
fn concurrent_writers_and_readers() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path(), small_options()).unwrap());
    let writers: Vec<_> = (0..4u64)
        .map(|t| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                for i in 0..1_000u64 {
                    let k = t * 1_000_000 + i;
                    db.put(key(k), value(k, t)).unwrap();
                }
            })
        })
        .collect();
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                for _ in 0..20 {
                    let n = db.range::<Vec<u8>, _>(..).map(Result::unwrap).count();
                    assert!(n <= 4_000);
                }
            })
        })
        .collect();
    for h in writers.into_iter().chain(readers) {
        h.join().unwrap();
    }
    for t in 0..4u64 {
        for i in 0..1_000u64 {
            let k = t * 1_000_000 + i;
            assert_eq!(db.get(key(k)).unwrap(), Some(value(k, t)));
        }
    }
}

/// Transfers between accounts preserve the total; any consistent read view
/// (snapshot or iterator) must observe exactly the initial total. This
/// checks batch atomicity and MVCC isolation under concurrency.
#[test]
fn atomic_batches_preserve_invariant_under_concurrency() {
    const ACCOUNTS: u64 = 50;
    const INITIAL: i64 = 1_000;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path(), small_options()).unwrap());
    let mut init = WriteBatch::new();
    for a in 0..ACCOUNTS {
        init.put(key(a), INITIAL.to_le_bytes());
    }
    db.write(init).unwrap();

    let writer = {
        let db = Arc::clone(&db);
        thread::spawn(move || {
            let mut rng = Rng::new(42);
            for _ in 0..3_000 {
                let (from, to) = (rng.below(ACCOUNTS), rng.below(ACCOUNTS));
                if from == to {
                    continue;
                }
                let snap = db.snapshot();
                let ro = ReadOptions::at_snapshot(&snap);
                let read = |a| {
                    i64::from_le_bytes(
                        db.get_opt(key(a), &ro)
                            .unwrap()
                            .unwrap()
                            .try_into()
                            .unwrap(),
                    )
                };
                let amount = rng.below(10) as i64;
                let mut b = WriteBatch::new();
                b.put(key(from), (read(from) - amount).to_le_bytes());
                b.put(key(to), (read(to) + amount).to_le_bytes());
                // Padding forces frequent flushes and compactions.
                b.put(format!("pad{}", rng.below(10_000)), [0u8; 64]);
                db.write(b).unwrap();
            }
        })
    };
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                for _ in 0..200 {
                    let snap = db.snapshot();
                    let ro = ReadOptions::at_snapshot(&snap);
                    let total: i64 = (0..ACCOUNTS)
                        .map(|a| {
                            let v = db.get_opt(key(a), &ro).unwrap().unwrap();
                            i64::from_le_bytes(v.try_into().unwrap())
                        })
                        .sum();
                    assert_eq!(total, INITIAL * ACCOUNTS as i64);

                    let scanned: i64 = db
                        .range(key(0)..key(ACCOUNTS))
                        .map(|kv| i64::from_le_bytes(kv.unwrap().1.try_into().unwrap()))
                        .sum();
                    assert_eq!(scanned, INITIAL * ACCOUNTS as i64);
                }
            })
        })
        .collect();
    writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }
}

#[test]
fn open_options_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let no_create = Options {
        create_if_missing: false,
        ..Options::default()
    };
    assert!(matches!(
        Db::open(&path, no_create.clone()),
        Err(Error::InvalidArgument(_))
    ));
    drop(Db::open(&path, Options::default()).unwrap());
    let exclusive = Options {
        error_if_exists: true,
        ..Options::default()
    };
    assert!(matches!(
        Db::open(&path, exclusive),
        Err(Error::InvalidArgument(_))
    ));
    assert!(Db::open(&path, no_create).is_ok());

    let bad = Options {
        max_levels: 1,
        ..Options::default()
    };
    assert!(matches!(
        Db::open(dir.path().join("x"), bad),
        Err(Error::InvalidArgument(_))
    ));
}

#[test]
fn bloom_filters_skip_tables_for_missing_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small_options()).unwrap();
    for i in 0..5_000 {
        db.put(key(i * 2), value(i, 0)).unwrap();
    }
    db.compact_range(None, None).unwrap();
    let before = db.stats().bloom_filter_negatives;
    for i in 0..1_000 {
        assert_eq!(db.get(key(i * 2 + 1)).unwrap(), None);
    }
    let skipped = db.stats().bloom_filter_negatives - before;
    assert!(
        skipped > 900,
        "bloom filter skipped only {skipped} of 1000 probes"
    );
}

#[test]
fn works_without_cache_or_filters() {
    let dir = tempfile::tempdir().unwrap();
    let opts = Options {
        block_cache_capacity: 0,
        bloom_bits_per_key: 0,
        ..small_options()
    };
    let db = Db::open(dir.path(), opts).unwrap();
    for i in 0..2_000 {
        db.put(key(i), value(i, 0)).unwrap();
    }
    db.compact_range(None, None).unwrap();
    for i in 0..2_000 {
        assert_eq!(db.get(key(i)).unwrap(), Some(value(i, 0)));
    }
}
