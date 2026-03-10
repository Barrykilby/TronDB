use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use tokio::sync::Mutex;

use crate::buffer::WalBuffer;
use crate::config::WalConfig;
use crate::error::WalError;
use crate::record::{RecordType, WalRecord};
use crate::segment::{segment_filename, segment_id_from_filename, WalSegment};

pub struct WalWriter {
    buffer: StdMutex<WalBuffer>,
    segment: Mutex<WalSegment>,
    next_lsn: AtomicU64,
    next_tx: AtomicU64,
    config: WalConfig,
}

impl WalWriter {
    pub async fn open(config: WalConfig) -> Result<Self, WalError> {
        tokio::fs::create_dir_all(&config.wal_dir).await?;

        // Find existing segments or create first one
        let (segment, head_lsn) = Self::open_or_create_segment(&config).await?;

        Ok(Self {
            buffer: StdMutex::new(WalBuffer::new(config.buffer_capacity_bytes)),
            segment: Mutex::new(segment),
            next_lsn: AtomicU64::new(head_lsn + 1),
            next_tx: AtomicU64::new(1),
            config,
        })
    }

    async fn open_or_create_segment(config: &WalConfig) -> Result<(WalSegment, u64), WalError> {
        let mut segments = Self::list_segment_ids(&config.wal_dir).await?;
        segments.sort();

        if let Some(&last_id) = segments.last() {
            let path = config.wal_dir.join(segment_filename(last_id));
            // Read existing records to find head LSN
            let records = WalSegment::read_all(&path).await?;
            let head_lsn = records.last().map(|r| r.lsn).unwrap_or(0);
            let seg = WalSegment::open_append(&path).await?;
            Ok((seg, head_lsn))
        } else {
            let path = config.wal_dir.join(segment_filename(1));
            let seg = WalSegment::create(&path, 1).await?;
            Ok((seg, 0))
        }
    }

    async fn list_segment_ids(dir: &Path) -> Result<Vec<u64>, WalError> {
        let mut ids = Vec::new();
        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(id) = segment_id_from_filename(name) {
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    }

    /// Allocate the next transaction ID.
    pub fn next_tx_id(&self) -> u64 {
        self.next_tx.fetch_add(1, Ordering::SeqCst)
    }

    /// Append a WAL record to the in-memory buffer. Returns the LSN assigned.
    /// This is sync — no I/O, just pushes to buffer using blocking_lock.
    pub fn append(
        &self,
        record_type: RecordType,
        collection: &str,
        tx_id: u64,
        schema_ver: u32,
        payload: Vec<u8>,
    ) -> u64 {
        let lsn = self.next_lsn.fetch_add(1, Ordering::SeqCst);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let record = WalRecord {
            lsn,
            ts,
            tx_id,
            record_type,
            schema_ver,
            collection: collection.to_owned(),
            payload,
        };

        // This blocks briefly on the mutex but append is fast (no I/O)
        let mut buf = self.buffer.lock().unwrap();
        buf.append(record);
        lsn
    }

    /// Commit a transaction: append TX_COMMIT, flush buffer to segment, fsync.
    /// Returns the commit LSN.
    pub async fn commit(&self, tx_id: u64) -> Result<u64, WalError> {
        let commit_lsn = self.next_lsn.fetch_add(1, Ordering::SeqCst);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let commit_record = WalRecord {
            lsn: commit_lsn,
            ts,
            tx_id,
            record_type: RecordType::TxCommit,
            schema_ver: 0,
            collection: String::new(),
            payload: vec![],
        };

        // Append commit record and drain buffer (std mutex, no await needed)
        let records = {
            let mut buf = self.buffer.lock().unwrap();
            buf.append(commit_record);
            buf.drain_all()
        };

        // Write to segment and fsync
        {
            let mut seg = self.segment.lock().await;
            for record in &records {
                seg.append(record).await?;

                // Check segment rotation
                if seg.current_size() >= self.config.segment_size_bytes as u64 {
                    seg.sync().await?;
                    let new_id = segment_id_from_filename(
                        seg.path().file_name().unwrap().to_str().unwrap(),
                    )
                    .unwrap_or(1)
                        + 1;
                    let new_path = self.config.wal_dir.join(segment_filename(new_id));
                    *seg = WalSegment::create(&new_path, new_id).await?;
                }
            }
            seg.sync().await?;
        }

        Ok(commit_lsn)
    }

    /// Write a checkpoint record. Returns the checkpoint LSN.
    pub async fn checkpoint(&self, snapshot_path: &Path) -> Result<u64, WalError> {
        let lsn = self.next_lsn.fetch_add(1, Ordering::SeqCst);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let payload = rmp_serde::to_vec_named(&serde_json::json!({
            "lsn": lsn,
            "snapshot_path": snapshot_path.to_string_lossy(),
        }))
        .map_err(|e| WalError::Serialisation(e.to_string()))?;

        let record = WalRecord {
            lsn,
            ts,
            tx_id: 0,
            record_type: RecordType::Checkpoint,
            schema_ver: 0,
            collection: String::new(),
            payload,
        };

        let mut seg = self.segment.lock().await;
        seg.append(&record).await?;
        seg.sync().await?;

        Ok(lsn)
    }

    /// Get the current head LSN (last assigned).
    pub fn head_lsn(&self) -> u64 {
        self.next_lsn.load(Ordering::SeqCst).saturating_sub(1)
    }

    /// Get the WAL directory path.
    pub fn wal_dir(&self) -> &Path {
        &self.config.wal_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WalConfig;
    use crate::record::RecordType;
    use tempfile::TempDir;

    #[tokio::test]
    async fn writer_open_creates_wal_dir_and_first_segment() {
        let dir = TempDir::new().unwrap();
        let config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };

        let _writer = WalWriter::open(config.clone()).await.unwrap();

        assert!(config.wal_dir.exists());
        // Should have one segment file
        let entries: Vec<_> = std::fs::read_dir(&config.wal_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn writer_append_and_commit() {
        let dir = TempDir::new().unwrap();
        let config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };

        let writer = WalWriter::open(config.clone()).await.unwrap();

        let tx_id = writer.next_tx_id();
        writer.append(RecordType::TxBegin, "venues", tx_id, 1, vec![]);
        writer.append(
            RecordType::SchemaCreateColl,
            "venues",
            tx_id,
            1,
            rmp_serde::to_vec_named(&serde_json::json!({"name": "venues", "dimensions": 3}))
                .unwrap(),
        );
        let commit_lsn = writer.commit(tx_id).await.unwrap();

        assert!(commit_lsn > 0);
    }

    #[tokio::test]
    async fn writer_records_survive_reopen() {
        let dir = TempDir::new().unwrap();
        let config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };

        // Write some records
        {
            let writer = WalWriter::open(config.clone()).await.unwrap();
            let tx_id = writer.next_tx_id();
            writer.append(RecordType::TxBegin, "test", tx_id, 1, vec![]);
            writer.append(RecordType::EntityWrite, "test", tx_id, 1, vec![1, 2, 3]);
            writer.commit(tx_id).await.unwrap();
        }

        // Read them back from segment files
        let entries: Vec<_> = std::fs::read_dir(&config.wal_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);

        let records = crate::segment::WalSegment::read_all(&entries[0].path())
            .await
            .unwrap();
        // TX_BEGIN + EntityWrite + TX_COMMIT = 3 records
        assert_eq!(records.len(), 3);
        assert_eq!(records[2].record_type, RecordType::TxCommit);
    }
}
