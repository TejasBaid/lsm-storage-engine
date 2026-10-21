//! Every on-disk decoder must reject arbitrary bytes with an error, never
//! panic, loop forever, or allocate unboundedly.
#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use lsm_engine::internal::{
    Block, Footer, InternalIterator, LogReader, VersionEdit, compare_internal_keys,
};
use lsm_engine::{WalRecoveryMode, WriteBatch};

fuzz_target!(|data: &[u8]| {
    if let Ok(block) = Block::new(data.to_vec()) {
        let block = Arc::new(block);
        let mut it = block.iter(compare_internal_keys);
        if it.seek_to_first().is_ok() {
            while it.valid() {
                let _ = (it.key(), it.value());
                if it.next().is_err() {
                    break;
                }
            }
        }
        let _ = it.seek(data);
    }

    for mode in [
        WalRecoveryMode::TolerateCorruptedTailRecords,
        WalRecoveryMode::AbsoluteConsistency,
        WalRecoveryMode::PointInTimeRecovery,
    ] {
        let mut reader = LogReader::new(data.to_vec(), mode);
        while let Ok(Some(record)) = reader.next_record() {
            let _ = WriteBatch::from_data(record);
        }
    }

    let _ = VersionEdit::decode(data);
    let _ = WriteBatch::from_data(data);
    if data.len() >= 48 {
        let _ = Footer::decode(&data[..48]);
    }
});
