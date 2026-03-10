use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::error::WalError;
use crate::record::{RecordType, WalRecord};
use crate::segment::{segment_filename, segment_id_from_filename, WalSegment};

pub struct RecoveryResult {
    /// Committed records in LSN order, excluding TX_BEGIN/TX_COMMIT/TX_ABORT control records.
    pub records: Vec<WalRecord>,
    /// The highest LSN seen across all segments.
    pub head_lsn: u64,
    /// The checkpoint LSN if one was found, or 0.
    pub checkpoint_lsn: u64,
}

pub struct WalRecovery;

impl WalRecovery {
    /// Recover committed records from WAL segments.
    ///
    /// Implements ARIES-style recovery:
    /// 1. Find the last CHECKPOINT record (if any)
    /// 2. Replay all records from checkpoint LSN forward
    /// 3. Group by tx_id, discard uncommitted transactions
    /// 4. Return committed mutation records in LSN order
    pub async fn recover(wal_dir: &Path) -> Result<RecoveryResult, WalError> {
        if !wal_dir.exists() {
            return Ok(RecoveryResult {
                records: Vec::new(),
                head_lsn: 0,
                checkpoint_lsn: 0,
            });
        }

        // Collect and sort segment files
        let mut segment_ids = Vec::new();
        let mut entries = tokio::fs::read_dir(wal_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(id) = segment_id_from_filename(name) {
                    segment_ids.push(id);
                }
            }
        }
        segment_ids.sort();

        if segment_ids.is_empty() {
            return Ok(RecoveryResult {
                records: Vec::new(),
                head_lsn: 0,
                checkpoint_lsn: 0,
            });
        }

        // Read all records from all segments in order
        let mut all_records = Vec::new();
        for seg_id in &segment_ids {
            let path = wal_dir.join(segment_filename(*seg_id));
            let records = WalSegment::read_all(&path).await?;
            all_records.extend(records);
        }

        if all_records.is_empty() {
            return Ok(RecoveryResult {
                records: Vec::new(),
                head_lsn: 0,
                checkpoint_lsn: 0,
            });
        }

        let head_lsn = all_records.iter().map(|r| r.lsn).max().unwrap_or(0);

        // Find last checkpoint LSN
        let checkpoint_lsn = all_records
            .iter()
            .rev()
            .find(|r| r.record_type == RecordType::Checkpoint)
            .map(|r| r.lsn)
            .unwrap_or(0);

        // Replay records after checkpoint (or all if no checkpoint)
        let replay_records: Vec<_> = if checkpoint_lsn == 0 {
            all_records
        } else {
            all_records
                .into_iter()
                .filter(|r| r.lsn > checkpoint_lsn)
                .collect()
        };

        // Group mutation records by tx_id; track committed tx_ids
        let mut tx_records: HashMap<u64, Vec<WalRecord>> = HashMap::new();
        let mut committed_txs: HashSet<u64> = HashSet::new();

        for record in replay_records {
            match record.record_type {
                RecordType::TxCommit => {
                    committed_txs.insert(record.tx_id);
                }
                RecordType::TxAbort => {
                    tx_records.remove(&record.tx_id);
                }
                RecordType::TxBegin | RecordType::Checkpoint => {
                    // Control records — ensure an entry exists but add no mutation
                    tx_records.entry(record.tx_id).or_default();
                }
                _ => {
                    tx_records
                        .entry(record.tx_id)
                        .or_default()
                        .push(record);
                }
            }
        }

        // Collect committed mutation records and sort by LSN
        let mut committed_records: Vec<WalRecord> = tx_records
            .into_iter()
            .filter(|(tx_id, _)| committed_txs.contains(tx_id))
            .flat_map(|(_, records)| records)
            .collect();

        committed_records.sort_by_key(|r| r.lsn);

        Ok(RecoveryResult {
            records: committed_records,
            head_lsn,
            checkpoint_lsn,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WalConfig;
    use crate::record::RecordType;
    use crate::writer::WalWriter;
    use tempfile::TempDir;

    #[tokio::test]
    async fn recover_committed_transactions() {
        let dir = TempDir::new().unwrap();
        let config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };

        // Write committed records
        {
            let writer = WalWriter::open(config.clone()).await.unwrap();
            let tx_id = writer.next_tx_id();
            writer.append(RecordType::TxBegin, "venues", tx_id, 1, vec![]);
            writer.append(
                RecordType::EntityWrite,
                "venues",
                tx_id,
                1,
                b"entity1".to_vec(),
            );
            writer.commit(tx_id).await.unwrap();
        }

        let committed = WalRecovery::recover(&config.wal_dir).await.unwrap();
        // Should contain the EntityWrite (not TxBegin/TxCommit which are control records)
        let entity_writes: Vec<_> = committed
            .records
            .iter()
            .filter(|r| r.record_type == RecordType::EntityWrite)
            .collect();
        assert_eq!(entity_writes.len(), 1);
        assert_eq!(committed.head_lsn, 3); // TxBegin=1, EntityWrite=2, TxCommit=3
    }

    #[tokio::test]
    async fn recover_discards_uncommitted() {
        use crate::segment::{segment_filename, WalSegment};

        let dir = TempDir::new().unwrap();
        let config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };
        tokio::fs::create_dir_all(&config.wal_dir).await.unwrap();

        // Write directly to segment to simulate crash with uncommitted tx on disk
        let seg_path = config.wal_dir.join(segment_filename(1));
        let mut seg = WalSegment::create(&seg_path, 1).await.unwrap();

        let make_record = |lsn: u64, tx_id: u64, record_type: RecordType, payload: Vec<u8>| {
            WalRecord {
                lsn,
                ts: 0,
                tx_id,
                record_type,
                schema_ver: 1,
                collection: "venues".into(),
                payload,
            }
        };

        // Tx1: committed
        seg.append(&make_record(1, 1, RecordType::TxBegin, vec![]))
            .await
            .unwrap();
        seg.append(&make_record(2, 1, RecordType::EntityWrite, b"committed".to_vec()))
            .await
            .unwrap();
        seg.append(&make_record(3, 1, RecordType::TxCommit, vec![]))
            .await
            .unwrap();

        // Tx2: on disk but never committed (simulates crash mid-transaction)
        seg.append(&make_record(4, 2, RecordType::TxBegin, vec![]))
            .await
            .unwrap();
        seg.append(&make_record(5, 2, RecordType::EntityWrite, b"uncommitted".to_vec()))
            .await
            .unwrap();
        // No TxCommit — crash happened here

        seg.sync().await.unwrap();

        let committed = WalRecovery::recover(&config.wal_dir).await.unwrap();
        let entity_writes: Vec<_> = committed
            .records
            .iter()
            .filter(|r| r.record_type == RecordType::EntityWrite)
            .collect();
        // Only the committed entity should survive
        assert_eq!(entity_writes.len(), 1);
        assert_eq!(entity_writes[0].payload, b"committed");
    }

    #[tokio::test]
    async fn recover_empty_wal() {
        let dir = TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        tokio::fs::create_dir_all(&wal_dir).await.unwrap();

        let result = WalRecovery::recover(&wal_dir).await.unwrap();
        assert!(result.records.is_empty());
        assert_eq!(result.head_lsn, 0);
    }

    #[tokio::test]
    async fn recover_with_checkpoint_discards_pre_checkpoint_records() {
        let dir = TempDir::new().unwrap();
        let config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };

        {
            let writer = WalWriter::open(config.clone()).await.unwrap();

            // Tx1: committed before checkpoint
            let tx1 = writer.next_tx_id();
            writer.append(RecordType::TxBegin, "venues", tx1, 1, vec![]);
            writer.append(
                RecordType::EntityWrite,
                "venues",
                tx1,
                1,
                b"pre_checkpoint".to_vec(),
            );
            writer.commit(tx1).await.unwrap();

            // Checkpoint
            writer
                .checkpoint(std::path::Path::new("/tmp/snapshot"))
                .await
                .unwrap();

            // Tx2: committed after checkpoint
            let tx2 = writer.next_tx_id();
            writer.append(RecordType::TxBegin, "venues", tx2, 1, vec![]);
            writer.append(
                RecordType::EntityWrite,
                "venues",
                tx2,
                1,
                b"post_checkpoint".to_vec(),
            );
            writer.commit(tx2).await.unwrap();
        }

        let result = WalRecovery::recover(&config.wal_dir).await.unwrap();
        // Only records after checkpoint should be replayed
        let entity_writes: Vec<_> = result
            .records
            .iter()
            .filter(|r| r.record_type == RecordType::EntityWrite)
            .collect();
        assert_eq!(entity_writes.len(), 1);
        assert_eq!(entity_writes[0].payload, b"post_checkpoint");
        assert!(result.checkpoint_lsn > 0);
    }

    #[tokio::test]
    async fn recover_aborted_transaction_discarded() {
        use crate::segment::{segment_filename, WalSegment};

        let dir = TempDir::new().unwrap();
        let config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };
        tokio::fs::create_dir_all(&config.wal_dir).await.unwrap();

        // Write records directly to segment to simulate a TX_ABORT scenario
        let seg_path = config.wal_dir.join(segment_filename(1));
        let mut seg = WalSegment::create(&seg_path, 1).await.unwrap();

        let make_record = |lsn: u64, tx_id: u64, record_type: RecordType, payload: Vec<u8>| {
            WalRecord {
                lsn,
                ts: 0,
                tx_id,
                record_type,
                schema_ver: 1,
                collection: "venues".into(),
                payload,
            }
        };

        // Tx1: committed
        seg.append(&make_record(1, 10, RecordType::TxBegin, vec![]))
            .await
            .unwrap();
        seg.append(&make_record(
            2,
            10,
            RecordType::EntityWrite,
            b"good".to_vec(),
        ))
        .await
        .unwrap();
        seg.append(&make_record(3, 10, RecordType::TxCommit, vec![]))
            .await
            .unwrap();

        // Tx2: aborted
        seg.append(&make_record(4, 20, RecordType::TxBegin, vec![]))
            .await
            .unwrap();
        seg.append(&make_record(
            5,
            20,
            RecordType::EntityWrite,
            b"aborted".to_vec(),
        ))
        .await
        .unwrap();
        seg.append(&make_record(6, 20, RecordType::TxAbort, vec![]))
            .await
            .unwrap();

        seg.sync().await.unwrap();

        let result = WalRecovery::recover(&config.wal_dir).await.unwrap();
        let entity_writes: Vec<_> = result
            .records
            .iter()
            .filter(|r| r.record_type == RecordType::EntityWrite)
            .collect();
        assert_eq!(entity_writes.len(), 1);
        assert_eq!(entity_writes[0].payload, b"good");
        assert_eq!(result.head_lsn, 6);
    }

    #[tokio::test]
    async fn recover_multiple_segments() {
        use crate::segment::{segment_filename, WalSegment};

        let dir = TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        tokio::fs::create_dir_all(&wal_dir).await.unwrap();

        let make_record = |lsn: u64, tx_id: u64, record_type: RecordType, payload: Vec<u8>| {
            WalRecord {
                lsn,
                ts: 0,
                tx_id,
                record_type,
                schema_ver: 1,
                collection: "test".into(),
                payload,
            }
        };

        // Segment 1: tx1 committed
        {
            let mut seg = WalSegment::create(&wal_dir.join(segment_filename(1)), 1)
                .await
                .unwrap();
            seg.append(&make_record(1, 1, RecordType::TxBegin, vec![]))
                .await
                .unwrap();
            seg.append(&make_record(
                2,
                1,
                RecordType::EntityWrite,
                b"seg1".to_vec(),
            ))
            .await
            .unwrap();
            seg.append(&make_record(3, 1, RecordType::TxCommit, vec![]))
                .await
                .unwrap();
            seg.sync().await.unwrap();
        }

        // Segment 2: tx2 committed
        {
            let mut seg = WalSegment::create(&wal_dir.join(segment_filename(2)), 2)
                .await
                .unwrap();
            seg.append(&make_record(4, 2, RecordType::TxBegin, vec![]))
                .await
                .unwrap();
            seg.append(&make_record(
                5,
                2,
                RecordType::EntityWrite,
                b"seg2".to_vec(),
            ))
            .await
            .unwrap();
            seg.append(&make_record(6, 2, RecordType::TxCommit, vec![]))
                .await
                .unwrap();
            seg.sync().await.unwrap();
        }

        let result = WalRecovery::recover(&wal_dir).await.unwrap();
        let entity_writes: Vec<_> = result
            .records
            .iter()
            .filter(|r| r.record_type == RecordType::EntityWrite)
            .collect();
        assert_eq!(entity_writes.len(), 2);
        assert_eq!(entity_writes[0].payload, b"seg1");
        assert_eq!(entity_writes[1].payload, b"seg2");
        assert_eq!(result.head_lsn, 6);
    }

    #[tokio::test]
    async fn recover_nonexistent_dir_returns_empty() {
        let dir = TempDir::new().unwrap();
        let wal_dir = dir.path().join("does_not_exist");

        let result = WalRecovery::recover(&wal_dir).await.unwrap();
        assert!(result.records.is_empty());
        assert_eq!(result.head_lsn, 0);
        assert_eq!(result.checkpoint_lsn, 0);
    }
}
