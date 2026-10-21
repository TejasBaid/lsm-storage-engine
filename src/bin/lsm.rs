//! `lsm` — command-line interface to an lsm-engine database, plus
//! inspection tools for the on-disk formats.

use std::fs::{self, File};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use clap::{Parser, Subcommand};
use lsm_engine::internal::{BatchOp, InternalIterator, LogReader, Table, parse_internal_key};
use lsm_engine::{Db, Options, WalRecoveryMode, WriteBatch, WriteOptions};

#[derive(Parser)]
#[command(name = "lsm", version, about = "LSM-tree key-value store")]
struct Cli {
    /// Database directory (required for database commands).
    #[arg(long, short, global = true, env = "LSM_DB")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Set KEY to VALUE.
    Put {
        key: String,
        value: String,
        /// fsync the WAL before returning.
        #[arg(long)]
        sync: bool,
    },
    /// Print the value of KEY.
    Get { key: String },
    /// Delete KEY.
    Delete { key: String },
    /// Print key/value pairs in [start, end).
    Scan {
        #[arg(long)]
        start: Option<String>,
        #[arg(long)]
        end: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Bulk-load COUNT generated key/value pairs.
    Load {
        #[arg(long, default_value_t = 100_000)]
        count: u64,
        #[arg(long, default_value_t = 100)]
        value_size: usize,
        /// Insert keys in pseudo-random order instead of sequentially.
        #[arg(long)]
        random: bool,
    },
    /// Flush the memtable to an SSTable.
    Flush,
    /// Compact the whole key space down to the last populated level.
    Compact,
    /// Print engine statistics and the LSM tree shape.
    Stats,
    /// Dump the contents of an SSTable file.
    DumpSst {
        file: PathBuf,
        /// Maximum number of entries to print (0 = summary only).
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Dump the batches recorded in a write-ahead log file.
    DumpWal {
        file: PathBuf,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

fn show(bytes: &[u8]) -> String {
    bytes.escape_ascii().to_string()
}

fn open_db(path: Option<PathBuf>) -> Result<Db, Box<dyn std::error::Error>> {
    let path = path.ok_or("--db <DIR> (or LSM_DB) is required for this command")?;
    Ok(Db::open(path, Options::default())?)
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Put { key, value, sync } => {
            let db = open_db(cli.db)?;
            let mut batch = WriteBatch::new();
            batch.put(key, value);
            db.write_opt(batch, &WriteOptions { sync })?;
            db.close()?;
        }
        Command::Get { key } => match open_db(cli.db)?.get(&key)? {
            Some(v) => println!("{}", show(&v)),
            None => return Err(format!("key {key:?} not found").into()),
        },
        Command::Delete { key } => {
            let db = open_db(cli.db)?;
            db.delete(&key)?;
            db.close()?;
        }
        Command::Scan { start, end, limit } => {
            let db = open_db(cli.db)?;
            let start = start.map(String::into_bytes).unwrap_or_default();
            let iter: Box<dyn Iterator<Item = _>> = match end {
                Some(end) => Box::new(db.range(start..end.into_bytes())),
                None => Box::new(db.range(start..)),
            };
            for kv in iter.take(limit) {
                let (k, v) = kv?;
                println!("{} => {}", show(&k), show(&v));
            }
        }
        Command::Load {
            count,
            value_size,
            random,
        } => {
            let db = open_db(cli.db)?;
            let started = Instant::now();
            let mut state = 0x2545_f491_4f6c_dd1du64;
            for i in 0..count {
                let n = if random {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state % count.max(1)
                } else {
                    i
                };
                let mut value = format!("value{n}").into_bytes();
                value.resize(value_size, b'x');
                db.put(format!("key{n:016}"), value)?;
            }
            let secs = started.elapsed().as_secs_f64();
            println!(
                "loaded {count} entries in {secs:.2}s ({:.0} ops/s)",
                count as f64 / secs
            );
            db.close()?;
        }
        Command::Flush => open_db(cli.db)?.flush()?,
        Command::Compact => {
            let db = open_db(cli.db)?;
            let started = Instant::now();
            db.compact_range(None, None)?;
            println!("compacted in {:.2}s", started.elapsed().as_secs_f64());
            println!("{}", db.level_summary());
        }
        Command::Stats => {
            let db = open_db(cli.db)?;
            println!("{}", db.stats());
        }
        Command::DumpSst { file, limit } => dump_sst(file, limit)?,
        Command::DumpWal { file, limit } => dump_wal(file, limit)?,
    }
    Ok(())
}

fn dump_sst(path: PathBuf, limit: usize) -> Result<(), Box<dyn std::error::Error>> {
    let size = fs::metadata(&path)?.len();
    let table = Arc::new(Table::open(File::open(&path)?, 0, size, None, true)?);
    let blocks = table.data_block_handles()?;
    println!("file:          {}", path.display());
    println!("size:          {size} bytes");
    println!("data blocks:   {}", blocks.len());
    println!("bloom filter:  {} bytes", table.filter_size());
    let mut it = table.iter(false);
    it.seek_to_first()?;
    let (mut entries, mut tombstones) = (0u64, 0u64);
    while it.valid() {
        let p = parse_internal_key(it.key()).ok_or("malformed internal key")?;
        if p.value_type == lsm_engine::internal::ValueType::Deletion {
            tombstones += 1;
        }
        if (entries as usize) < limit {
            println!(
                "  {} @ {} {:?} => {}",
                show(p.user_key),
                p.sequence,
                p.value_type,
                show(&it.value()[..it.value().len().min(64)])
            );
        }
        entries += 1;
        it.next()?;
    }
    println!("entries:       {entries} ({tombstones} tombstones)");
    Ok(())
}

fn dump_wal(path: PathBuf, limit: usize) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = LogReader::new(
        fs::read(&path)?,
        WalRecoveryMode::TolerateCorruptedTailRecords,
    );
    let mut records = 0usize;
    while let Some(record) = reader.next_record()? {
        let batch = WriteBatch::from_data(record)?;
        if records < limit {
            println!("batch seq={} ops={}", batch.sequence(), batch.count());
            for op in batch.iter().take(8) {
                match op? {
                    BatchOp::Put(k, v) => {
                        println!("  put {} => {}", show(k), show(&v[..v.len().min(64)]))
                    }
                    BatchOp::Delete(k) => println!("  del {}", show(k)),
                }
            }
        }
        records += 1;
    }
    println!(
        "{records} records, {} torn/corrupt tail bytes",
        reader.dropped_bytes()
    );
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
