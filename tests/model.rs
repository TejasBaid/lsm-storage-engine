//! Model-based property tests: random operation sequences are applied to
//! both the engine and a `BTreeMap`, and every observation must agree.
//! Tiny buffers make flushes, compactions and multi-level reads frequent;
//! reopen operations exercise recovery in the middle of a sequence.

mod common;

use std::collections::BTreeMap;
use std::path::Path;

use common::{Rng, small_options};
use lsm_engine::{Db, Options, ReadOptions, Snapshot, WriteBatch};
use proptest::prelude::*;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Debug, Clone)]
enum Op {
    Put(u8, u16),
    Delete(u8),
    Batch(Vec<(u8, Option<u16>)>),
    Get(u8),
    Scan(u8, u8),
    TakeSnapshot,
    ReadSnapshot(usize, u8),
    ScanSnapshot(usize),
    DropSnapshot(usize),
    Flush,
    Compact(u8, u8),
    Reopen,
}

fn k(i: u8) -> Vec<u8> {
    format!("key{:03}", i % 80).into_bytes()
}

fn v(seed: u16) -> Vec<u8> {
    let mut out = format!("v{seed}:").into_bytes();
    out.resize(out.len() + (seed % 300) as usize, b'#');
    out
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        30 => (any::<u8>(), any::<u16>()).prop_map(|(a, b)| Op::Put(a, b)),
        8 => any::<u8>().prop_map(Op::Delete),
        6 => proptest::collection::vec((any::<u8>(), proptest::option::of(any::<u16>())), 1..8)
            .prop_map(Op::Batch),
        12 => any::<u8>().prop_map(Op::Get),
        4 => (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Op::Scan(a, b)),
        2 => Just(Op::TakeSnapshot),
        4 => (any::<usize>(), any::<u8>()).prop_map(|(a, b)| Op::ReadSnapshot(a, b)),
        1 => any::<usize>().prop_map(Op::ScanSnapshot),
        1 => any::<usize>().prop_map(Op::DropSnapshot),
        1 => Just(Op::Flush),
        1 => (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Op::Compact(a, b)),
        1 => Just(Op::Reopen),
    ]
}

fn model_opts() -> Options {
    Options {
        write_buffer_size: 4 << 10,
        target_file_size: 4 << 10,
        max_bytes_for_level_base: 16 << 10,
        ..small_options()
    }
}

fn scan(db: &Db, ro: &ReadOptions<'_>, lo: &[u8], hi: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.range_opt(lo.to_vec()..hi.to_vec(), ro)
        .map(Result::unwrap)
        .collect()
}

fn model_scan(m: &Model, lo: &[u8], hi: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    if lo >= hi {
        return Vec::new();
    }
    m.range(lo.to_vec()..hi.to_vec())
        .map(|(a, b)| (a.clone(), b.clone()))
        .collect()
}

fn run(path: &Path, ops: Vec<Op>) -> Result<(), TestCaseError> {
    let mut db = Some(Db::open(path, model_opts()).unwrap());
    let mut model = Model::new();
    let mut snaps: Vec<(Snapshot, Model)> = Vec::new();

    for op in ops {
        let d = db.as_ref().unwrap();
        match op {
            Op::Put(a, b) => {
                d.put(k(a), v(b)).unwrap();
                model.insert(k(a), v(b));
            }
            Op::Delete(a) => {
                d.delete(k(a)).unwrap();
                model.remove(&k(a));
            }
            Op::Batch(entries) => {
                let mut batch = WriteBatch::new();
                for (a, val) in entries {
                    match val {
                        Some(b) => {
                            batch.put(k(a), v(b));
                            model.insert(k(a), v(b));
                        }
                        None => {
                            batch.delete(k(a));
                            model.remove(&k(a));
                        }
                    }
                }
                d.write(batch).unwrap();
            }
            Op::Get(a) => {
                prop_assert_eq!(d.get(k(a)).unwrap(), model.get(&k(a)).cloned());
            }
            Op::Scan(a, b) => {
                let (lo, hi) = (k(a), k(b));
                prop_assert_eq!(
                    scan(d, &ReadOptions::default(), &lo, &hi),
                    model_scan(&model, &lo, &hi)
                );
            }
            Op::TakeSnapshot => snaps.push((d.snapshot(), model.clone())),
            Op::ReadSnapshot(i, a) if !snaps.is_empty() => {
                let (snap, m) = &snaps[i % snaps.len()];
                let got = d.get_opt(k(a), &ReadOptions::at_snapshot(snap)).unwrap();
                prop_assert_eq!(got, m.get(&k(a)).cloned());
            }
            Op::ScanSnapshot(i) if !snaps.is_empty() => {
                let (snap, m) = &snaps[i % snaps.len()];
                let got: Vec<_> = d
                    .range_opt::<Vec<u8>, _>(.., &ReadOptions::at_snapshot(snap))
                    .map(Result::unwrap)
                    .collect();
                prop_assert_eq!(got, m.clone().into_iter().collect::<Vec<_>>());
            }
            Op::DropSnapshot(i) if !snaps.is_empty() => {
                let idx = i % snaps.len();
                snaps.remove(idx);
            }
            Op::ReadSnapshot(..) | Op::ScanSnapshot(_) | Op::DropSnapshot(_) => {}
            Op::Flush => d.flush().unwrap(),
            Op::Compact(a, b) => {
                let (lo, hi) = (k(a.min(b)), k(a.max(b)));
                d.compact_range(Some(&lo), Some(&hi)).unwrap();
            }
            Op::Reopen => {
                snaps.clear(); // snapshots do not survive a restart
                drop(db.take());
                db = Some(Db::open(path, model_opts()).unwrap());
            }
        }
    }

    let d = db.as_ref().unwrap();
    let all: Vec<_> = d.range::<Vec<u8>, _>(..).map(Result::unwrap).collect();
    prop_assert_eq!(all, model.into_iter().collect::<Vec<_>>());
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        // Override with e.g. PROPTEST_CASES=1000 for a deeper soak.
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(48),
        max_shrink_iters: 2_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn engine_matches_btreemap_model(ops in proptest::collection::vec(op_strategy(), 1..250)) {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), ops)?;
    }
}

/// A longer deterministic workload with periodic restarts and a final
/// full compaction, checked against the model throughout.
#[test]
fn long_random_workload_with_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = Rng::new(0xdecaf);
    let mut model = Model::new();
    let mut db = Db::open(dir.path(), small_options()).unwrap();
    for round in 0..6u64 {
        for _ in 0..4_000 {
            let i = rng.below(3_000);
            let key = format!("k{i:06}").into_bytes();
            match rng.below(10) {
                0..=1 => {
                    db.delete(&key).unwrap();
                    model.remove(&key);
                }
                2 => assert_eq!(db.get(&key).unwrap(), model.get(&key).cloned()),
                _ => {
                    let value = format!("{round}-{}", rng.next_u64()).into_bytes();
                    db.put(&key, &value).unwrap();
                    model.insert(key, value);
                }
            }
        }
        let all: Vec<_> = db.range::<Vec<u8>, _>(..).map(Result::unwrap).collect();
        assert_eq!(all.len(), model.len(), "round {round}");
        assert!(
            all.iter()
                .zip(&model)
                .all(|(a, b)| a.0 == *b.0 && a.1 == *b.1)
        );
        drop(db);
        db = Db::open(dir.path(), small_options()).unwrap();
    }
    db.compact_range(None, None).unwrap();
    for (key, value) in &model {
        assert_eq!(db.get(key).unwrap().as_ref(), Some(value));
    }
    let stats = db.stats();
    assert!(stats.compactions > 0, "{stats}");
}
