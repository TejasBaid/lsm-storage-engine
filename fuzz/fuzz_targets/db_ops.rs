//! Differential fuzzing: arbitrary operation sequences applied to the engine
//! and to a `BTreeMap` must produce identical observations.
#![no_main]

use std::collections::BTreeMap;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use lsm_engine::{Db, Options, ReadOptions, WriteBatch};

#[derive(Arbitrary, Debug)]
enum Op {
    Put(u8, Vec<u8>),
    Delete(u8),
    Batch(Vec<(u8, Option<Vec<u8>>)>),
    Get(u8),
    Scan(u8, u8),
    SnapshotRead(u8),
    Flush,
    Compact,
    Reopen,
}

fn key(k: u8) -> Vec<u8> {
    vec![b'k', k % 64]
}

fn opts() -> Options {
    Options {
        write_buffer_size: 1024,
        target_file_size: 2048,
        block_size: 128,
        max_bytes_for_level_base: 8 << 10,
        l0_compaction_trigger: 2,
        ..Options::default()
    }
}

fuzz_target!(|ops: Vec<Op>| {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Some(Db::open(dir.path(), opts()).unwrap());
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for op in ops.into_iter().take(200) {
        let d = db.as_ref().unwrap();
        match op {
            Op::Put(k, v) => {
                d.put(key(k), &v).unwrap();
                model.insert(key(k), v);
            }
            Op::Delete(k) => {
                d.delete(key(k)).unwrap();
                model.remove(&key(k));
            }
            Op::Batch(entries) => {
                let mut b = WriteBatch::new();
                for (k, v) in entries {
                    match v {
                        Some(v) => {
                            b.put(key(k), &v);
                            model.insert(key(k), v);
                        }
                        None => {
                            b.delete(key(k));
                            model.remove(&key(k));
                        }
                    }
                }
                d.write(b).unwrap();
            }
            Op::Get(k) => assert_eq!(d.get(key(k)).unwrap(), model.get(&key(k)).cloned()),
            Op::Scan(a, b) => {
                let (x, y) = (key(a), key(b));
                let (lo, hi) = if x <= y { (x, y) } else { (y, x) };
                let got: Vec<_> = d.range(lo.clone()..hi.clone()).map(Result::unwrap).collect();
                let want: Vec<_> = model.range(lo..hi).map(|(k, v)| (k.clone(), v.clone())).collect();
                assert_eq!(got, want);
            }
            Op::SnapshotRead(k) => {
                let snap = d.snapshot();
                d.put(key(k), b"after-snapshot").unwrap();
                let got = d.get_opt(key(k), &ReadOptions::at_snapshot(&snap)).unwrap();
                assert_eq!(got, model.get(&key(k)).cloned());
                model.insert(key(k), b"after-snapshot".to_vec());
            }
            Op::Flush => d.flush().unwrap(),
            Op::Compact => d.compact_range(None, None).unwrap(),
            Op::Reopen => {
                drop(db.take());
                db = Some(Db::open(dir.path(), opts()).unwrap());
            }
        }
    }
});
