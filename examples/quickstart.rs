//! The README quick-start, compiled and run by `cargo run --example quickstart`.

use lsm_engine::{Db, Options, ReadOptions, WriteBatch};

fn main() -> lsm_engine::Result<()> {
    let dir = std::env::temp_dir().join(format!("lsm-quickstart-{}", std::process::id()));
    let db = Db::open(&dir, Options::default())?;

    db.put(b"user:1", b"alice")?;
    db.put(b"user:2", b"bob")?;
    assert_eq!(db.get(b"user:1")?, Some(b"alice".to_vec()));

    // Atomic: after a crash either both operations are present or neither is.
    let mut batch = WriteBatch::new();
    batch.delete(b"user:1");
    batch.put(b"user:3", b"carol");
    db.write(batch)?;

    // Snapshot isolation: the snapshot keeps seeing the old state.
    let snap = db.snapshot();
    db.put(b"user:2", b"robert")?;
    assert_eq!(
        db.get_opt(b"user:2", &ReadOptions::at_snapshot(&snap))?,
        Some(b"bob".to_vec())
    );

    // Ordered range scan, merged across memtables and every level.
    for kv in db.range(b"user:".to_vec()..b"user;".to_vec()) {
        let (key, value) = kv?;
        println!(
            "{} = {}",
            String::from_utf8_lossy(&key),
            String::from_utf8_lossy(&value)
        );
    }

    drop(snap);
    db.close()?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
