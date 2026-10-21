//! Criterion micro-benchmarks of individual components.
//!
//! `cargo bench --bench micro`

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use lsm_engine::internal::{
    Block, BlockBuilder, BloomFilterPolicy, InternalIterator, LogWriter, MemTable, Table,
    TableBuilder, ValueType, bloom_hash, compare_internal_keys, make_internal_key,
};
use lsm_engine::{Db, Options};

fn ikey(i: u64) -> Vec<u8> {
    make_internal_key(format!("key{i:016}").as_bytes(), i + 1, ValueType::Value)
}

/// Multiplicative hash for scattering benchmark probes.
fn scatter(i: u64, n: u64) -> u64 {
    i.wrapping_mul(0x9E37_79B9_7F4A_7C15) % n
}

fn memtable(c: &mut Criterion) {
    let mut g = c.benchmark_group("memtable");
    g.throughput(Throughput::Elements(1));
    let value = [0u8; 100];
    g.bench_function("insert", |b| {
        let m = MemTable::new();
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            m.add(
                i,
                ValueType::Value,
                format!("key{:016}", scatter(i, 1 << 40)).as_bytes(),
                &value,
            );
        });
    });
    let m = MemTable::new();
    let n = 100_000u64;
    for i in 0..n {
        m.add(
            i + 1,
            ValueType::Value,
            format!("key{i:016}").as_bytes(),
            &value,
        );
    }
    let mut i = 0u64;
    g.bench_function("get_hit_100k", |b| {
        b.iter(|| {
            i += 1;
            black_box(m.get(
                format!("key{:016}", scatter(i, n)).as_bytes(),
                u64::MAX >> 8,
            ));
        });
    });
    g.finish();
}

fn block(c: &mut Criterion) {
    let mut builder = BlockBuilder::new(16);
    let n = 40u64;
    for i in 0..n {
        builder.add(&ikey(i), &[7u8; 80]);
    }
    let block = Arc::new(Block::new(builder.finish()).unwrap());
    let probes: Vec<Vec<u8>> = (0..n).map(ikey).collect();
    let mut g = c.benchmark_group("block");
    g.throughput(Throughput::Elements(1));
    let mut i = 0u64;
    g.bench_function("seek_4k_block", |b| {
        let mut it = block.iter(compare_internal_keys);
        b.iter(|| {
            i += 1;
            it.seek(&probes[scatter(i, n) as usize]).unwrap();
            black_box(it.value());
        });
    });
    g.bench_function("build_4k_block", |b| {
        b.iter_batched(
            || BlockBuilder::new(16),
            |mut builder| {
                for p in &probes {
                    builder.add(p, &[7u8; 80]);
                }
                black_box(builder.finish())
            },
            BatchSize::SmallInput,
        );
    });
    g.finish();
}

fn bloom(c: &mut Criterion) {
    let policy = BloomFilterPolicy::new(10);
    let keys: Vec<Vec<u8>> = (0..10_000u64)
        .map(|i| format!("key{i:016}").into_bytes())
        .collect();
    let hashes: Vec<u32> = keys.iter().map(|k| bloom_hash(k)).collect();
    let filter = policy.create_filter(&hashes);
    let mut g = c.benchmark_group("bloom");
    g.throughput(Throughput::Elements(1));
    let mut i = 0u64;
    g.bench_function("may_match", |b| {
        b.iter(|| {
            i += 1;
            black_box(BloomFilterPolicy::key_may_match(
                &keys[scatter(i, 10_000) as usize],
                &filter,
            ));
        });
    });
    g.finish();
}

fn wal(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let mut w = LogWriter::new(std::fs::File::create(dir.path().join("bench.log")).unwrap());
    let record = [0x5au8; 128];
    let mut g = c.benchmark_group("wal");
    g.throughput(Throughput::Bytes(record.len() as u64));
    g.bench_function("append_128b_no_sync", |b| {
        b.iter(|| w.add_record(&record).unwrap())
    });
    g.finish();
}

fn table(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("000001.sst");
    let opts = Options::default();
    let mut builder = TableBuilder::new(std::fs::File::create(&path).unwrap(), &opts);
    let n = 20_000u64;
    for i in 0..n {
        builder.add(&ikey(i), &[1u8; 100]).unwrap();
    }
    let size = builder.finish().unwrap();
    let cache = Arc::new(lsm_engine::internal::LruCache::new(64 << 20, 16));
    let t = Table::open(
        std::fs::File::open(&path).unwrap(),
        1,
        size,
        Some(cache),
        true,
    )
    .unwrap();
    let probes: Vec<Vec<u8>> = (0..n).map(ikey).collect();
    let mut g = c.benchmark_group("table");
    g.throughput(Throughput::Elements(1));
    let mut i = 0u64;
    g.bench_function("get_cached_20k", |b| {
        b.iter(|| {
            i += 1;
            black_box(t.get(&probes[scatter(i, n) as usize], true).unwrap());
        });
    });
    g.finish();
}

fn db(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), Options::default()).unwrap();
    let n = 10_000u64;
    for i in 0..n {
        db.put(format!("key{i:016}"), [3u8; 100]).unwrap();
    }
    let mut g = c.benchmark_group("db");
    g.throughput(Throughput::Elements(1));
    let mut i = 0u64;
    g.bench_function("get_memtable_hit", |b| {
        b.iter(|| {
            i += 1;
            black_box(db.get(format!("key{:016}", scatter(i, n))).unwrap());
        });
    });
    let mut j = 0u64;
    g.bench_function("put_100b", |b| {
        b.iter(|| {
            j += 1;
            db.put(format!("key{:016}", scatter(j, 1 << 30)), [4u8; 100])
                .unwrap();
        });
    });
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(30)
        .warm_up_time(std::time::Duration::from_millis(500))
        .measurement_time(std::time::Duration::from_secs(2));
    targets = memtable, block, bloom, wal, table, db
}
criterion_main!(benches);
