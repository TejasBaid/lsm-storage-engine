//! db_bench-style macro benchmark comparing lsm-engine with sled.
//!
//! ```text
//! cargo bench --bench db_bench -- [--num=1000000] [--value-size=100]
//!     [--engines=lsm,sled] [--benchmarks=fillseq,fillrandom,overwrite,readrandom,readmissing,seekrandom,readseq]
//!     [--scan-length=100] [--dir=PATH]
//! ```
//!
//! Each `fill*` benchmark starts from an empty database. `fillrandom`
//! inserts every key exactly once in a shuffled order; the read benchmarks
//! and `overwrite` then run against that database. Writes are not fsynced
//! individually by either engine (lsm-engine: `sync = false`; sled: default
//! periodic flush), so both measure the in-process write path.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lsm_engine::{Db, DbStats, Options};

struct Config {
    num: u64,
    value_size: usize,
    engines: Vec<String>,
    benchmarks: Vec<String>,
    scan_length: usize,
    dir: Option<PathBuf>,
    cache_bytes: usize,
}

impl Config {
    fn parse() -> Self {
        let mut c = Config {
            num: 1_000_000,
            value_size: 100,
            engines: vec!["lsm".into(), "sled".into()],
            benchmarks: "fillseq,fillrandom,overwrite,readrandom,readmissing,seekrandom,readseq"
                .split(',')
                .map(String::from)
                .collect(),
            scan_length: 100,
            dir: None,
            cache_bytes: 64 << 20,
        };
        for arg in std::env::args().skip(1) {
            let Some((flag, value)) = arg.strip_prefix("--").and_then(|a| a.split_once('=')) else {
                continue; // e.g. the `--bench` flag cargo passes
            };
            let list = || value.split(',').map(String::from).collect();
            match flag {
                "num" => c.num = value.parse().expect("--num"),
                "value-size" => c.value_size = value.parse().expect("--value-size"),
                "engines" => c.engines = list(),
                "benchmarks" => c.benchmarks = list(),
                "scan-length" => c.scan_length = value.parse().expect("--scan-length"),
                "dir" => c.dir = Some(PathBuf::from(value)),
                "cache-mb" => c.cache_bytes = value.parse::<usize>().expect("--cache-mb") << 20,
                other => panic!("unknown flag --{other}"),
            }
        }
        c
    }
}

/// SplitMix64: fast, deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn key(i: u64) -> [u8; 16] {
    let mut k = [0u8; 16];
    write!(&mut ArrayWriter(&mut k, 0), "{i:016}").unwrap();
    k
}

struct ArrayWriter<'a>(&'a mut [u8; 16], usize);

impl std::fmt::Write for ArrayWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let b = s.as_bytes();
        self.0[self.1..self.1 + b.len()].copy_from_slice(b);
        self.1 += b.len();
        Ok(())
    }
}

fn shuffled(n: u64, seed: u64) -> Vec<u64> {
    let mut v: Vec<u64> = (0..n).collect();
    let mut rng = Rng(seed);
    for i in (1..v.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
    v
}

trait Engine {
    fn name(&self) -> &'static str;
    fn put(&self, k: &[u8], v: &[u8]);
    fn get(&self, k: &[u8]) -> Option<Vec<u8>>;
    /// Reads up to `n` entries starting at `k`; returns how many were read.
    fn scan(&self, k: &[u8], n: usize) -> usize;
    fn scan_all(&self) -> usize;
    /// Waits for background work so the next phase starts quiescent.
    fn settle(&self);
    fn stats(&self) -> Option<DbStats> {
        None
    }
}

struct Lsm(Db);

impl Engine for Lsm {
    fn name(&self) -> &'static str {
        "lsm-engine"
    }
    fn put(&self, k: &[u8], v: &[u8]) {
        self.0.put(k, v).unwrap();
    }
    fn get(&self, k: &[u8]) -> Option<Vec<u8>> {
        self.0.get(k).unwrap()
    }
    fn scan(&self, k: &[u8], n: usize) -> usize {
        let mut it = self.0.iter().unwrap();
        it.seek(k).unwrap();
        let mut read = 0;
        while it.valid() && read < n {
            std::hint::black_box((it.key(), it.value()));
            read += 1;
            it.next().unwrap();
        }
        read
    }
    fn scan_all(&self) -> usize {
        let mut it = self.0.iter().unwrap();
        it.seek_to_first().unwrap();
        let mut read = 0;
        while it.valid() {
            std::hint::black_box((it.key(), it.value()));
            read += 1;
            it.next().unwrap();
        }
        read
    }
    fn settle(&self) {
        self.0.wait_for_compactions().unwrap();
    }
    fn stats(&self) -> Option<DbStats> {
        Some(self.0.stats())
    }
}

struct Sled(sled::Db);

impl Engine for Sled {
    fn name(&self) -> &'static str {
        "sled 0.34"
    }
    fn put(&self, k: &[u8], v: &[u8]) {
        self.0.insert(k, v).unwrap();
    }
    fn get(&self, k: &[u8]) -> Option<Vec<u8>> {
        self.0.get(k).unwrap().map(|v| v.to_vec())
    }
    fn scan(&self, k: &[u8], n: usize) -> usize {
        let mut read = 0;
        for kv in self.0.range(k..).take(n) {
            std::hint::black_box(kv.unwrap());
            read += 1;
        }
        read
    }
    fn scan_all(&self) -> usize {
        let mut read = 0;
        for kv in self.0.iter() {
            std::hint::black_box(kv.unwrap());
            read += 1;
        }
        read
    }
    fn settle(&self) {
        self.0.flush().unwrap();
    }
}

fn open(engine: &str, path: &Path, cfg: &Config) -> Box<dyn Engine> {
    match engine {
        "lsm" => {
            let opts = Options {
                block_cache_capacity: cfg.cache_bytes,
                ..Options::default()
            };
            Box::new(Lsm(Db::open(path, opts).unwrap()))
        }
        "sled" => Box::new(Sled(
            sled::Config::new()
                .path(path)
                .cache_capacity(cfg.cache_bytes as u64)
                .open()
                .unwrap(),
        )),
        other => panic!("unknown engine {other}"),
    }
}

struct Report {
    ops: u64,
    elapsed: Duration,
    latencies_ns: Vec<u32>,
    bytes: u64,
    note: String,
}

impl Report {
    fn percentile(&self, p: f64) -> f64 {
        if self.latencies_ns.is_empty() {
            return 0.0;
        }
        let idx = ((self.latencies_ns.len() - 1) as f64 * p).round() as usize;
        self.latencies_ns[idx] as f64 / 1000.0
    }
}

/// Times `ops` calls of `f`, recording each call's latency.
fn timed(ops: u64, bytes_per_op: u64, mut f: impl FnMut(u64)) -> Report {
    let mut latencies_ns = Vec::with_capacity(ops as usize);
    let start = Instant::now();
    for i in 0..ops {
        let t = Instant::now();
        f(i);
        latencies_ns.push(t.elapsed().as_nanos().min(u32::MAX as u128) as u32);
    }
    let elapsed = start.elapsed();
    latencies_ns.sort_unstable();
    Report {
        ops,
        elapsed,
        latencies_ns,
        bytes: ops * bytes_per_op,
        note: String::new(),
    }
}

fn value_for(i: u64, size: usize, buf: &mut Vec<u8>) {
    buf.clear();
    let mut rng = Rng(i);
    while buf.len() < size {
        buf.extend_from_slice(&rng.next().to_le_bytes());
    }
    buf.truncate(size);
}

fn main() {
    let cfg = Config::parse();
    let base = match &cfg.dir {
        Some(d) => {
            std::fs::create_dir_all(d).unwrap();
            tempfile::tempdir_in(d).unwrap()
        }
        None => tempfile::tempdir().unwrap(),
    };
    println!(
        "db_bench: {} keys, 16-byte keys, {}-byte values, cache {} MiB, {} CPUs",
        cfg.num,
        cfg.value_size,
        cfg.cache_bytes >> 20,
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    println!();
    println!(
        "| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|---|");

    let entry_bytes = 16 + cfg.value_size as u64;
    for engine in &cfg.engines {
        let order = shuffled(cfg.num, 42);
        let mut random_db: Option<Box<dyn Engine>> = None;
        for bench in &cfg.benchmarks {
            let mut buf = Vec::with_capacity(cfg.value_size);
            let report = match bench.as_str() {
                "fillseq" | "fillrandom" => {
                    let path = base.path().join(format!("{engine}-{bench}"));
                    let db = open(engine, &path, &cfg);
                    let mut r = timed(cfg.num, entry_bytes, |i| {
                        let k = if bench == "fillseq" {
                            i
                        } else {
                            order[i as usize]
                        };
                        value_for(k, cfg.value_size, &mut buf);
                        db.put(&key(k), &buf);
                    });
                    let settle = Instant::now();
                    db.settle();
                    r.note = format!("settle {:.2}s", settle.elapsed().as_secs_f64());
                    if let Some(s) = db.stats() {
                        let _ = write!(
                            r.note,
                            "; WA {:.2}x; {}",
                            s.write_amplification(),
                            tree_shape(&s)
                        );
                    }
                    if bench == "fillrandom" {
                        random_db = Some(db);
                    }
                    r
                }
                other => {
                    let Some(db) = random_db.as_ref() else {
                        eprintln!("skipping {other}: run fillrandom first");
                        continue;
                    };
                    match other {
                        "overwrite" => {
                            let mut rng = Rng(7);
                            let before = db.stats();
                            let mut r = timed(cfg.num, entry_bytes, |_| {
                                let k = rng.next() % cfg.num;
                                value_for(k ^ 0xffff, cfg.value_size, &mut buf);
                                db.put(&key(k), &buf);
                            });
                            let settle = Instant::now();
                            db.settle();
                            r.note = format!("settle {:.2}s", settle.elapsed().as_secs_f64());
                            if let (Some(s), Some(b)) = (db.stats(), before) {
                                // Write amplification of this phase alone.
                                let sst = (s.flush_bytes_written + s.compaction_bytes_written)
                                    - (b.flush_bytes_written + b.compaction_bytes_written);
                                let user = s.user_bytes_written - b.user_bytes_written;
                                let _ = write!(
                                    r.note,
                                    "; WA {:.2}x; {}",
                                    sst as f64 / user as f64,
                                    tree_shape(&s)
                                );
                            }
                            r
                        }
                        "readrandom" => {
                            let mut rng = Rng(11);
                            let mut found = 0u64;
                            let mut r = timed(cfg.num, entry_bytes, |_| {
                                if db.get(&key(rng.next() % cfg.num)).is_some() {
                                    found += 1;
                                }
                            });
                            r.note = format!("{found} of {} found", cfg.num);
                            r
                        }
                        "readmissing" => {
                            let mut rng = Rng(13);
                            let mut found = 0u64;
                            let mut r = timed(cfg.num, 16, |_| {
                                // Keys beyond the populated range, interleaved with it.
                                let k = cfg.num + rng.next() % cfg.num;
                                if db.get(&key(k)).is_some() {
                                    found += 1;
                                }
                            });
                            r.note = format!("{found} found (expect 0)");
                            r
                        }
                        "seekrandom" => {
                            let mut rng = Rng(17);
                            let scans = (cfg.num / 100).max(1);
                            let mut read = 0u64;
                            let mut r = timed(scans, entry_bytes * cfg.scan_length as u64, |_| {
                                read += db.scan(&key(rng.next() % cfg.num), cfg.scan_length) as u64;
                            });
                            r.note =
                                format!("seek + {} nexts per op ({read} entries)", cfg.scan_length);
                            r
                        }
                        "readseq" => {
                            let start = Instant::now();
                            let n = db.scan_all() as u64;
                            Report {
                                ops: n,
                                elapsed: start.elapsed(),
                                latencies_ns: Vec::new(),
                                bytes: n * entry_bytes,
                                note: "full scan".into(),
                            }
                        }
                        unknown => panic!("unknown benchmark {unknown}"),
                    }
                }
            };
            print_row(engine_name(engine), bench, &report);
        }
        if let Some(db) = random_db.take()
            && let Some(s) = db.stats()
        {
            eprintln!("\n{} stats after all benchmarks:\n{s}\n", db.name());
        }
    }
}

fn engine_name(e: &str) -> &'static str {
    match e {
        "lsm" => "lsm-engine",
        "sled" => "sled 0.34",
        _ => "?",
    }
}

fn tree_shape(s: &DbStats) -> String {
    s.levels
        .iter()
        .filter(|l| l.files > 0)
        .map(|l| format!("L{}={}", l.level, l.files))
        .collect::<Vec<_>>()
        .join(" ")
}

fn print_row(engine: &str, bench: &str, r: &Report) {
    let secs = r.elapsed.as_secs_f64();
    let has_lat = !r.latencies_ns.is_empty();
    let lat = |p: f64| {
        if has_lat {
            format!("{:.2}", r.percentile(p))
        } else {
            "–".into()
        }
    };
    println!(
        "| {engine} | {bench} | {:.0} | {:.1} | {:.2} | {} | {} | {} | {} |",
        r.ops as f64 / secs,
        r.bytes as f64 / secs / 1e6,
        secs * 1e6 / r.ops as f64,
        lat(0.50),
        lat(0.99),
        lat(0.999),
        r.note
    );
}
