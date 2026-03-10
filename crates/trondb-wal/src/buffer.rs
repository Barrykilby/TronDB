use std::collections::VecDeque;
use crate::record::{RecordType, WalRecord};

pub struct WalBuffer {
    records: VecDeque<WalRecord>,
    byte_size: usize,
    capacity: usize,
    has_commit_or_checkpoint: bool,
}

impl WalBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            records: VecDeque::new(),
            byte_size: 0,
            capacity,
            has_commit_or_checkpoint: false,
        }
    }

    pub fn append(&mut self, record: WalRecord) {
        if matches!(record.record_type, RecordType::TxCommit | RecordType::Checkpoint) {
            self.has_commit_or_checkpoint = true;
        }
        // Estimate byte size (actual MessagePack size varies, but this is close enough for capacity checks)
        self.byte_size += record.payload.len() + 64; // ~64 bytes overhead per record
        self.records.push_back(record);
    }

    /// Should the buffer be flushed? True if:
    /// - A TX_COMMIT or CHECKPOINT record was appended
    /// - Buffer byte size exceeds capacity
    pub fn should_flush(&self) -> bool {
        self.has_commit_or_checkpoint || self.byte_size >= self.capacity
    }

    pub fn drain_all(&mut self) -> Vec<WalRecord> {
        self.byte_size = 0;
        self.has_commit_or_checkpoint = false;
        self.records.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Returns the LSN of the most recently appended record (back of the queue).
    pub fn head_lsn(&self) -> Option<u64> {
        self.records.back().map(|r| r.lsn)
    }

    /// Returns the LSN of the oldest buffered record (front of the queue).
    pub fn tail_lsn(&self) -> Option<u64> {
        self.records.front().map(|r| r.lsn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordType, WalRecord};

    fn make_record(lsn: u64, record_type: RecordType) -> WalRecord {
        WalRecord {
            lsn,
            ts: 1741612800000,
            tx_id: 1,
            record_type,
            schema_ver: 1,
            collection: "test".into(),
            payload: vec![1, 2, 3],
        }
    }

    #[test]
    fn buffer_append_and_drain() {
        let mut buf = WalBuffer::new(64 * 1024 * 1024);
        buf.append(make_record(1, RecordType::EntityWrite));
        buf.append(make_record(2, RecordType::EntityWrite));

        assert_eq!(buf.len(), 2);
        assert_eq!(buf.head_lsn(), Some(2));
        assert_eq!(buf.tail_lsn(), Some(1));

        let drained = buf.drain_all();
        assert_eq!(drained.len(), 2);
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn should_flush_on_commit() {
        let mut buf = WalBuffer::new(64 * 1024 * 1024);
        buf.append(make_record(1, RecordType::EntityWrite));
        assert!(!buf.should_flush());

        buf.append(make_record(2, RecordType::TxCommit));
        assert!(buf.should_flush());
    }

    #[test]
    fn should_flush_on_checkpoint() {
        let mut buf = WalBuffer::new(64 * 1024 * 1024);
        buf.append(make_record(1, RecordType::Checkpoint));
        assert!(buf.should_flush());
    }

    #[test]
    fn should_flush_on_capacity() {
        // Tiny capacity
        let mut buf = WalBuffer::new(1);
        buf.append(make_record(1, RecordType::EntityWrite));
        assert!(buf.should_flush());
    }
}
